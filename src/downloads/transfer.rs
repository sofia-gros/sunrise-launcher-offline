//! HTTP requests, streaming responses and retries around the shared downloader.

use super::metrics::{progress_of, BatchPending, Count, Counters, Metrics, Progress, VISIBLE};
use super::resume::{partial_path, sweep_partials, valid};
use super::runtime::{build_client, Clients};
use super::CANCELLED;
use crate::backend::lock::recover;
use crate::i18n::{tr, trf};

/// Why acquiring a permit can't fail: nothing closes these semaphores (they are private to this
/// file, which never calls `close()`).
const NEVER_CLOSED: &str = "the download semaphores are never closed";
use futures_util::{stream, StreamExt};
use reqwest::{header, Client, Response, StatusCode, Url};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering::Relaxed},
        Arc, Mutex, Weak,
    },
    time::{Duration, Instant, SystemTime},
};
use tokio::{
    fs,
    sync::{OwnedSemaphorePermit, Semaphore},
};

const MAX_RETRY_AFTER: Duration = Duration::from_secs(60);
type Sweeps = HashMap<PathBuf, Arc<tokio::sync::OnceCell<()>>>;
#[derive(Clone, Debug)]
pub struct Config {
    pub files: usize,
    pub connections: usize,
    pub per_host: usize,
    pub segments: usize,
    pub threshold: u64,
    pub minimum_segment: u64,
    pub connect_timeout: Duration,
    pub read_timeout: Duration,
    pub retries: u32,
    pub buffer: usize,
}
impl Default for Config {
    fn default() -> Self {
        Self {
            files: 16,
            connections: 32,
            per_host: 16,
            segments: 8,
            threshold: 32 << 20,
            minimum_segment: 4 << 20,
            connect_timeout: Duration::from_secs(10),
            read_timeout: Duration::from_secs(30),
            retries: 5,
            buffer: 128 << 10,
        }
    }
}
impl Config {
    pub fn from_env() -> Self {
        let mut c = Self::default();
        macro_rules! setting {
            ($field:ident,$name:literal) => {
                if let Ok(s) = std::env::var(concat!("SUNRISE_DOWNLOAD_", $name)) {
                    if let Ok(n) = s.parse() {
                        c.$field = n;
                    }
                }
            };
        }
        setting!(files, "FILES");
        setting!(connections, "CONNECTIONS");
        setting!(per_host, "PER_HOST");
        setting!(segments, "SEGMENTS");
        setting!(threshold, "THRESHOLD");
        setting!(minimum_segment, "MINIMUM_SEGMENT");
        setting!(retries, "RETRIES");
        setting!(buffer, "BUFFER");
        for (name, value) in [
            ("CONNECT_TIMEOUT", &mut c.connect_timeout),
            ("READ_TIMEOUT", &mut c.read_timeout),
        ] {
            if let Ok(n) = std::env::var(format!("SUNRISE_DOWNLOAD_{name}"))
                .unwrap_or_default()
                .parse::<u64>()
            {
                *value = Duration::from_secs(n.max(1));
            }
        }
        c
    }
}
#[derive(Clone, Debug)]
pub struct Artifact {
    pub url: String,
    pub path: PathBuf,
    pub sha1: String,
    pub size: Option<u64>,
}
pub struct Downloader {
    pub config: Config,
    clients: Arc<Mutex<Clients>>,
    network: Arc<Semaphore>,
    files: Arc<Semaphore>,
    hosts: Arc<Mutex<HashMap<String, Arc<Semaphore>>>>,
    paths: Arc<Mutex<HashMap<PathBuf, Weak<Semaphore>>>>,
    sweeps: Arc<Mutex<Sweeps>>,
    pub(super) metrics: Arc<Metrics>,
}
#[derive(Debug)]
pub(super) struct Failure {
    message: String,
    pub(super) retry: bool,
    pub(super) after: Duration,
    pub(super) range: bool,
}
impl Failure {
    pub(super) fn permanent(message: impl ToString) -> Self {
        Self {
            message: message.to_string(),
            retry: false,
            after: Duration::ZERO,
            range: false,
        }
    }
    pub(super) fn transient(message: impl ToString) -> Self {
        Self {
            retry: true,
            ..Self::permanent(message)
        }
    }
    pub(super) fn range(message: impl ToString) -> Self {
        Self {
            range: true,
            ..Self::permanent(message)
        }
    }
}
impl From<std::io::Error> for Failure {
    fn from(e: std::io::Error) -> Self {
        Self::permanent(e)
    }
}
impl From<reqwest::Error> for Failure {
    fn from(e: reqwest::Error) -> Self {
        Self {
            retry: e.is_timeout()
                || e.is_connect()
                || e.is_body()
                || e.is_decode()
                || e.is_request(),
            ..Self::permanent(e.without_url())
        }
    }
}
pub(super) struct Connection<'a> {
    pub(super) response: Response,
    _global: OwnedSemaphorePermit,
    _host: OwnedSemaphorePermit,
    _count: Count<'a>,
}
/// A refusal from `url`, as said to the player.
fn answered(url: &Url, status: StatusCode) -> String {
    trf(
        "{url} answered {status}",
        &[("url", url.as_str()), ("status", &status.to_string())],
    )
}

impl Downloader {
    pub fn new(mut config: Config) -> Result<Self, String> {
        config.files = config.files.clamp(1, 1024);
        config.connections = config.connections.clamp(1, 1024);
        config.per_host = config.per_host.clamp(1, 1024);
        config.segments = config.segments.clamp(1, 16);
        config.minimum_segment = config.minimum_segment.max(1);
        config.buffer = config.buffer.clamp(64 << 10, 256 << 10);
        let direct = build_client(&config, None).map_err(|e| e.message)?;
        Ok(Self {
            network: Arc::new(Semaphore::new(config.connections)),
            files: Arc::new(Semaphore::new(config.files)),
            config,
            clients: Arc::new(Mutex::new(Clients {
                direct,
                proxy: None,
            })),
            hosts: Arc::new(Mutex::new(HashMap::new())),
            paths: Arc::new(Mutex::new(HashMap::new())),
            sweeps: Arc::new(Mutex::new(HashMap::new())),
            metrics: Arc::new(Metrics {
                counters: Counters::default(),
                started: Instant::now(),
            }),
        })
    }
    pub fn progress(&self) -> Progress {
        progress_of(&self.metrics)
    }
    /// A job owns its metrics; all jobs still share the HTTP pool and resource budgets.
    pub fn session(&self) -> Self {
        let metrics = Arc::new(Metrics {
            counters: Counters::default(),
            started: Instant::now(),
        });
        *recover(&VISIBLE) = Some(Arc::downgrade(&metrics));
        self.with_metrics(metrics)
    }
    pub(super) fn with_metrics(&self, metrics: Arc<Metrics>) -> Self {
        Self {
            config: self.config.clone(),
            clients: self.clients.clone(),
            network: self.network.clone(),
            files: self.files.clone(),
            hosts: self.hosts.clone(),
            paths: self.paths.clone(),
            sweeps: self.sweeps.clone(),
            metrics,
        }
    }
    /// A pool is tied to its route. Changing Settings must never reuse an old proxy tunnel.
    pub(super) fn client_for_proxy(&self, proxy: Option<String>) -> Result<Client, Failure> {
        {
            let clients = recover(&self.clients);
            match &proxy {
                None => return Ok(clients.direct.clone()),
                Some(spec) => {
                    if let Some((kept, client)) = &clients.proxy {
                        if spec == kept {
                            return Ok(client.clone());
                        }
                    }
                }
            }
        }
        let client = build_client(&self.config, proxy.as_deref())?;
        let mut clients = recover(&self.clients);
        if let Some((spec, kept)) = &clients.proxy {
            if Some(spec) == proxy.as_ref() {
                return Ok(kept.clone());
            }
        }
        clients.proxy = proxy.map(|spec| (spec, client.clone()));
        Ok(client)
    }
    pub(super) async fn request(
        &self,
        url: &str,
        range: Option<(u64, u64)>,
        etag: Option<&str>,
    ) -> Result<Connection<'_>, Failure> {
        let mut url = Url::parse(url).map_err(Failure::permanent)?;
        for _ in 0..=10 {
            let host = url
                .host_str()
                .ok_or_else(|| Failure::permanent(tr("URL has no host")))?
                .to_owned();
            let budget = {
                let mut hosts = recover(&self.hosts);
                hosts
                    .entry(host)
                    .or_insert_with(|| Arc::new(Semaphore::new(self.config.per_host)))
                    .clone()
            };
            let host = budget.acquire_owned().await.expect(NEVER_CLOSED);
            let global = self
                .network
                .clone()
                .acquire_owned()
                .await
                .expect(NEVER_CLOSED);
            let count = Count::new(&self.metrics.counters.connections);
            let proxy =
                sunrise_launcher_proxy::address_for(&url, crate::proxy::setting().as_deref())
                    .map_err(Failure::permanent)?;
            let client = self.client_for_proxy(proxy.map(crate::proxy::address))?;
            let mut request = client
                .get(url.clone())
                .header(header::ACCEPT_ENCODING, "identity");
            if let Some((start, end)) = range {
                request = request.header(header::RANGE, format!("bytes={start}-{end}"));
            }
            if let Some(etag) = etag {
                request = request.header(header::IF_RANGE, etag);
            }
            let response = request.send().await?;
            if response.status().is_redirection() {
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|s| s.to_str().ok())
                    .ok_or_else(|| Failure::permanent(tr("Redirect has no Location")))?;
                url = redirect_target(&url, location)?;
                continue;
            }
            if !response.status().is_success() {
                let status = response.status();
                let after = response
                    .headers()
                    .get(header::RETRY_AFTER)
                    .and_then(|s| s.to_str().ok())
                    .map(|s| retry_after(s, SystemTime::now()))
                    .unwrap_or_default();
                return Err(Failure {
                    message: answered(&url, status),
                    retry: status == StatusCode::TOO_MANY_REQUESTS
                        || status == StatusCode::REQUEST_TIMEOUT
                        || status.is_server_error(),
                    after,
                    range: range.is_some()
                        && (status == StatusCode::RANGE_NOT_SATISFIABLE
                            || status == StatusCode::BAD_REQUEST),
                });
            }
            return Ok(Connection {
                response,
                _global: global,
                _host: host,
                _count: count,
            });
        }
        Err(Failure::permanent(tr("Too many redirects")))
    }
    pub(super) async fn pause(&self, attempt: u32, after: Duration) {
        self.metrics.counters.retried.fetch_add(1, Relaxed);
        let mut random = [0; 8];
        let _ = getrandom::fill(&mut random);
        let jitter = u64::from_ne_bytes(random) % 251;
        tokio::time::sleep(after.min(MAX_RETRY_AFTER).max(Duration::from_millis(
            (250u64.saturating_mul(1u64 << attempt.min(7)) + jitter).min(30_000),
        )))
        .await;
    }
    pub async fn download(&self, artifact: &Artifact, cancel: &AtomicBool) -> Result<(), String> {
        self.register(std::slice::from_ref(artifact));
        self.one(artifact, cancel).await
    }
    fn register(&self, artifacts: &[Artifact]) {
        self.metrics
            .counters
            .submitted
            .fetch_add(artifacts.len() as u64, Relaxed);
        self.metrics.counters.total.fetch_add(
            artifacts.iter().filter_map(|a| a.size).sum::<u64>(),
            Relaxed,
        );
    }
    async fn one(&self, artifact: &Artifact, cancel: &AtomicBool) -> Result<(), String> {
        let start = Instant::now();
        let operation = async {
            if artifact.sha1.len() != 40 || !artifact.sha1.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(Failure::permanent(tr(
                    "Artifact requires a 40-character SHA-1",
                )));
            }
            let lock = {
                let mut paths = recover(&self.paths);
                match paths.get(&artifact.path).and_then(Weak::upgrade) {
                    Some(lock) => lock,
                    None => {
                        let lock = Arc::new(Semaphore::new(1));
                        paths.insert(artifact.path.clone(), Arc::downgrade(&lock));
                        lock
                    }
                }
            };
            let _unique = lock.acquire().await.expect(NEVER_CLOSED);
            let _file = self.files.acquire().await.expect(NEVER_CLOSED);
            let _active = Count::new(&self.metrics.counters.active);
            let parent = artifact
                .path
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."));
            self.sweep(parent).await;
            if valid(artifact, &artifact.path, self.config.buffer).await? {
                return Ok(());
            }
            fs::create_dir_all(parent).await?;
            let partial = partial_path(artifact);
            fs::OpenOptions::new()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&partial)
                .await?;
            let result = async {
                let streamed = self.transfer(artifact, &partial).await?;
                let matches = match streamed {
                    Some(hash) => hash.eq_ignore_ascii_case(&artifact.sha1),
                    None => valid(artifact, &partial, self.config.buffer).await?,
                };
                if !matches {
                    let _ = fs::remove_file(&partial).await;
                    let _ = fs::remove_dir_all(partial.with_extension("segments")).await;
                    return Err(Failure::permanent(tr(
                        "Downloaded size or SHA-1 does not match the manifest",
                    )));
                }
                if cancel.load(Relaxed) {
                    return Err(Failure::permanent(CANCELLED));
                }
                fs::rename(&partial, &artifact.path).await?;
                let _ = fs::remove_dir_all(partial.with_extension("segments")).await;
                Ok(())
            }
            .await;
            result
        };
        let result = tokio::select! {biased; _=cancelled(cancel)=>Err(Failure::permanent(CANCELLED)),result=operation=>result};
        self.metrics.counters.file_micros.store(
            start.elapsed().as_micros().min(u64::MAX as u128) as u64,
            Relaxed,
        );
        {
            let mut paths = recover(&self.paths);
            if paths
                .get(&artifact.path)
                .is_some_and(|p| p.strong_count() == 0)
            {
                paths.remove(&artifact.path);
            }
        }
        match result {
            Ok(()) => {
                self.metrics.counters.completed.fetch_add(1, Relaxed);
                self.metrics
                    .counters
                    .verified
                    .fetch_add(artifact.size.unwrap_or(0), Relaxed);
                Ok(())
            }
            Err(e) => {
                self.metrics.counters.failed.fetch_add(1, Relaxed);
                Err(if e.message == CANCELLED {
                    e.message
                } else {
                    trf(
                        "{url}: {message} (after {seconds}s)",
                        &[
                            ("url", artifact.url.as_str()),
                            ("message", e.message.as_str()),
                            ("seconds", &format!("{:.2}", start.elapsed().as_secs_f64())),
                        ],
                    )
                })
            }
        }
    }
    pub async fn batch(
        &self,
        artifacts: &[Artifact],
        cancel: &AtomicBool,
        log: &dyn Fn(String),
    ) -> Result<(), String> {
        self.report(self.batch_policy(artifacts, cancel, true), log)
            .await
    }
    pub async fn install(
        &self,
        required: &[Artifact],
        optional: &[Artifact],
        cancel: &AtomicBool,
        log: &dyn Fn(String),
    ) -> Result<Result<(), String>, String> {
        self.report(
            async {
                let (_, optional) =
                    tokio::try_join!(self.batch_policy(required, cancel, true), async {
                        Ok::<_, String>(self.batch_policy(optional, cancel, false).await)
                    })?;
                Ok(optional)
            },
            log,
        )
        .await
    }
    async fn report<F: std::future::Future>(&self, future: F, log: &dyn Fn(String)) -> F::Output {
        tokio::pin!(future);
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.tick().await;
        loop {
            tokio::select! {result=&mut future=>return result,_=tick.tick()=>{let p=self.progress();log(trf("{completed} files complete, {active} active, {pending} pending; {speed} MiB/s; {verified}/{total} bytes",&[("completed",&p.completed.to_string()),("active",&p.active.to_string()),("pending",&p.pending.to_string()),("speed",&format!("{:.2}",p.mib_per_second)),("verified",&p.verified_bytes.to_string()),("total",&p.total_bytes.to_string())]));}}
        }
    }
    async fn batch_policy(
        &self,
        artifacts: &[Artifact],
        cancel: &AtomicBool,
        fail_fast: bool,
    ) -> Result<(), String> {
        self.register(artifacts);
        let mut first = None;
        let mut pending = BatchPending {
            metrics: self.metrics.clone(),
            remaining: artifacts.len() as u64,
        };
        let work = stream::iter(artifacts.iter().map(|a| self.one(a, cancel)))
            .buffer_unordered(self.config.files);
        tokio::pin!(work);
        while let Some(result) = work.next().await {
            pending.remaining -= 1;
            if let Err(e) = result {
                if fail_fast {
                    return Err(e);
                }
                first.get_or_insert(e);
            }
        }
        first.map_or(Ok(()), Err)
    }
    async fn sweep(&self, parent: &Path) {
        let once = recover(&self.sweeps)
            .entry(parent.to_owned())
            .or_default()
            .clone();
        once.get_or_init(|| async {
            let parent = parent.to_owned();
            let _ = tokio::task::spawn_blocking(move || sweep_partials(&parent)).await;
        })
        .await;
    }
    /// Metadata is small and must be parsed before dependent work can be queued.
    pub async fn metadata(&self, url: &str, cancel: &AtomicBool) -> Result<Vec<u8>, String> {
        let operation = async {
            for attempt in 0..=self.config.retries {
                let result = async {
                    let mut connection = self.request(url, None, None).await?;
                    if connection.response.status() != StatusCode::OK {
                        return Err(Failure::permanent(tr(
                            "Unexpected partial metadata response",
                        )));
                    }
                    let mut body = Vec::new();
                    while let Some(chunk) = connection.response.chunk().await? {
                        if body.len() + chunk.len() > 16 << 20 {
                            return Err(Failure::permanent(tr("Metadata exceeds 16 MiB")));
                        }
                        self.metrics
                            .counters
                            .bytes
                            .fetch_add(chunk.len() as u64, Relaxed);
                        body.extend_from_slice(&chunk);
                    }
                    Ok(body)
                }
                .await;
                match result {
                    Ok(body) => return Ok(body),
                    Err(e) if e.retry && attempt < self.config.retries => {
                        self.pause(attempt, e.after).await
                    }
                    Err(e) => return Err(e.message),
                }
            }
            unreachable!()
        };
        tokio::select! {biased; _=cancelled(cancel)=>Err(CANCELLED.into()),result=operation=>result}
    }
}
pub(super) fn redirect_target(url: &Url, location: &str) -> Result<Url, Failure> {
    let target = url.join(location).map_err(Failure::permanent)?;
    if target.scheme() != "https" {
        return Err(Failure::permanent(tr("Redirect target must use HTTPS")));
    }
    Ok(target)
}
pub(super) fn retry_after(value: &str, now: SystemTime) -> Duration {
    value
        .parse::<u64>()
        .ok()
        .map(Duration::from_secs)
        .or_else(|| {
            httpdate::parse_http_date(value)
                .ok()
                .map(|date| date.duration_since(now).unwrap_or_default())
        })
        .unwrap_or_default()
        .min(MAX_RETRY_AFTER)
}

async fn cancelled(cancel: &AtomicBool) {
    loop {
        if cancel.load(Relaxed) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

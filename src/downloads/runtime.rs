//! The process-wide tokio runtime and shared downloader used by callers that cannot await.

use super::metrics::VISIBLE;
use super::transfer::{Config, Downloader, Failure};
use crate::backend::lock::recover;
use reqwest::{Client, Url};
use std::sync::{atomic::AtomicBool, OnceLock, Weak};

static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
pub fn run<F: std::future::Future>(future: F) -> F::Output {
    RUNTIME
        .get_or_init(|| {
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("download runtime")
        })
        .block_on(future)
}
static SHARED: OnceLock<Result<Downloader, String>> = OnceLock::new();
pub fn shared() -> Result<&'static Downloader, String> {
    SHARED
        .get_or_init(|| Downloader::new(Config::from_env()))
        .as_ref()
        .map_err(Clone::clone)
}
pub fn get_bytes(url: &str) -> Result<Vec<u8>, String> {
    run(shared()?.metadata(url, &AtomicBool::new(false)))
}
pub fn install_metadata(url: &str) -> Result<Vec<u8>, String> {
    let engine = shared()?;
    let metrics = recover(&VISIBLE).as_ref().and_then(Weak::upgrade);
    match metrics {
        Some(metrics) => run(engine
            .with_metrics(metrics)
            .metadata(url, &crate::mojang::CANCEL)),
        None => run(engine.metadata(url, &crate::mojang::CANCEL)),
    }
}
pub(super) struct Clients {
    pub(super) direct: Client,
    pub(super) proxy: Option<(String, Client)>,
}
pub(super) fn build_client(config: &Config, proxy: Option<&str>) -> Result<Client, Failure> {
    let mut builder = Client::builder()
        .user_agent(concat!("sunrise-launcher/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(config.connect_timeout)
        .read_timeout(config.read_timeout)
        .pool_max_idle_per_host(config.per_host)
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy();
    if let Some(proxy) = proxy {
        let url = Url::parse(proxy).map_err(Failure::permanent)?;
        if !matches!(
            url.scheme(),
            "http" | "https" | "socks4" | "socks4a" | "socks5" | "socks5h"
        ) {
            return Err(Failure::permanent(crate::i18n::tr(
                "Unsupported proxy scheme",
            )));
        }
        builder = builder.proxy(reqwest::Proxy::all(proxy)?);
    }
    builder.build().map_err(Into::into)
}

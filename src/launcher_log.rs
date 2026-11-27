//! The launcher's own log: every line goes, scrubbed, to a rotating file in the logs folder and to
//! an in-memory ring the Logs panel reads. Words in log lines are English: the log is for bug
//! reports, not for the player to read in their language.
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use sunrise_launcher_telemetry::scrub::Scrubber;

/// How many of the newest lines the ring keeps.
const RING_LINES: usize = 5000;
/// The size at which `launcher.log` is rotated.
const FILE_LIMIT: u64 = 1 << 20;
/// How many rotated files are kept: `launcher.1.log` to `launcher.3.log`.
const OLD_FILES: u32 = 3;
/// What replaces a secret.
const SECRET: &str = "<secret>";
/// Query, form and JSON keys whose values are secrets. A key matches as the end of a longer
/// name (`access_token`, `refresh_token`, `id_token` all end in `token`).
const SECRET_KEYS: [&str; 6] = ["code", "token", "client_secret", "password", "key", "sig"];

/// The newest lines, numbered by the revision at which each was logged, each with the
/// [`Operation`] it was logged in, if any.
struct Ring {
    lines: VecDeque<(Option<u64>, String)>,
    /// How many lines were ever pushed.
    revision: u64,
}

impl Ring {
    const fn new() -> Self {
        Self {
            lines: VecDeque::new(),
            revision: 0,
        }
    }

    #[cfg(test)]
    fn push(&mut self, line: String) {
        self.push_in(None, line);
    }

    fn push_in(&mut self, operation: Option<u64>, line: String) {
        if self.lines.len() == RING_LINES {
            self.lines.pop_front();
        }
        self.lines.push_back((operation, line));
        self.revision += 1;
    }

    /// The lines after revision `since` that are still kept, and the newest revision; only
    /// those logged in operation `only` when it is given.
    fn since_in(&self, since: u64, only: Option<u64>) -> (u64, Vec<String>) {
        let first = self.revision - self.lines.len() as u64;
        let skip = since.saturating_sub(first).min(self.lines.len() as u64) as usize;
        (
            self.revision,
            self.lines
                .iter()
                .skip(skip)
                .filter(|(operation, _)| only.is_none() || *operation == only)
                .map(|(_, line)| line.clone())
                .collect(),
        )
    }

    #[cfg(test)]
    fn since(&self, since: u64) -> (u64, Vec<String>) {
        self.since_in(since, None)
    }
}

/// `dir/launcher.log` and its rotated files. A file that can't be opened or written is left
/// alone from then on, so a full disk never slows or breaks the launcher.
struct LogFile {
    dir: PathBuf,
    file: Option<File>,
    size: u64,
    limit: u64,
}

impl LogFile {
    fn open(dir: &Path, limit: u64) -> Self {
        let _ = fs::create_dir_all(dir);
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("launcher.log"))
            .ok();
        let size = file
            .as_ref()
            .and_then(|f| f.metadata().ok())
            .map_or(0, |m| m.len());
        Self {
            dir: dir.to_owned(),
            file,
            size,
            limit,
        }
    }

    fn write(&mut self, line: &str) {
        if self.file.is_none() {
            return;
        }
        if self.size >= self.limit {
            self.rotate();
        }
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if writeln!(file, "{line}").is_err() {
            self.file = None;
            return;
        }
        self.size += line.len() as u64 + 1;
    }

    /// Moves `launcher.log` to `launcher.1.log` and each older file one up, dropping the oldest,
    /// then starts a new `launcher.log`.
    fn rotate(&mut self) {
        // Windows can't rename an open file, nor onto an existing one.
        self.file = None;
        let name = |n: u32| match n {
            0 => self.dir.join("launcher.log"),
            n => self.dir.join(format!("launcher.{n}.log")),
        };
        for n in (0..OLD_FILES).rev() {
            let _ = fs::remove_file(name(n + 1));
            let _ = fs::rename(name(n), name(n + 1));
        }
        *self = Self::open(&self.dir, self.limit);
    }
}

static RING: Mutex<Ring> = Mutex::new(Ring::new());
static FILE: Mutex<Option<LogFile>> = Mutex::new(None);
/// The scrubber, built on the first line after `init`.
static SCRUBBER: Mutex<Option<Arc<Scrubber>>> = Mutex::new(None);
/// What `hide` was told, kept for a scrubber that isn't built yet.
static HIDDEN: Mutex<Vec<String>> = Mutex::new(Vec::new());

thread_local! {
    /// Whether this thread is inside building the scrubber, which reads the accounts and may log.
    static BUILDING: Cell<bool> = const { Cell::new(false) };
    /// The operation this thread works for, if any (`Operation::enter`).
    static CURRENT: RefCell<Option<Operation>> = const { RefCell::new(None) };
}

static OPERATION_NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// One piece of work the window follows on its own, such as a switch with its download,
/// preparation and profile change: the lines its threads log carry its id, and its live step
/// text goes to its own sink. Another job's lines and counts (startup's copy of prepared files,
/// a Play elsewhere) never reach it. A thread joins with [`Operation::enter`]; a thread it
/// starts joins by entering [`current`] there.
#[derive(Clone)]
pub struct Operation {
    id: u64,
    sink: Option<PlayDetailSink>,
}

impl Operation {
    /// A new operation whose step text goes to `sink`.
    pub fn new(sink: impl Fn(String) + Send + Sync + 'static) -> Self {
        Self {
            id: OPERATION_NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            sink: Some(Arc::new(sink)),
        }
    }

    pub fn id(&self) -> u64 {
        self.id
    }

    /// This thread works for the operation until the guard is dropped.
    pub fn enter(&self) -> OperationGuard {
        let prev = CURRENT.with(|current| current.replace(Some(self.clone())));
        OperationGuard { prev }
    }
}

/// Puts back the operation the thread worked for before [`Operation::enter`].
pub struct OperationGuard {
    prev: Option<Operation>,
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        let prev = self.prev.take();
        CURRENT.with(|current| *current.borrow_mut() = prev);
    }
}

/// The operation this thread works for, to hand to a thread it starts.
pub fn current() -> Option<Operation> {
    CURRENT.with(|current| current.borrow().clone())
}

/// Enters `operation` when there is one: for a thread started by a thread that worked for it.
pub fn enter(operation: &Option<Operation>) -> Option<OperationGuard> {
    operation.as_ref().map(Operation::enter)
}

fn current_id() -> Option<u64> {
    CURRENT.with(|current| current.borrow().as_ref().map(|op| op.id))
}

/// Where step text goes from this thread: its operation's sink, else the process-wide one.
fn detail_sink() -> Option<PlayDetailSink> {
    CURRENT
        .with(|current| current.borrow().as_ref().map(|op| op.sink.clone()))
        .unwrap_or_else(|| lock(&PLAY_DETAIL).as_ref().map(|(_, sink)| sink.clone()))
}

/// A lock that a panic elsewhere can't turn into a panic here.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Starts the file: `dir/launcher.log`, rotated at 1 MiB into launcher.1.log .. launcher.3.log
/// (oldest dropped). Called once by `main()` before anything else logs, for the window and the
/// command line alike. Without it (tests, other tools) lines go only to the ring.
pub fn init(dir: &Path) {
    {
        let mut file = lock(&FILE);
        if file.is_some() {
            return;
        }
        *file = Some(LogFile::open(dir, FILE_LIMIT));
    }
    line(format!(
        "Sunrise launcher {} on {} {}",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    ));
}

/// The logs folder `init` was given.
pub fn dir() -> Option<PathBuf> {
    lock(&FILE).as_ref().map(|f| f.dir.clone())
}

/// One line: "HH:MM:SS.mmm text" (local time, chrono), scrubbed, into the ring and the file.
/// Multi-line text becomes several lines. Never panics; a file error is ignored after the first.
pub fn line(text: impl AsRef<str>) {
    line_in(current_id(), text.as_ref());
}

fn line_in(operation: Option<u64>, text: &str) {
    let text = scrub(text);
    let stamp = chrono::Local::now().format("%H:%M:%S%.3f");
    let mut file = lock(&FILE);
    for text in text.lines() {
        let line = format!("{stamp} {text}");
        if let Some(file) = file.as_mut() {
            file.write(&line);
        }
        lock(&RING).push_in(operation, line);
    }
}

type PlayDetailSink = std::sync::Arc<dyn Fn(String) + Send + Sync>;

static PLAY_DETAIL_NEXT_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
static PLAY_DETAIL: Mutex<Option<(u64, PlayDetailSink)>> = Mutex::new(None);

/// Keeps a Play progress sink installed across all threads until dropped.
pub struct PlayDetailGuard {
    id: u64,
    prev: Option<(u64, PlayDetailSink)>,
}

impl Drop for PlayDetailGuard {
    fn drop(&mut self) {
        let mut slot = lock(&PLAY_DETAIL);
        if slot.as_ref().is_some_and(|(id, _)| *id == self.id) {
            *slot = self.prev.take();
        }
    }
}

/// Installs a process-wide sink that receives live Play step text from any thread while the guard
/// is alive.
pub fn on_play_detail(sink: impl Fn(String) + Send + Sync + 'static) -> PlayDetailGuard {
    let id = PLAY_DETAIL_NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let prev = lock(&PLAY_DETAIL).replace((id, std::sync::Arc::new(sink)));
    PlayDetailGuard { id, prev }
}

/// Sends a Play step line to the window (when a sink is installed) and to `launcher.log`.
pub fn play_detail(text: impl Into<String>) {
    let text = text.into();
    line(&text);
    play_detail_ui_only(text);
}

/// Sends a Play step line to the window without adding another `launcher.log` line: to this
/// thread's operation when it works for one, else to the process-wide sink.
pub fn play_detail_ui_only(text: impl Into<String>) {
    let text = text.into();
    if let Some(sink) = detail_sink() {
        sink(text);
    }
}

/// Throttles per-file progress updates from parallel workers so the window updates every ~250 ms
/// and `launcher.log` gets a line at most every ~2 s without flooding either.
pub struct PlayMeter {
    sink: Option<PlayDetailSink>,
    /// The operation of the thread that made it: its workers' counts are that operation's.
    operation: Option<u64>,
    format: fn(usize, usize) -> String,
    state: Mutex<(Option<Instant>, Option<Instant>, usize)>,
}

impl PlayMeter {
    pub fn new(format: fn(usize, usize) -> String) -> Self {
        Self {
            sink: detail_sink(),
            operation: current_id(),
            format,
            state: Mutex::new((None, None, 0)),
        }
    }

    pub fn tick(&self, done: usize, total: usize) {
        if total == 0 {
            return;
        }
        let now = Instant::now();
        let (emit_ui, emit_log, shown) = {
            let mut guard = lock(&self.state);
            let done = done.min(total);
            if done > guard.2 {
                guard.2 = done;
            }
            let shown = guard.2;
            let ui = self.sink.is_some()
                && (shown == 1
                    || shown >= total
                    || guard.0.is_none_or(|t| {
                        now.saturating_duration_since(t) >= Duration::from_millis(250)
                    }));
            let log = guard
                .1
                .is_none_or(|t| now.saturating_duration_since(t) >= Duration::from_secs(2));
            if ui {
                guard.0 = Some(now);
            }
            if log {
                guard.1 = Some(now);
            }
            (ui, log, shown)
        };
        if !emit_ui && !emit_log {
            return;
        }
        let text = (self.format)(shown, total);
        if emit_log {
            line_in(self.operation, &text);
        }
        if emit_ui {
            if let Some(sink) = &self.sink {
                sink(text);
            }
        }
    }
}

/// A long step. Logs "{what}: started" now; `done` logs "{what}: done in {duration}{: detail}",
/// `failed` logs "{what}: failed after {duration}: {why}", and a Step dropped without either
/// logs "{what}: ended after {duration} with no result". Duration as "850 ms" under a second,
/// else "12.3 s". Plain words, so any font shows them and a search finds them.
pub fn step(what: impl Into<String>) -> Step {
    let what = what.into();
    line(format!("{what}: started"));
    Step {
        what,
        started: Instant::now(),
        finished: false,
    }
}

/// A step that has started and not yet said how it ended.
#[must_use = "a step that is dropped logs that it ended without a result"]
pub struct Step {
    what: String,
    started: Instant,
    finished: bool,
}

impl Step {
    /// Progress inside the step.
    pub fn note(&self, text: impl AsRef<str>) {
        line(format!("{}: {}", self.what, text.as_ref()));
    }

    pub fn done(mut self, detail: impl AsRef<str>) {
        self.finished = true;
        let detail = detail.as_ref();
        let sep = if detail.is_empty() { "" } else { ": " };
        line(format!(
            "{}: done in {}{sep}{detail}",
            self.what,
            duration_text(self.started.elapsed())
        ));
    }

    /// The step stopped because the player cancelled it.
    pub fn cancelled(mut self) {
        self.finished = true;
        line(format!(
            "{}: cancelled after {}",
            self.what,
            duration_text(self.started.elapsed())
        ));
    }

    /// Logs `why` only as [`safe_error`]'s category: none of its text is kept.
    pub fn failed(mut self, why: impl AsRef<str>) {
        self.finished = true;
        line(format!(
            "{}: failed after {}: {}",
            self.what,
            duration_text(self.started.elapsed()),
            safe_error(why.as_ref())
        ));
    }
}

impl Drop for Step {
    fn drop(&mut self) {
        if !self.finished {
            line(format!(
                "{}: ended after {} with no result",
                self.what,
                duration_text(self.started.elapsed())
            ));
        }
    }
}

fn duration_text(elapsed: Duration) -> String {
    if elapsed < Duration::from_secs(1) {
        format!("{} ms", elapsed.as_millis())
    } else {
        format!("{:.1} s", elapsed.as_secs_f64())
    }
}

/// The ring (newest 5000 lines): the lines after revision `since`, and the newest revision.
/// Revision counts every line ever logged, so a reader at revision r asks `since(r)`.
pub fn since(since: u64) -> (u64, Vec<String>) {
    lock(&RING).since_in(since, None)
}

/// [`since`], only the lines logged in operation `id`.
pub fn since_in(since: u64, id: u64) -> (u64, Vec<String>) {
    lock(&RING).since_in(since, Some(id))
}

/// What `launcher.log` and the ring keep of a line the game's preparer wrote, or a line about
/// its preparation: the line itself, or a fixed note when it names a file or an address. The
/// preparer writes paths in its progress lines ("Imported client 26.3 from <file>") and its
/// errors, and no rule can tell where such a path ends (review of #615), so a line with a path
/// separator or an address is left out whole. The window's Logs panel still shows it.
pub fn preparer_line(line: &str) -> String {
    format!("Preparing: {}", without_paths(line))
}

/// `line` as the log keeps a line about preparing or downloading a build's files: as it is, or a
/// fixed note when it has a path separator or an address in it (see [`preparer_line`]).
pub fn without_paths(line: &str) -> &str {
    if line.contains(['/', '\\']) || line.contains("://") {
        "(a line that names a file or an address, left out of the log)"
    } else {
        line
    }
}

/// An error's text as the log keeps it: a fixed category and, when the text has one, its OS
/// error code or HTTP status (`entity not found (os error 2)`, `HTTP 404`). Nothing else of the
/// text reaches `launcher.log` or the ring: an error's words can hold a local path with any
/// spaces or punctuation in it (`/home/ada/Private Client Backup/drafts`) or an address, and no
/// rule can tell where such a path ends (review of #615). The window's error view keeps the whole
/// text, and the step's own name says what failed.
pub fn safe_error(text: &str) -> String {
    let text = text.trim();
    if text == crate::native_mods::CANCELLED || text == crate::downloads::CANCELLED {
        return "cancelled".into();
    }
    if text.ends_with(" is already being prepared.") {
        return "the build is already being prepared".into();
    }
    if let Some(code) = os_error(text) {
        let kind = std::io::Error::from_raw_os_error(code).kind();
        return format!("{kind} (os error {code})");
    }
    if let Some(status) = http_status(text) {
        return format!("HTTP {status}");
    }
    "an error with no OS error code or HTTP status (its text stays out of the log)".into()
}

/// The code of the last `(os error N)` in `text`, as Rust's I/O errors write it.
fn os_error(text: &str) -> Option<i32> {
    const MARK: &str = "(os error ";
    let at = text.rfind(MARK)? + MARK.len();
    let digits = &text[at..text[at..].find(')')? + at];
    digits.trim().parse().ok()
}

/// An HTTP status after `answered`, `status` or `HTTP`, as the launcher's downloads write it
/// (`… answered 404 Not Found`, `status 404`).
fn http_status(text: &str) -> Option<u16> {
    let words: Vec<&str> = text.split_whitespace().collect();
    words.windows(2).find_map(|pair| {
        let lead = pair[0].trim_end_matches(':').to_ascii_lowercase();
        let number = pair[1].trim_end_matches([',', '.', ':', ';', ')']);
        (matches!(lead.as_str(), "answered" | "status" | "http") && number.len() == 3)
            .then(|| number.parse().ok())
            .flatten()
            .filter(|status| (100..600).contains(status))
    })
}

/// Hide `term` from now on (an account name, a server address). Terms under 3 chars ignored.
/// The accounts are read often, so a term already hidden is skipped.
pub fn hide(term: impl Into<String>) {
    let term = term.into();
    let mut scrubber = lock(&SCRUBBER);
    let mut hidden = lock(&HIDDEN);
    if hidden.contains(&term) {
        return;
    }
    if let Some(built) = scrubber.as_mut() {
        *built = Arc::new((**built).clone().term(term.clone()));
    }
    hidden.push(term);
}

/// The scrubber, built on first use like `crate::telemetry::scrubber()` and told every hidden term.
fn scrubber() -> Arc<Scrubber> {
    if let Some(built) = lock(&SCRUBBER).clone() {
        return built;
    }
    if BUILDING.replace(true) {
        // A line logged while the accounts load: only the generic rules for it.
        return Arc::new(Scrubber::new());
    }
    let mut base = crate::telemetry::scrubber();
    BUILDING.set(false);
    let mut slot = lock(&SCRUBBER);
    for term in lock(&HIDDEN).iter() {
        base = base.term(term);
    }
    let built = Arc::new(base);
    *slot = Some(built.clone());
    built
}

/// The scrub every shown, copied or saved line passes (the game's lines in the panel too).
pub fn scrub(text: &str) -> String {
    hide_secrets(&scrubber().scrub(text))
}

/// `text` with `SECRET` in place of what looks like a credential: a JWT, a bearer token, the
/// value of a key like `code` or `access_token` (as `k=v` or `"k":"v"`), and any long run of
/// base64url characters that mixes upper case, lower case and digits (lowercase hex digests
/// and ordinary words stay).
fn hide_secrets(text: &str) -> String {
    let mut out = text.to_owned();
    for key in SECRET_KEYS {
        let lower = out.to_ascii_lowercase();
        out = replace_spans(&out, |from| value_after_key(&lower, key, from));
    }
    let lower = out.to_ascii_lowercase();
    out = replace_spans(&out, |from| bearer_token(&lower, from));
    replace_spans(&out, |from| secret_run(&out, from))
}

/// `text` with each span `next` finds (given where to look from) replaced by `SECRET`.
fn replace_spans(text: &str, mut next: impl FnMut(usize) -> Option<Range<usize>>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut from = 0;
    while let Some(span) = next(from) {
        out.push_str(&text[from..span.start]);
        out.push_str(SECRET);
        from = span.end;
    }
    out.push_str(&text[from..]);
    out
}

/// The value of the next `key` (as a whole word, in `lower`, ASCII-lowercased text) at or after
/// `from`, written `key=value`, `"key":"value"` or with escaped quotes. A bare `key: value` is
/// ordinary prose and stays.
fn value_after_key(lower: &str, key: &str, from: usize) -> Option<Range<usize>> {
    let bytes = lower.as_bytes();
    let quote = |b: &u8| matches!(b, b'"' | b'\'' | b'\\');
    let mut at = from;
    while let Some(found) = lower[at..].find(key) {
        let start = at + found;
        let mut i = start + key.len();
        at = i;
        if start > 0 && bytes[start - 1].is_ascii_alphanumeric() {
            continue;
        }
        let quoted = bytes.get(i).is_some_and(quote);
        while bytes.get(i).is_some_and(quote) {
            i += 1;
        }
        while bytes.get(i) == Some(&b' ') {
            i += 1;
        }
        match bytes.get(i) {
            Some(b'=') => {}
            Some(b':') if quoted => {}
            _ => continue,
        }
        i += 1;
        while bytes.get(i).is_some_and(|b| *b == b' ' || quote(b)) {
            i += 1;
        }
        let len = bytes[i..]
            .iter()
            .take_while(|b| {
                !quote(b)
                    && !matches!(b, b'&' | b' ' | b'\t' | b'\r' | b'\n' | b',' | b';')
                    && !matches!(b, b')' | b'}' | b']')
            })
            .count();
        if len > 0 && &lower[i..i + len] != SECRET {
            return Some(i..i + len);
        }
    }
    None
}

/// The token of the next `Bearer <token>` in `lower` at or after `from`.
fn bearer_token(lower: &str, from: usize) -> Option<Range<usize>> {
    const HEAD: &str = "bearer ";
    let mut at = from;
    while let Some(found) = lower[at..].find(HEAD) {
        let start = at + found + HEAD.len();
        at = start;
        let len = lower.as_bytes()[start..]
            .iter()
            .take_while(|b| {
                b.is_ascii_alphanumeric()
                    || matches!(b, b'.' | b'_' | b'~' | b'+' | b'/' | b'=' | b'-')
            })
            .count();
        if len > 0 {
            return Some(start..start + len);
        }
    }
    None
}

/// The next run of base64url characters (with `.` and `=`) at or after `from` that is a JWT
/// (`eyJ…` with two dots) or is 32 or more characters with an upper case letter, a lower case
/// letter and a digit.
fn secret_run(text: &str, from: usize) -> Option<Range<usize>> {
    let bytes = text.as_bytes();
    let in_run = |b: &u8| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'=');
    let mut i = from;
    while i < bytes.len() {
        if !in_run(&bytes[i]) {
            i += 1;
            continue;
        }
        let start = i;
        while bytes.get(i).is_some_and(in_run) {
            i += 1;
        }
        let run = &text[start..i];
        let jwt = run.contains("eyJ") && run.matches('.').count() >= 2;
        let long = run.len() >= 32
            && run.bytes().any(|b| b.is_ascii_uppercase())
            && run.bytes().any(|b| b.is_ascii_lowercase())
            && run.bytes().any(|b| b.is_ascii_digit());
        if jwt || long {
            return Some(start..i);
        }
    }
    None
}

/// `eprintln!` that also logs: `sunrise_launcher::elog!` (bin) / `crate::elog!` (lib).
#[macro_export]
macro_rules! elog {
    ($($t:tt)*) => {{
        let text = format!($($t)*);
        eprintln!("{text}");
        $crate::launcher_log::line(&text);
    }};
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sign_in_trace_carries_no_secret_and_keeps_hosts_and_statuses() {
        hide("Steve_Tester");
        let jwt = "eyJhbGciOiJSUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.c2lnbmF0dXJlMTIzNDU2";
        let trace = [
            "Signing in: login.microsoftonline.com".to_owned(),
            "POST login.microsoftonline.com 200".to_owned(),
            "POST user.auth.xboxlive.com 200".to_owned(),
            "POST xsts.auth.xboxlive.com 401".to_owned(),
            "POST api.minecraftservices.com 200".to_owned(),
            format!("response token {jwt} accepted"),
            "redirect https://login.microsoftonline.com/cb?access_token=abc123def&state=1"
                .to_owned(),
            r#"{"user_code":"ZXCV9876","device_code":"dev1","expires_in":900}"#.to_owned(),
            r#"{"code":"M.C507_BAY.2.U.qwerty"}"#.to_owned(),
            "Authorization: Bearer shorttok".to_owned(),
            "refresh_token=r.1.2&client_secret=hush&password=pw1".to_owned(),
            "account 069a79f4-44e9-4726-a5be-fca90e38aaf5 is Steve_Tester".to_owned(),
        ]
        .join("\n");
        let out = scrub(&trace);
        for secret in [
            "eyJ",
            "abc123def",
            "ZXCV9876",
            "M.C507_BAY",
            "qwerty",
            "shorttok",
            "r.1.2",
            "hush",
            "pw1",
            "069a79f4",
            "Steve_Tester",
        ] {
            assert!(!out.contains(secret), "{secret} survived in:\n{out}");
        }
        for kept in [
            "login.microsoftonline.com 200",
            "user.auth.xboxlive.com 200",
            "xsts.auth.xboxlive.com 401",
            "api.minecraftservices.com 200",
            "state=1",
            "expires_in",
        ] {
            assert!(out.contains(kept), "{kept} lost in:\n{out}");
        }
    }

    #[test]
    fn a_sha256_and_a_sentence_pass_unchanged() {
        let sha = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        let sentence = "Checking game files: 1204 of 5310 verified, nothing to download";
        assert_eq!(scrub(sha), sha);
        assert_eq!(scrub(sentence), sentence);
    }

    #[test]
    fn an_operation_sees_only_its_own_lines_and_counts_even_from_its_workers() {
        let (rev, _) = since(u64::MAX);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let op = Operation::new({
            let seen = seen.clone();
            move |text| lock(&seen).push(text)
        });
        // Another job's line and step text, outside the operation, while it runs.
        line("startup: copying another build's files");
        {
            let _in = op.enter();
            line("switch: started");
            play_detail_ui_only("Preparing the game");
            let meter = PlayMeter::new(|done, total| format!("Copying: {done} of {total}"));
            let handed = current();
            std::thread::spawn(move || {
                let _in = enter(&handed);
                line("switch: a worker's line");
            })
            .join()
            .unwrap();
            std::thread::scope(|scope| {
                scope.spawn(|| meter.tick(1, 2));
            });
        }
        line("startup: done");
        let (_, mine) = since_in(rev, op.id());
        let text: Vec<&str> = mine.iter().map(|l| l.split_once(' ').unwrap().1).collect();
        assert_eq!(
            text,
            [
                "switch: started",
                "switch: a worker's line",
                "Copying: 1 of 2"
            ]
        );
        assert_eq!(*lock(&seen), ["Preparing the game", "Copying: 1 of 2"]);
        assert!(
            current().is_none(),
            "the thread works for nothing once the guard drops"
        );
    }

    /// None of an error's words reach the log, only its kind: a Unix path with several spaces
    /// in a folder's name, a Windows one with spaces and a drive letter, a UNC path and an
    /// address with credentials (review of #615).
    #[test]
    fn a_logged_error_keeps_its_kind_and_none_of_its_text() {
        let private = [
            "ada", "Ada", "Private", "Client", "Backup", "drafts", "hunter2", "nas",
        ];
        let cases = [
            (
                "/home/ada/Private Client Backup/drafts/dev-1/a.tar.gz: No such file or directory (os error 2)",
                format!("{} (os error 2)", std::io::Error::from_raw_os_error(2).kind()),
            ),
            (
                r"C:\Users\Ada Lovelace\Private Client Backup\drafts\a.tar.gz: Access is denied. (os error 5)",
                format!("{} (os error 5)", std::io::Error::from_raw_os_error(5).kind()),
            ),
            (
                r"\\nas\Private Client Backup\drafts: (os error 53)",
                format!("{} (os error 53)", std::io::Error::from_raw_os_error(53).kind()),
            ),
            (
                "https://ada:hunter2@github.com/sunrise-org/Private Client Backup/a.tar.gz answered 404 Not Found",
                "HTTP 404".into(),
            ),
            (
                "Could not read /home/ada/Private Client Backup/drafts, it's gone",
                "an error with no OS error code or HTTP status (its text stays out of the log)".into(),
            ),
        ];
        for (error, logged) in cases {
            let safe = safe_error(error);
            assert_eq!(safe, logged, "{error}");
            for word in private {
                assert!(
                    !safe.contains(word),
                    "{word:?} of {error:?} logged as {safe:?}"
                );
            }
        }
        assert_eq!(safe_error(crate::native_mods::CANCELLED), "cancelled");
        assert_eq!(
            safe_error("Sunrise dev-2026-11-25-bbbbbbb is already being prepared."),
            "the build is already being prepared"
        );
    }

    #[test]
    fn since_returns_only_newer_lines_and_the_ring_keeps_the_newest() {
        let mut ring = Ring::new();
        for n in 0..3 {
            ring.push(format!("line {n}"));
        }
        assert_eq!(
            ring.since(0),
            (3, vec!["line 0".into(), "line 1".into(), "line 2".into()])
        );
        assert_eq!(ring.since(2), (3, vec!["line 2".into()]));
        assert_eq!(ring.since(3), (3, Vec::new()));
        for n in 3..RING_LINES + 10 {
            ring.push(format!("line {n}"));
        }
        let (revision, lines) = ring.since(0);
        assert_eq!(revision, RING_LINES as u64 + 10);
        assert_eq!(lines.len(), RING_LINES);
        assert_eq!(lines[0], "line 10");
        assert_eq!(lines[RING_LINES - 1], format!("line {}", RING_LINES + 9));
        let (_, newer) = ring.since(revision - 2);
        assert_eq!(newer.len(), 2);
    }

    #[test]
    fn the_file_rotates_and_keeps_three_old_files() {
        let dir = tempfile::tempdir().expect("a scratch folder");
        let mut file = LogFile::open(dir.path(), 100);
        for n in 0..60 {
            file.write(&format!("line number {n:03} of the rotation test"));
        }
        let names: Vec<String> = fs::read_dir(dir.path())
            .expect("the logs folder")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        for kept in [
            "launcher.log",
            "launcher.1.log",
            "launcher.2.log",
            "launcher.3.log",
        ] {
            assert!(
                names.iter().any(|n| n == kept),
                "{kept} missing in {names:?}"
            );
        }
        assert_eq!(names.len(), 4, "{names:?}");
        let newest = fs::read_to_string(dir.path().join("launcher.log")).expect("the new file");
        assert!(newest.contains("line number 059"));
        assert!(newest.len() as u64 <= 100 + 41);
    }

    #[test]
    fn a_step_dropped_without_a_result_logs_that_it_ended() {
        let before = since(0).0;
        drop(step("drop-test step alpha"));
        step("drop-test step beta").done("");
        step("drop-test step gamma").failed("no reason");
        let (_, lines) = since(before);
        let has = |text: &str| lines.iter().any(|l| l.contains(text));
        assert!(has("drop-test step alpha: ended after "));
        assert!(has("drop-test step beta: done in "));
        assert!(has("drop-test step gamma: failed after "));
        assert!(!has("drop-test step beta: ended"));
        assert!(!has("drop-test step gamma: ended"));
    }

    #[test]
    fn a_preparation_line_is_logged_without_the_players_name() {
        hide("Steve_Prepare");
        let before = since(0).0;
        line("Preparing: 12 of 4000 files for Steve_Prepare");
        let (_, lines) = since(before);
        let found: Vec<&String> = lines
            .iter()
            .filter(|l| l.contains("Preparing: 12 of 4000 files"))
            .collect();
        assert_eq!(found.len(), 1, "{lines:?}");
        assert!(!found[0].contains("Steve_Prepare"), "{found:?}");
    }

    static DETAIL_TEST_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn play_meter_reports_counted_steps_to_both_ui_and_log() {
        let _serial = lock(&DETAIL_TEST_LOCK);
        let seen = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));
        let sink_seen = seen.clone();
        let before = since(0).0;
        let _guard = on_play_detail(move |text| {
            lock(&sink_seen).push(text);
        });
        let meter = PlayMeter::new(|done, total| format!("Sealing test files: {done} of {total}"));
        meter.tick(1, 100);
        meter.tick(50, 100);
        meter.tick(100, 100);
        let ui: Vec<String> = lock(&seen)
            .iter()
            .filter(|l| l.starts_with("Sealing test files:"))
            .cloned()
            .collect();
        assert_eq!(
            ui,
            vec![
                "Sealing test files: 1 of 100".to_string(),
                "Sealing test files: 100 of 100".to_string(),
            ]
        );
        let (_, lines) = since(before);
        assert!(
            lines
                .iter()
                .any(|l| l.contains("Sealing test files: 1 of 100")),
            "{lines:?}"
        );
    }

    #[test]
    fn play_detail_from_spawned_thread_reaches_sink() {
        let _serial = lock(&DETAIL_TEST_LOCK);
        let seen = std::sync::Arc::new(Mutex::new(Vec::<String>::new()));
        let sink_seen = seen.clone();
        let _guard = on_play_detail(move |text| {
            lock(&sink_seen).push(text);
        });
        std::thread::spawn(|| {
            play_detail_ui_only("spawned-thread-test: Downloading Minecraft's files: 12 of 400");
            let meter = PlayMeter::new(|done, total| {
                format!("spawned-thread-test: Sealing worker files: {done} of {total}")
            });
            meter.tick(1, 10);
            meter.tick(10, 10);
        })
        .join()
        .expect("worker thread joined");
        let got: Vec<String> = lock(&seen)
            .iter()
            .filter(|l| l.starts_with("spawned-thread-test:"))
            .cloned()
            .collect();
        assert_eq!(
            got,
            vec![
                "spawned-thread-test: Downloading Minecraft's files: 12 of 400".to_string(),
                "spawned-thread-test: Sealing worker files: 1 of 10".to_string(),
                "spawned-thread-test: Sealing worker files: 10 of 10".to_string(),
            ]
        );
    }
}

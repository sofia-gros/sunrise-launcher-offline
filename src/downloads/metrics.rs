//! Progress counters and the snapshot the settings and CLI pages read.

use std::{
    sync::{
        atomic::{AtomicU64, Ordering::Relaxed},
        Arc, Mutex, Weak,
    },
    time::{Duration, Instant},
};

#[derive(Default)]
pub(super) struct Counters {
    pub(super) submitted: AtomicU64,
    pub(super) active: AtomicU64,
    pub(super) connections: AtomicU64,
    pub(super) completed: AtomicU64,
    pub(super) failed: AtomicU64,
    pub(super) retried: AtomicU64,
    pub(super) bytes: AtomicU64,
    pub(super) total: AtomicU64,
    pub(super) verified: AtomicU64,
    pub(super) file_micros: AtomicU64,
}
#[derive(Clone, Debug)]
pub struct Progress {
    pub active: u64,
    pub connections: u64,
    pub completed: u64,
    pub pending: u64,
    pub failed: u64,
    pub retried: u64,
    pub downloaded_bytes: u64,
    pub total_bytes: u64,
    pub verified_bytes: u64,
    pub elapsed: Duration,
    pub last_file_elapsed: Duration,
    pub mib_per_second: f64,
}
pub(super) struct Count<'a>(&'a AtomicU64);
impl<'a> Count<'a> {
    pub(super) fn new(n: &'a AtomicU64) -> Self {
        n.fetch_add(1, Relaxed);
        Self(n)
    }
}
impl Drop for Count<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Relaxed);
    }
}

pub(super) struct Metrics {
    pub(super) counters: Counters,
    pub(super) started: Instant,
}
pub(super) static VISIBLE: Mutex<Option<Weak<Metrics>>> = Mutex::new(None);
pub(super) fn progress_of(metrics: &Metrics) -> Progress {
    let c = &metrics.counters;
    let active = c.active.load(Relaxed);
    let completed = c.completed.load(Relaxed);
    let failed = c.failed.load(Relaxed);
    let bytes = c.bytes.load(Relaxed);
    let elapsed = metrics.started.elapsed();
    Progress {
        active,
        connections: c.connections.load(Relaxed),
        completed,
        failed,
        pending: c
            .submitted
            .load(Relaxed)
            .saturating_sub(active + completed + failed),
        retried: c.retried.load(Relaxed),
        downloaded_bytes: bytes,
        total_bytes: c.total.load(Relaxed),
        verified_bytes: c.verified.load(Relaxed),
        elapsed,
        last_file_elapsed: Duration::from_micros(c.file_micros.load(Relaxed)),
        mib_per_second: bytes as f64 / (1 << 20) as f64 / elapsed.as_secs_f64().max(0.001),
    }
}

pub(super) struct BatchPending {
    pub(super) metrics: Arc<Metrics>,
    pub(super) remaining: u64,
}
impl Drop for BatchPending {
    fn drop(&mut self) {
        self.metrics
            .counters
            .failed
            .fetch_add(self.remaining, Relaxed);
    }
}

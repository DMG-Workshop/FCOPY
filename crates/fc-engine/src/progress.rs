//! Progress tracking.
//!
//! [`Progress`] is a bundle of atomic counters that workers bump once per chunk
//! (cheap, lock-free) plus an optional *throttled* observer callback. The UI can
//! either poll [`Progress::snapshot`] on its own timer (best for immediate-mode
//! GUIs) or receive pushed [`Snapshot`]s at most every `min_interval`.
//!
//! "Work" is measured in bytes and counts verification too: with read-back
//! verification a 10 GB job is 20 GB of work, so the percentage and ETA stay
//! honest instead of racing to 100 % and then stalling during the verify pass.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicU8, Ordering::Relaxed};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

/// What the engine is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Phase {
    #[default]
    Idle,
    Preparing,
    Copying,
    Verifying,
    Finalizing,
}

impl Phase {
    fn from_u8(v: u8) -> Phase {
        match v {
            1 => Phase::Preparing,
            2 => Phase::Copying,
            3 => Phase::Verifying,
            4 => Phase::Finalizing,
            _ => Phase::Idle,
        }
    }
}

/// Network link telemetry, filled in by remote transfer backends.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LinkStats {
    /// `user@host:port` or similar.
    pub peer: String,
    pub connected: bool,
    pub rtt: Duration,
    /// Bytes put on the wire (including retransmission-free payload only).
    pub sent_bytes: u64,
    /// Bytes the receiver confirmed as written.
    pub acked_bytes: u64,
    pub lost_packets: u64,
    /// Congestion window in bytes.
    pub cwnd: u64,
    /// Automatic reconnects after dropouts in this job.
    pub reconnects: u32,
    /// The receiving side paused the transfer.
    pub remote_paused: bool,
}

/// A consistent-enough view of progress for display.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub phase: Phase,
    pub current_file: Option<PathBuf>,
    /// Bytes processed in the current file's *current phase*, and that phase's total.
    pub file_bytes_done: u64,
    pub file_bytes_total: u64,
    pub files_done: u64,
    pub files_skipped: u64,
    pub files_total: u64,
    pub bytes_copied: u64,
    pub bytes_verified: u64,
    /// Total bytes of source data in the job (excluding verification work).
    pub bytes_total: u64,
    /// 0.0 ..= 1.0 across copy + verify work.
    pub fraction: f64,
    /// Smoothed throughput over copy + verify work, bytes/second.
    pub rate_bps: f64,
    pub eta: Option<Duration>,
    pub elapsed: Duration,
    /// Present for remote transfers.
    pub link: Option<LinkStats>,
}

type Observer = Box<dyn Fn(&Snapshot) + Send + Sync>;

#[derive(Debug)]
struct Throttle {
    last_emit: Instant,
    last_work: u64,
    rate: f64,
}

/// Shared progress state. Create one per job and share it as `Arc<Progress>`.
pub struct Progress {
    files_total: AtomicU64,
    files_done: AtomicU64,
    files_skipped: AtomicU64,
    bytes_total: AtomicU64,
    bytes_copied: AtomicU64,
    bytes_verified: AtomicU64,
    /// Work abandoned because a file was skipped (so the bar still reaches 100 %).
    work_skipped: AtomicU64,
    verify: AtomicBool,
    /// True when the caller never declared job totals (single bare `copy_file`).
    auto_totals: AtomicBool,

    file_total: AtomicU64,
    file_done: AtomicU64,
    file_copied: AtomicU64,
    file_verified: AtomicU64,
    phase: AtomicU8,
    current: Mutex<Option<PathBuf>>,
    link: Mutex<Option<LinkStats>>,

    observer: Option<Observer>,
    min_interval: Duration,
    throttle: Mutex<Throttle>,
    started: Instant,
}

impl std::fmt::Debug for Progress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Progress").field("snapshot", &self.snapshot()).finish()
    }
}

impl Default for Progress {
    fn default() -> Self {
        Self::new()
    }
}

impl Progress {
    /// Progress with no push callback (poll with [`snapshot`](Self::snapshot)).
    pub fn new() -> Self {
        Self::build(None, Duration::from_millis(100))
    }

    /// Progress that calls `observer` at most once per `min_interval`, always
    /// from a worker thread (never while a lock is held), plus once at every
    /// phase change and file boundary. Forward into your UI's event loop.
    pub fn with_observer(
        min_interval: Duration,
        observer: impl Fn(&Snapshot) + Send + Sync + 'static,
    ) -> Self {
        Self::build(Some(Box::new(observer)), min_interval)
    }

    fn build(observer: Option<Observer>, min_interval: Duration) -> Self {
        let now = Instant::now();
        Self {
            files_total: AtomicU64::new(0),
            files_done: AtomicU64::new(0),
            files_skipped: AtomicU64::new(0),
            bytes_total: AtomicU64::new(0),
            bytes_copied: AtomicU64::new(0),
            bytes_verified: AtomicU64::new(0),
            work_skipped: AtomicU64::new(0),
            verify: AtomicBool::new(false),
            auto_totals: AtomicBool::new(false),
            file_total: AtomicU64::new(0),
            file_done: AtomicU64::new(0),
            file_copied: AtomicU64::new(0),
            file_verified: AtomicU64::new(0),
            phase: AtomicU8::new(0),
            current: Mutex::new(None),
            link: Mutex::new(None),
            observer,
            min_interval,
            throttle: Mutex::new(Throttle { last_emit: now, last_work: 0, rate: 0.0 }),
            started: now,
        }
    }

    // ----- job-level setup (called by the batch runner or your own code) -----

    /// Declare the size of the job up front so percentages and ETA are meaningful.
    pub fn set_job_totals(&self, files: u64, bytes: u64, verify: bool) {
        self.files_total.store(files, Relaxed);
        self.bytes_total.store(bytes, Relaxed);
        self.verify.store(verify, Relaxed);
    }

    /// Update network telemetry (remote backends).
    pub fn set_link(&self, link: LinkStats) {
        *self.link.lock().unwrap_or_else(PoisonError::into_inner) = Some(link);
        self.emit(false);
    }

    /// Account for a file handled outside the per-file slot, e.g. one of many
    /// small files transferred concurrently: `abandoned` is the part of its
    /// size that will never be transferred (0 if it completed).
    pub fn count_file(&self, skipped: bool, abandoned: u64) {
        self.files_done.fetch_add(1, Relaxed);
        if skipped {
            self.files_skipped.fetch_add(1, Relaxed);
            let verify = self.verify.load(Relaxed) as u64;
            self.work_skipped.fetch_add(abandoned.saturating_mul(1 + verify), Relaxed);
        }
        self.emit(false);
    }

    // ----- per-file hooks (used by the engine and by transfer backends) -----

    /// Start tracking a file. `size_hint` may be 0 if the size is not known yet;
    /// call [`set_file_size`](Self::set_file_size) once it is.
    pub fn begin_file(&self, path: &std::path::Path, size_hint: u64, verify: bool) {
        // A bare `copy_file` call with no declared totals still gets sane numbers.
        let auto = self.files_total.load(Relaxed) == 0;
        self.auto_totals.store(auto, Relaxed);
        if auto {
            self.files_total.store(1, Relaxed);
            self.bytes_total.store(size_hint, Relaxed);
            self.verify.store(verify, Relaxed);
        }
        *self.current.lock().unwrap_or_else(PoisonError::into_inner) = Some(path.to_path_buf());
        self.file_total.store(size_hint, Relaxed);
        self.file_done.store(0, Relaxed);
        self.file_copied.store(0, Relaxed);
        self.file_verified.store(0, Relaxed);
        self.set_phase(Phase::Preparing);
    }

    pub fn set_file_size(&self, size: u64) {
        self.file_total.store(size, Relaxed);
        if self.auto_totals.load(Relaxed) {
            self.bytes_total.store(size, Relaxed);
        }
    }

    pub fn set_phase(&self, phase: Phase) {
        self.phase.store(phase as u8, Relaxed);
        if phase == Phase::Verifying {
            self.file_done.store(0, Relaxed);
            self.file_total.store(self.file_copied.load(Relaxed), Relaxed);
        }
        self.emit(true);
    }

    #[inline]
    pub fn add_copied(&self, n: u64) {
        self.bytes_copied.fetch_add(n, Relaxed);
        self.file_copied.fetch_add(n, Relaxed);
        self.file_done.fetch_add(n, Relaxed);
        self.emit(false);
    }

    #[inline]
    pub fn add_verified(&self, n: u64) {
        self.bytes_verified.fetch_add(n, Relaxed);
        self.file_verified.fetch_add(n, Relaxed);
        self.file_done.fetch_add(n, Relaxed);
        self.emit(false);
    }

    /// Undo this file's contribution before a full restart, so retries don't
    /// push the percentage past 100 %.
    pub fn rewind_file(&self) {
        self.bytes_copied.fetch_sub(self.file_copied.swap(0, Relaxed), Relaxed);
        self.bytes_verified.fetch_sub(self.file_verified.swap(0, Relaxed), Relaxed);
        self.file_done.store(0, Relaxed);
        self.set_phase(Phase::Copying);
    }

    pub fn finish_file(&self) {
        self.files_done.fetch_add(1, Relaxed);
        self.set_phase(Phase::Idle);
    }

    pub fn skip_file(&self, size: u64) {
        let verify = self.verify.load(Relaxed) as u64;
        let planned = size.saturating_mul(1 + verify);
        let done = self.file_copied.load(Relaxed) + self.file_verified.load(Relaxed);
        self.work_skipped.fetch_add(planned.saturating_sub(done), Relaxed);
        self.files_skipped.fetch_add(1, Relaxed);
        self.files_done.fetch_add(1, Relaxed);
        self.set_phase(Phase::Idle);
    }

    // ----- reading -----

    fn work_total(&self) -> u64 {
        let verify = self.verify.load(Relaxed) as u64;
        self.bytes_total.load(Relaxed).saturating_mul(1 + verify)
    }

    fn work_done(&self) -> u64 {
        self.bytes_copied.load(Relaxed)
            + self.bytes_verified.load(Relaxed)
            + self.work_skipped.load(Relaxed)
    }

    fn snapshot_with_rate(&self, rate_bps: f64) -> Snapshot {
        let total = self.work_total();
        let done = self.work_done();
        let fraction = if total == 0 { 0.0 } else { (done as f64 / total as f64).min(1.0) };
        let eta = (rate_bps > 1.0 && total > done)
            .then(|| Duration::from_secs_f64((total - done) as f64 / rate_bps));
        Snapshot {
            phase: Phase::from_u8(self.phase.load(Relaxed)),
            current_file: self.current.lock().unwrap_or_else(PoisonError::into_inner).clone(),
            file_bytes_done: self.file_done.load(Relaxed),
            file_bytes_total: self.file_total.load(Relaxed),
            files_done: self.files_done.load(Relaxed),
            files_skipped: self.files_skipped.load(Relaxed),
            files_total: self.files_total.load(Relaxed),
            bytes_copied: self.bytes_copied.load(Relaxed),
            bytes_verified: self.bytes_verified.load(Relaxed),
            bytes_total: self.bytes_total.load(Relaxed),
            fraction,
            rate_bps,
            eta,
            elapsed: self.started.elapsed(),
            link: self.link.lock().unwrap_or_else(PoisonError::into_inner).clone(),
        }
    }

    /// Current state, for UIs that poll. Uses the most recently smoothed rate.
    pub fn snapshot(&self) -> Snapshot {
        let rate = self.throttle.lock().unwrap_or_else(PoisonError::into_inner).rate;
        self.snapshot_with_rate(rate)
    }

    /// Update the smoothed rate and, if due (or `force`), call the observer.
    fn emit(&self, force: bool) {
        let Some(observer) = &self.observer else { return };
        // Never block a copy worker on the UI's bookkeeping: if another thread is
        // already emitting, skip this tick.
        let Ok(mut t) = self.throttle.try_lock() else { return };
        let now = Instant::now();
        let dt = now.duration_since(t.last_emit);
        if !force && dt < self.min_interval {
            return;
        }
        let work = self.work_done();
        if dt >= Duration::from_millis(20) {
            let inst = work.saturating_sub(t.last_work) as f64 / dt.as_secs_f64();
            // Exponentially weighted moving average; first sample seeds it.
            t.rate = if t.rate == 0.0 { inst } else { 0.3 * inst + 0.7 * t.rate };
            t.last_emit = now;
            t.last_work = work;
        }
        let rate = t.rate;
        drop(t); // never call user code while holding our lock
        observer(&self.snapshot_with_rate(rate));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::Arc;

    #[test]
    fn fraction_counts_verify_work() {
        let p = Progress::new();
        p.set_job_totals(1, 1000, true);
        p.begin_file(Path::new("a"), 1000, true);
        p.add_copied(1000);
        assert!((p.snapshot().fraction - 0.5).abs() < 1e-9);
        p.set_phase(Phase::Verifying);
        p.add_verified(1000);
        p.finish_file();
        assert!((p.snapshot().fraction - 1.0).abs() < 1e-9);
        assert_eq!(p.snapshot().files_done, 1);
    }

    #[test]
    fn skipping_still_reaches_100_percent() {
        let p = Progress::new();
        p.set_job_totals(2, 2000, true);
        p.begin_file(Path::new("a"), 1000, true);
        p.add_copied(300);
        p.skip_file(1000);
        p.begin_file(Path::new("b"), 1000, true);
        p.add_copied(1000);
        p.set_phase(Phase::Verifying);
        p.add_verified(1000);
        p.finish_file();
        let s = p.snapshot();
        assert!((s.fraction - 1.0).abs() < 1e-9, "{}", s.fraction);
        assert_eq!((s.files_done, s.files_skipped), (2, 1));
    }

    #[test]
    fn rewind_removes_the_files_contribution() {
        let p = Progress::new();
        p.set_job_totals(1, 1000, true);
        p.begin_file(Path::new("a"), 1000, true);
        p.add_copied(1000);
        p.set_phase(Phase::Verifying);
        p.add_verified(400);
        p.rewind_file();
        let s = p.snapshot();
        assert_eq!((s.bytes_copied, s.bytes_verified), (0, 0));
        assert_eq!(s.phase, Phase::Copying);
    }

    #[test]
    fn observer_is_throttled_but_sees_phase_changes() {
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let c2 = count.clone();
        let p = Progress::with_observer(Duration::from_secs(3600), move |_| {
            c2.fetch_add(1, Relaxed);
        });
        p.begin_file(Path::new("a"), 10, false); // forced (phase change)
        for _ in 0..1000 {
            p.add_copied(1); // all throttled away
        }
        p.finish_file(); // forced
        assert_eq!(count.load(Relaxed), 2);
    }
}

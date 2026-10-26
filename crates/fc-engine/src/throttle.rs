//! Speed limiter: a token bucket shared by every worker.
//!
//! * One `Arc<RateLimiter>` caps the *combined* throughput of all transfers.
//! * The limit can be changed (or removed) at any time from the UI thread.
//!   Sleeping workers notice within one slice (≤ 50 ms) and re-plan.
//! * "Debt" accounting: a worker may take a whole chunk even when the bucket
//!   holds less, and then sleeps until the debt is repaid. Chunk size never has
//!   to be tuned to the limit, and the long-run rate is exact.
//! * The bucket holds at most `burst` worth of tokens (¼ s at the current rate),
//!   so an idle period can't be followed by an unthrottled burst.
//! * Unlimited is the fast path: one atomic load, no lock.
//!
//! Sleeps are interruptible: cancel and skip end them immediately. Pause just
//! stops the worker at its next checkpoint.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::{Duration, Instant};

use crate::control::JobControl;

const SLICE: Duration = Duration::from_millis(50);
const BURST_SECONDS: f64 = 0.25;

#[derive(Debug)]
struct Bucket {
    tokens: f64,
    last: Instant,
}

/// Shared, runtime-adjustable bandwidth cap (bytes per second).
#[derive(Debug)]
pub struct RateLimiter {
    /// 0 = unlimited.
    limit: AtomicU64,
    /// Bumped on every `set_limit`, so sleepers re-plan.
    generation: AtomicU64,
    bucket: Mutex<Bucket>,
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::new(None)
    }
}

impl RateLimiter {
    pub fn new(bytes_per_sec: Option<u64>) -> Self {
        Self {
            limit: AtomicU64::new(bytes_per_sec.unwrap_or(0)),
            generation: AtomicU64::new(0),
            bucket: Mutex::new(Bucket { tokens: 0.0, last: Instant::now() }),
        }
    }

    /// Current limit in bytes/second, `None` if unlimited.
    pub fn limit(&self) -> Option<u64> {
        match self.limit.load(Ordering::Relaxed) {
            0 => None,
            n => Some(n),
        }
    }

    /// Change the limit; `None` (or `Some(0)`) removes it. Takes effect within ~50 ms.
    pub fn set_limit(&self, bytes_per_sec: Option<u64>) {
        let mut b = self.bucket.lock().unwrap_or_else(PoisonError::into_inner);
        // Start the new regime from an empty bucket: no debt from a slower
        // limit carries over, and no stored credit from a faster one.
        b.tokens = 0.0;
        b.last = Instant::now();
        self.limit.store(bytes_per_sec.unwrap_or(0), Ordering::Relaxed);
        self.generation.fetch_add(1, Ordering::AcqRel);
    }

    /// Non-blocking form for async code: account for `bytes` now and return
    /// how long the caller should wait before sending them (zero if unlimited
    /// or within budget). Async callers sleep on their own timer.
    pub fn reserve(&self, bytes: u64) -> Duration {
        let rate = self.limit.load(Ordering::Relaxed);
        if rate == 0 || bytes == 0 {
            return Duration::ZERO;
        }
        let mut b = self.bucket.lock().unwrap_or_else(PoisonError::into_inner);
        let rate_f = rate as f64;
        let now = Instant::now();
        b.tokens = (b.tokens + now.duration_since(b.last).as_secs_f64() * rate_f).min(rate_f * BURST_SECONDS);
        b.last = now;
        b.tokens -= bytes as f64;
        if b.tokens >= 0.0 { Duration::ZERO } else { Duration::from_secs_f64(-b.tokens / rate_f) }
    }

    /// Block until `bytes` may be transferred. Returns early (without
    /// consuming the full wait) if the job is cancelled or a skip is requested;
    /// the caller's next checkpoint then acts on it.
    pub fn acquire(&self, bytes: u64, control: &JobControl) {
        loop {
            let rate = self.limit.load(Ordering::Relaxed);
            if rate == 0 || bytes == 0 {
                return;
            }
            let generation = self.generation.load(Ordering::Acquire);
            let wait = {
                let mut b = self.bucket.lock().unwrap_or_else(PoisonError::into_inner);
                let rate_f = rate as f64;
                let now = Instant::now();
                let burst = rate_f * BURST_SECONDS;
                b.tokens = (b.tokens + now.duration_since(b.last).as_secs_f64() * rate_f).min(burst);
                b.last = now;
                b.tokens -= bytes as f64;
                if b.tokens >= 0.0 { Duration::ZERO } else { Duration::from_secs_f64(-b.tokens / rate_f) }
            };
            if wait.is_zero() {
                return;
            }
            // Sleep off the debt in slices so cancel/skip/limit changes are responsive.
            let deadline = Instant::now() + wait;
            let mut replan = false;
            loop {
                if control.is_cancelled() || control.skip_requested() {
                    return;
                }
                if self.generation.load(Ordering::Acquire) != generation {
                    replan = true; // limit changed: our debt was forgiven, ask again
                    break;
                }
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    break;
                }
                std::thread::sleep(left.min(SLICE));
            }
            if !replan {
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn pump(l: &RateLimiter, c: &JobControl, total: u64, chunk: u64) {
        let mut sent = 0;
        while sent < total {
            l.acquire(chunk, c);
            sent += chunk;
        }
    }

    #[test]
    fn reserve_reports_the_wait_without_sleeping() {
        let l = RateLimiter::new(Some(1_000_000));
        assert!(l.reserve(0).is_zero());
        let w = l.reserve(500_000).as_secs_f64();
        assert!((0.45..0.55).contains(&w), "{w}");
        assert!(RateLimiter::new(None).reserve(u64::MAX).is_zero());
    }

    #[test]
    fn unlimited_never_blocks() {
        let l = RateLimiter::new(None);
        let c = JobControl::new();
        let t = Instant::now();
        pump(&l, &c, 10 << 30, 1 << 20);
        assert!(t.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn caps_long_run_rate() {
        // 10 MB/s, 3 MB in 64 KiB chunks ≈ 0.3 s
        let l = RateLimiter::new(Some(10_000_000));
        let c = JobControl::new();
        let t = Instant::now();
        pump(&l, &c, 3_000_000, 65_536);
        let s = t.elapsed().as_secs_f64();
        assert!((0.25..0.45).contains(&s), "took {s}s");
    }

    #[test]
    fn chunks_larger_than_the_bucket_still_obey_the_rate() {
        let l = RateLimiter::new(Some(4_000_000));
        let c = JobControl::new();
        let t = Instant::now();
        pump(&l, &c, 2 * 1_048_576, 1_048_576); // 2 MiB at 4 MB/s ≈ 0.52 s
        let s = t.elapsed().as_secs_f64();
        assert!((0.45..0.7).contains(&s), "took {s}s");
    }

    #[test]
    fn limit_is_shared_between_threads() {
        let l = Arc::new(RateLimiter::new(Some(10_000_000)));
        let c = Arc::new(JobControl::new());
        let t = Instant::now();
        let hs: Vec<_> = (0..3)
            .map(|_| {
                let (l, c) = (l.clone(), c.clone());
                std::thread::spawn(move || pump(&l, &c, 1_000_000, 65_536))
            })
            .collect();
        hs.into_iter().for_each(|h| h.join().unwrap());
        let s = t.elapsed().as_secs_f64();
        assert!((0.25..0.45).contains(&s), "3 MB total at 10 MB/s took {s}s");
    }

    #[test]
    fn raising_the_limit_wakes_sleepers() {
        let l = Arc::new(RateLimiter::new(Some(1_000))); // absurdly slow
        let c = Arc::new(JobControl::new());
        let (l2, c2) = (l.clone(), c.clone());
        let t = Instant::now();
        let h = std::thread::spawn(move || l2.acquire(1_000_000, &c2)); // ~1000 s of debt
        std::thread::sleep(Duration::from_millis(100));
        l.set_limit(None);
        h.join().unwrap();
        assert!(t.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn cancel_and_skip_interrupt_the_wait() {
        let l = Arc::new(RateLimiter::new(Some(1_000)));
        for skip in [false, true] {
            let c = Arc::new(JobControl::new());
            let (l2, c2) = (l.clone(), c.clone());
            let h = std::thread::spawn(move || l2.acquire(1_000_000, &c2));
            std::thread::sleep(Duration::from_millis(60));
            if skip { c.request_skip() } else { c.cancel() }
            let t = Instant::now();
            h.join().unwrap();
            assert!(t.elapsed() < Duration::from_millis(200));
        }
    }
}

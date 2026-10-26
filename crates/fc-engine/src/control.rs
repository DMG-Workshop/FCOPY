//! Cooperative pause / resume / cancel.
//!
//! Workers call [`JobControl::checkpoint`] once per chunk (≤ 4 MiB, i.e. a few
//! milliseconds of I/O), so pause, cancel and skip take effect almost
//! instantly. The fast path is a single atomic load; only a paused job takes
//! the lock and sleeps on a condvar, so the controls cost nothing while running.
//!
//! Pausing never closes files or discards buffers: the reader and writer
//! simply stop at their next chunk boundary with all state intact, and
//! [`JobControl::resume`] continues from the exact byte.
//!
//! *Skip* is a one-shot request: the file currently being copied is abandoned
//! (its partial output removed) and the job moves on to the next file.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};

const RUNNING: u8 = 0;
const PAUSED: u8 = 1;
const CANCELLED: u8 = 2;

/// Marker returned by [`JobControl::checkpoint`] when the job was cancelled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cancelled;

/// Shared run-state for a job. Wrap in `Arc` to share between the UI and workers.
#[derive(Debug, Default)]
pub struct JobControl {
    state: AtomicU8,
    skip: AtomicBool,
    lock: Mutex<()>,
    wake: Condvar,
}

impl JobControl {
    pub fn new() -> Self {
        Self::default()
    }

    fn guard(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Pause at the next chunk boundary. No effect once cancelled.
    pub fn pause(&self) {
        let _g = self.guard();
        let _ = self
            .state
            .compare_exchange(RUNNING, PAUSED, Ordering::AcqRel, Ordering::Acquire);
    }

    /// Resume a paused job.
    pub fn resume(&self) {
        let _g = self.guard();
        let _ = self
            .state
            .compare_exchange(PAUSED, RUNNING, Ordering::AcqRel, Ordering::Acquire);
        self.wake.notify_all();
    }

    /// Cancel. Wakes paused workers so they can exit. Irreversible.
    pub fn cancel(&self) {
        let _g = self.guard();
        self.state.store(CANCELLED, Ordering::Release);
        self.wake.notify_all();
    }

    pub fn is_cancelled(&self) -> bool {
        self.state.load(Ordering::Acquire) == CANCELLED
    }

    pub fn is_paused(&self) -> bool {
        self.state.load(Ordering::Acquire) == PAUSED
    }

    /// Abandon the file currently being copied (even while paused).
    pub fn request_skip(&self) {
        let _g = self.guard();
        self.skip.store(true, Ordering::Release);
        self.wake.notify_all();
    }

    /// Is a skip pending? (Does not consume it.)
    pub fn skip_requested(&self) -> bool {
        self.skip.load(Ordering::Acquire)
    }

    /// Consume a pending skip request.
    pub fn take_skip(&self) -> bool {
        self.skip.swap(false, Ordering::AcqRel)
    }

    /// Forget a stale skip request (called when a new file starts).
    pub fn clear_skip(&self) {
        self.skip.store(false, Ordering::Release);
    }

    /// Call at chunk boundaries: returns `Err(Cancelled)` if the job was
    /// cancelled and blocks while it is paused. A skip request wakes a paused
    /// worker (returning `Ok`) so the caller can act on it.
    #[inline]
    pub fn checkpoint(&self) -> Result<(), Cancelled> {
        match self.state.load(Ordering::Acquire) {
            RUNNING => Ok(()),
            CANCELLED => Err(Cancelled),
            _ => self.checkpoint_slow(),
        }
    }

    #[cold]
    fn checkpoint_slow(&self) -> Result<(), Cancelled> {
        let mut g = self.guard();
        loop {
            match self.state.load(Ordering::Acquire) {
                RUNNING => return Ok(()),
                CANCELLED => return Err(Cancelled),
                _ if self.skip.load(Ordering::Acquire) => return Ok(()),
                // The state is only changed while holding `lock`, so a wake-up
                // cannot be lost between this check and the wait.
                _ => g = self.wake.wait(g).unwrap_or_else(PoisonError::into_inner),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn pause_blocks_until_resume() {
        let c = Arc::new(JobControl::new());
        c.pause();
        let c2 = Arc::clone(&c);
        let t = std::thread::spawn(move || c2.checkpoint());
        std::thread::sleep(Duration::from_millis(50));
        assert!(!t.is_finished());
        c.resume();
        assert_eq!(t.join().unwrap(), Ok(()));
    }

    #[test]
    fn cancel_wakes_paused_worker() {
        let c = Arc::new(JobControl::new());
        c.pause();
        let c2 = Arc::clone(&c);
        let t = std::thread::spawn(move || c2.checkpoint());
        std::thread::sleep(Duration::from_millis(50));
        c.cancel();
        assert_eq!(t.join().unwrap(), Err(Cancelled));
        // cancel is sticky
        c.resume();
        assert!(c.is_cancelled());
    }

    #[test]
    fn skip_wakes_a_paused_worker_and_is_one_shot() {
        let c = Arc::new(JobControl::new());
        c.pause();
        let c2 = Arc::clone(&c);
        let t = std::thread::spawn(move || (c2.checkpoint(), c2.take_skip()));
        std::thread::sleep(Duration::from_millis(50));
        assert!(!t.is_finished());
        c.request_skip();
        assert_eq!(t.join().unwrap(), (Ok(()), true));
        assert!(!c.take_skip(), "skip is consumed");
        assert!(c.is_paused(), "skipping does not resume the job");
    }
}

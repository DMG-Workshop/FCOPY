//! Error recovery: **Retry / Skip / Cancel**.
//!
//! Every fallible I/O step in the engine runs through [`Recovery::run`]. When a
//! step fails, the failure is classified ([`ErrorClass`]) and handed to your
//! [`ErrorHandler`], which answers with an [`ErrorAction`]:
//!
//! * [`ErrorAction::Retry`]  – run the step again (after the handler's delay).
//! * [`ErrorAction::Skip`]   – abandon *this file*, delete its partial output, carry on.
//! * [`ErrorAction::Cancel`] – stop the whole job.
//!
//! The engine never talks to a UI. A GUI implements `ErrorHandler` by sending
//! the question to the UI thread and blocking on the reply (the worker is
//! already on its own thread); a CLI prompts on stdin; unattended runs use
//! [`AutoHandler`].
//!
//! Handlers may be called from several worker threads at once, hence
//! `Send + Sync`. An interactive handler that wants "apply to all" keeps that
//! state behind a `Mutex`.

use std::error::Error as StdError;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use crate::control::JobControl;
use crate::error::{classify, CopyError, ErrorClass, Operation};

/// What to do about a failed operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorAction {
    Retry,
    Skip,
    Cancel,
}

/// Everything a handler (or a dialog box) needs to know about a failure.
#[derive(Debug)]
pub struct ErrorContext<'a> {
    /// File the failing operation was working on.
    pub path: &'a Path,
    pub operation: Operation,
    pub class: ErrorClass,
    /// The underlying error (an `io::Error` or a [`CopyError`]); `Display` it for the UI.
    pub error: &'a (dyn StdError + 'static),
    /// Byte offset within the file, for read/write errors.
    pub offset: Option<u64>,
    /// 1 for the first failure of this operation, 2 for the second, …
    pub attempt: u32,
}

/// Decides how to recover from failures. See the module docs.
pub trait ErrorHandler: Send + Sync {
    /// Choose Retry, Skip or Cancel. May block (e.g. waiting for the user).
    fn on_error(&self, ctx: &ErrorContext<'_>) -> ErrorAction;

    /// How long the engine waits before honouring a `Retry`. The wait is
    /// cancellation-aware. Interactive handlers keep the default (no delay).
    fn retry_delay(&self, _ctx: &ErrorContext<'_>) -> Duration {
        Duration::ZERO
    }
}

/// Exponential back-off retry limits for [`AutoHandler`].
#[derive(Debug, Clone)]
pub struct RetryPolicy {
    /// Retries for locked / busy files, which usually clear by themselves.
    pub max_retries: u32,
    /// Retries for device I/O errors. Kept low: hammering a failing disk can
    /// stall for seconds per attempt and make the failure worse.
    pub max_bad_sector_retries: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 5,
            max_bad_sector_retries: 2,
            base_delay: Duration::from_millis(250),
            max_delay: Duration::from_secs(8),
        }
    }
}

impl RetryPolicy {
    /// Delay before retry number `attempt` (1-based): `base * 2^(attempt-1)`, capped.
    pub fn delay(&self, attempt: u32) -> Duration {
        let shift = attempt.saturating_sub(1).min(16);
        self.base_delay.saturating_mul(1u32 << shift).min(self.max_delay)
    }
}

/// Sensible unattended policy:
///
/// | Class              | Behaviour |
/// |--------------------|-----------|
/// | Locked, Other, Mismatch | retry with back-off, then skip the file |
/// | BadSector          | retry a couple of times, then skip the file |
/// | PermissionDenied, NotFound, AlreadyExists | skip the file (retrying won't help) |
/// | DiskFull           | cancel the job (every later file would fail too) |
#[derive(Debug, Clone, Default)]
pub struct AutoHandler {
    pub policy: RetryPolicy,
}

impl AutoHandler {
    pub fn new(policy: RetryPolicy) -> Self {
        Self { policy }
    }
}

impl ErrorHandler for AutoHandler {
    fn on_error(&self, ctx: &ErrorContext<'_>) -> ErrorAction {
        match ctx.class {
            ErrorClass::DiskFull => ErrorAction::Cancel,
            ErrorClass::PermissionDenied | ErrorClass::NotFound | ErrorClass::AlreadyExists => {
                ErrorAction::Skip
            }
            ErrorClass::BadSector => {
                if ctx.attempt <= self.policy.max_bad_sector_retries {
                    ErrorAction::Retry
                } else {
                    ErrorAction::Skip
                }
            }
            ErrorClass::Locked | ErrorClass::Mismatch | ErrorClass::Other => {
                if ctx.attempt <= self.policy.max_retries {
                    ErrorAction::Retry
                } else {
                    ErrorAction::Skip
                }
            }
        }
    }

    fn retry_delay(&self, ctx: &ErrorContext<'_>) -> Duration {
        self.policy.delay(ctx.attempt)
    }
}

/// Skip every failing file, never retry.
#[derive(Debug, Clone, Copy, Default)]
pub struct SkipOnError;

impl ErrorHandler for SkipOnError {
    fn on_error(&self, _: &ErrorContext<'_>) -> ErrorAction {
        ErrorAction::Skip
    }
}

/// Abort the whole job on the first error.
#[derive(Debug, Clone, Copy, Default)]
pub struct CancelOnError;

impl ErrorHandler for CancelOnError {
    fn on_error(&self, _: &ErrorContext<'_>) -> ErrorAction {
        ErrorAction::Cancel
    }
}

/// Adapter so a closure can be a handler: `FnHandler(|ctx| ErrorAction::Skip)`.
pub struct FnHandler<F>(pub F);

impl<F> ErrorHandler for FnHandler<F>
where
    F: Fn(&ErrorContext<'_>) -> ErrorAction + Send + Sync,
{
    fn on_error(&self, ctx: &ErrorContext<'_>) -> ErrorAction {
        (self.0)(ctx)
    }
}

// ---------------------------------------------------------------------------
// Internal machinery
// ---------------------------------------------------------------------------

/// Why a step stopped the current file. Internal control flow, not public API.
#[derive(Debug)]
pub(crate) enum Abort {
    /// Handler said Skip: drop this file, continue the job.
    Skip(CopyError),
    /// Handler said Cancel, or the job was cancelled.
    Cancel,
    /// Unrecoverable and not negotiable (e.g. destination exists).
    Fatal(CopyError),
}

pub(crate) type Step<T> = Result<T, Abort>;

enum Verdict {
    Retry(Duration),
    Skip,
    Cancel,
}

/// Wraps the user's handler plus the job control for one `copy_file` call.
pub(crate) struct Recovery<'a> {
    pub handler: &'a dyn ErrorHandler,
    pub control: &'a JobControl,
    /// Number of retries performed (reported to the caller).
    pub retries: AtomicU32,
}

impl<'a> Recovery<'a> {
    pub fn new(handler: &'a dyn ErrorHandler, control: &'a JobControl) -> Self {
        Self { handler, control, retries: AtomicU32::new(0) }
    }

    pub fn retries(&self) -> u32 {
        self.retries.load(Ordering::Relaxed)
    }

    fn verdict(
        &self,
        path: &Path,
        operation: Operation,
        offset: Option<u64>,
        class: ErrorClass,
        error: &(dyn StdError + 'static),
        attempt: u32,
    ) -> Verdict {
        let ctx = ErrorContext { path, operation, class, error, offset, attempt };
        match self.handler.on_error(&ctx) {
            ErrorAction::Retry => Verdict::Retry(self.handler.retry_delay(&ctx)),
            ErrorAction::Skip => Verdict::Skip,
            ErrorAction::Cancel => Verdict::Cancel,
        }
    }

    /// Ask the handler about a *file-level* failure that is not a plain
    /// `io::Error` from a retryable step (checksum mismatch, source changed,
    /// failed fsync). `Ok(())` means "restart the whole file".
    pub fn ask_restart(&self, path: &Path, operation: Operation, err: CopyError, attempt: u32) -> Step<()> {
        match self.verdict(path, operation, None, err.class(), &err, attempt) {
            Verdict::Retry(delay) => {
                self.retries.fetch_add(1, Ordering::Relaxed);
                self.sleep(delay)?;
                Ok(())
            }
            Verdict::Skip => Err(Abort::Skip(err)),
            Verdict::Cancel => Err(Abort::Cancel),
        }
    }

    /// Run `f`, consulting the handler on failure.
    ///
    /// `f` receives the number of previous *calls* (0 on the first). Steps that
    /// keep a file position must use it to re-seek before retrying, because a
    /// failed read/write may have left the cursor anywhere.
    pub fn run<T>(
        &self,
        path: &Path,
        operation: Operation,
        offset: Option<u64>,
        mut f: impl FnMut(u32) -> io::Result<T>,
    ) -> Step<T> {
        let mut calls = 0u32;
        let mut failures = 0u32;
        loop {
            let result = f(calls);
            calls += 1;
            let err = match result {
                Ok(v) => return Ok(v),
                // EINTR is not a real failure; just go again without bothering the user.
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => e,
            };
            if self.control.is_cancelled() {
                return Err(Abort::Cancel);
            }
            failures += 1;
            match self.verdict(path, operation, offset, classify(&err), &err, failures) {
                Verdict::Retry(delay) => {
                    self.retries.fetch_add(1, Ordering::Relaxed);
                    self.sleep(delay)?;
                }
                Verdict::Skip => return Err(Abort::Skip(CopyError::io(operation, path, err))),
                Verdict::Cancel => return Err(Abort::Cancel),
            }
        }
    }

    /// Sleep for `d`, waking early if the job is cancelled or the file skipped.
    fn sleep(&self, d: Duration) -> Step<()> {
        let deadline = Instant::now() + d;
        loop {
            if self.control.is_cancelled() {
                return Err(Abort::Cancel);
            }
            if self.control.take_skip() {
                return Err(Abort::Skip(CopyError::SkippedByUser));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Ok(());
            }
            std::thread::sleep(left.min(Duration::from_millis(50)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::sync::atomic::AtomicUsize;

    fn io_err(kind: io::ErrorKind) -> io::Error {
        io::Error::new(kind, "boom")
    }

    fn quick() -> AutoHandler {
        AutoHandler::new(RetryPolicy {
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
            ..RetryPolicy::default()
        })
    }

    #[test]
    fn retry_until_success() {
        let h = quick();
        let c = JobControl::new();
        let r = Recovery::new(&h, &c);
        let n = Cell::new(0);
        let out = r.run(Path::new("x"), Operation::Read, Some(0), |_| {
            n.set(n.get() + 1);
            if n.get() < 3 { Err(io_err(io::ErrorKind::ResourceBusy)) } else { Ok(42) }
        });
        assert!(matches!(out, Ok(42)));
        assert_eq!(n.get(), 3);
        assert_eq!(r.retries(), 2);
    }

    #[test]
    fn closure_sees_call_count_for_reseeking() {
        let h = quick();
        let c = JobControl::new();
        let r = Recovery::new(&h, &c);
        let seen = std::cell::RefCell::new(vec![]);
        let _ = r.run(Path::new("x"), Operation::Write, None, |attempt| {
            seen.borrow_mut().push(attempt);
            if attempt < 2 { Err(io_err(io::ErrorKind::ResourceBusy)) } else { Ok(()) }
        });
        assert_eq!(*seen.borrow(), vec![0, 1, 2]);
    }

    #[test]
    fn permission_denied_is_skipped_immediately() {
        let h = quick();
        let c = JobControl::new();
        let r = Recovery::new(&h, &c);
        let n = Cell::new(0);
        let out: Step<()> = r.run(Path::new("x"), Operation::OpenSource, None, |_| {
            n.set(n.get() + 1);
            Err(io_err(io::ErrorKind::PermissionDenied))
        });
        assert!(matches!(out, Err(Abort::Skip(_))));
        assert_eq!(n.get(), 1);
    }

    #[test]
    fn locked_file_gives_up_after_max_retries() {
        let h = quick(); // max_retries = 5
        let c = JobControl::new();
        let r = Recovery::new(&h, &c);
        let n = Cell::new(0);
        let out: Step<()> = r.run(Path::new("x"), Operation::OpenSource, None, |_| {
            n.set(n.get() + 1);
            Err(io_err(io::ErrorKind::ResourceBusy))
        });
        assert!(matches!(out, Err(Abort::Skip(_))));
        assert_eq!(n.get(), 6); // first try + 5 retries
    }

    #[test]
    fn disk_full_cancels() {
        let h = quick();
        let c = JobControl::new();
        let r = Recovery::new(&h, &c);
        let out: Step<()> = r.run(Path::new("x"), Operation::Write, Some(10), |_| {
            Err(io_err(io::ErrorKind::StorageFull))
        });
        assert!(matches!(out, Err(Abort::Cancel)));
    }

    #[test]
    fn interrupted_is_transparent() {
        let h = CancelOnError; // would cancel on any *real* error
        let c = JobControl::new();
        let r = Recovery::new(&h, &c);
        let n = Cell::new(0);
        let out = r.run(Path::new("x"), Operation::Read, None, |_| {
            n.set(n.get() + 1);
            if n.get() < 4 { Err(io_err(io::ErrorKind::Interrupted)) } else { Ok(()) }
        });
        assert!(out.is_ok());
        assert_eq!(r.retries(), 0);
    }

    #[test]
    fn user_choices_are_honoured_and_context_is_populated() {
        let calls = AtomicUsize::new(0);
        let h = FnHandler(|ctx: &ErrorContext<'_>| {
            assert_eq!(ctx.operation, Operation::Read);
            assert_eq!(ctx.offset, Some(4096));
            assert_eq!(ctx.class, ErrorClass::NotFound);
            // Retry twice, then Skip.
            if calls.fetch_add(1, Ordering::SeqCst) < 2 { ErrorAction::Retry } else { ErrorAction::Skip }
        });
        let c = JobControl::new();
        let r = Recovery::new(&h, &c);
        let out: Step<()> = r.run(Path::new("x"), Operation::Read, Some(4096), |_| {
            Err(io_err(io::ErrorKind::NotFound))
        });
        assert!(matches!(out, Err(Abort::Skip(_))));
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn cancel_interrupts_backoff_sleep() {
        let h = AutoHandler::new(RetryPolicy {
            base_delay: Duration::from_secs(30),
            max_delay: Duration::from_secs(30),
            ..RetryPolicy::default()
        });
        let c = std::sync::Arc::new(JobControl::new());
        let c2 = c.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            c2.cancel();
        });
        let r = Recovery::new(&h, &c);
        let t = Instant::now();
        let out: Step<()> = r.run(Path::new("x"), Operation::Read, None, |_| {
            Err(io_err(io::ErrorKind::ResourceBusy))
        });
        assert!(matches!(out, Err(Abort::Cancel)));
        assert!(t.elapsed() < Duration::from_secs(5));
    }

    #[test]
    fn backoff_doubles_then_caps() {
        let p = RetryPolicy {
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_millis(500),
            ..RetryPolicy::default()
        };
        let ms: Vec<u128> = (1..=5).map(|a| p.delay(a).as_millis()).collect();
        assert_eq!(ms, vec![100, 200, 400, 500, 500]);
    }

    #[cfg(unix)]
    #[test]
    fn os_codes_are_classified() {
        let e = |c| classify(&io::Error::from_raw_os_error(c));
        assert_eq!(e(libc::EIO), ErrorClass::BadSector);
        assert_eq!(e(libc::ENOSPC), ErrorClass::DiskFull);
        assert_eq!(e(libc::EDQUOT), ErrorClass::DiskFull);
        assert_eq!(e(libc::EBUSY), ErrorClass::Locked);
        assert_eq!(e(libc::ETXTBSY), ErrorClass::Locked);
        assert_eq!(e(libc::EACCES), ErrorClass::PermissionDenied);
        assert_eq!(e(libc::EROFS), ErrorClass::PermissionDenied);
        assert_eq!(e(libc::ENOENT), ErrorClass::NotFound);
    }
}

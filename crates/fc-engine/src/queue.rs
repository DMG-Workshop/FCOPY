//! Transfer queue manager: the layer every front end (CLI, GUI, remote agent)
//! talks to.
//!
//! # Concurrency model
//!
//! ```text
//!   UI / CLI thread(s)                      worker thread (one per Queue)
//!   ──────────────────                      ─────────────────────────────
//!   queue.add(req) ─┐                        loop {
//!   queue.pause()   │  Mutex<QState>           wait on Condvar until a Pending job
//!   queue.cancel(id)├─────────────────────▶    and !paused  (or shutdown)
//!   queue.jobs()    │  + Condvar "wake"        plan → copy (engine; its own
//!                   ┘                          reader thread per file)
//!                                            }
//!   events: Receiver<Event> ◀──── mpsc ────── Progress observer (≤ 1 per interval),
//!                                             per-file results, decisions, state changes
//! ```
//!
//! * **Commands go through shared state, events come back on a channel.**
//!   A command channel would make "reorder / remove a pending job" and
//!   "show me the queue" awkward. Queue state behind one mutex is simple, and
//!   the lock is only ever held for bookkeeping, never across I/O or user code.
//! * **Every job owns a [`JobControl`].** Pause/resume/skip act on the active
//!   job's control; cancel works on any job; the engine checks the control at
//!   every chunk boundary, so a pause takes effect within one chunk and resume
//!   continues from the exact byte with buffers and handles intact.
//! * **One [`RateLimiter`] for the whole queue**, adjustable live.
//! * **Telemetry is pushed, throttled.** The engine's [`Progress`] observer
//!   fires at most once per `telemetry_interval` (default 100 ms), so the UI
//!   gets ~10 updates/s regardless of file count or speed. The latest
//!   snapshot is also kept in [`JobInfo::telemetry`] for UIs that poll.
//! * **Errors that need a human** become [`Event::Decision`] carrying a
//!   one-shot reply channel; the worker blocks on it (the UI never blocks).
//!
//! A queue runs one job at a time. Each file inside it is already pipelined;
//! parallel jobs would contend for the same disks, which is rarely faster.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use crate::batch::{plan_copy_with, plan_file_to, BatchReport, FileEvent, Plan, PlanOptions};
use crate::control::JobControl;
use crate::copy::{Copier, CopyOptions};
use crate::error::CopyError;
use crate::error::{ErrorClass, Operation};
use crate::progress::{Progress, Snapshot};
use crate::recovery::{AutoHandler, ErrorAction, ErrorContext, ErrorHandler, RetryPolicy};
use crate::throttle::RateLimiter;
use crate::conflict::{Conflict, ConflictAction, ConflictChoice, ConflictResolver};

pub type JobId = u64;

/// What a [`TransferBackend`] gets for one job.
pub struct BackendContext<'a> {
    pub progress: &'a Progress,
    pub control: &'a JobControl,
    /// The job's options: verify mode, algorithm, overwrite, shared rate limiter.
    pub options: &'a CopyOptions,
}

/// A transfer involving a backend (e.g. a remote agent in either direction).
///
/// By default the queue plans local sources with destination paths *relative*
/// (`photos/2026/a.jpg`). Download backends override `plan` to enumerate remote
/// sources and choose local destinations. The backend reports
/// per-file results through `on_file` and progress/link telemetry through
/// `ctx.progress`, and must honour `ctx.control` (pause, cancel, skip), so remote
/// jobs look exactly like local ones to every front end.
pub trait TransferBackend: Send + Sync {
    /// Human-readable destination, e.g. `alice@nas:/backup/photos`.
    fn describe(&self) -> String;

    /// Plan on the queue worker. Remote sources can override this without
    /// introducing networking or an async runtime into the engine.
    fn plan(&self, sources: &[PathBuf], options: &PlanOptions, _control: &JobControl) -> Result<Plan, CopyError> {
        plan_copy_with(sources, std::path::Path::new(""), options)
            .map_err(|e| CopyError::Backend(e.to_string()))
    }

    fn run(&self, plan: &Plan, ctx: &BackendContext<'_>, on_file: &mut dyn FnMut(FileEvent<'_>)) -> Result<BatchReport, CopyError>;
}

/// Where a transfer goes.
#[derive(Clone)]
pub enum Target {
    /// Copy each source *into* this directory.
    IntoDir(PathBuf),
    /// Copy a single file to exactly this path (new name allowed).
    FileTo(PathBuf),
    /// Copy each source into a backend-defined destination (remote PC, …).
    Backend(Arc<dyn TransferBackend>),
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::IntoDir(p) => f.debug_tuple("IntoDir").field(p).finish(),
            Target::FileTo(p) => f.debug_tuple("FileTo").field(p).finish(),
            Target::Backend(b) => f.debug_tuple("Backend").field(&b.describe()).finish(),
        }
    }
}

impl PartialEq for Target {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Target::IntoDir(a), Target::IntoDir(b)) | (Target::FileTo(a), Target::FileTo(b)) => a == b,
            (Target::Backend(a), Target::Backend(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }
}

impl Target {
    /// Display form for UIs.
    pub fn describe(&self) -> String {
        match self {
            Target::IntoDir(p) | Target::FileTo(p) => p.display().to_string(),
            Target::Backend(b) => b.describe(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct TransferRequest {
    pub sources: Vec<PathBuf>,
    pub target: Target,
    /// Shown in the UI; defaults to the first source's name.
    pub label: Option<String>,
    /// Per-job engine options (verify mode, overwrite…). `None` = queue defaults.
    /// The queue's shared speed limit always applies.
    pub options: Option<CopyOptions>,
    pub conflict_policy: Option<crate::conflict::ConflictPolicy>,
}

impl TransferRequest {
    pub fn into_dir(sources: Vec<PathBuf>, dir: impl Into<PathBuf>) -> Self {
        Self { sources, target: Target::IntoDir(dir.into()), label: None, options: None, conflict_policy: None }
    }

    pub fn file_to(src: impl Into<PathBuf>, dst: impl Into<PathBuf>) -> Self {
        Self { sources: vec![src.into()], target: Target::FileTo(dst.into()), label: None, options: None, conflict_policy: None }
    }

    /// Copy `sources` into a backend destination (e.g. a remote agent).
    pub fn to_backend(sources: Vec<PathBuf>, backend: Arc<dyn TransferBackend>) -> Self {
        Self { sources, target: Target::Backend(backend), label: None, options: None, conflict_policy: None }
    }

    pub fn with_options(mut self, options: CopyOptions) -> Self {
        self.options = Some(options);
        self
    }

    pub fn with_conflict_policy(mut self, policy: crate::conflict::ConflictPolicy) -> Self {
        self.conflict_policy = Some(policy);
        self
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// The label shown by the queue, also suitable for a finished-job summary.
    pub fn display_label(&self) -> String {
        if let Some(l) = &self.label {
            return l.clone();
        }
        let first = self
            .sources
            .first()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "transfer".into());
        match self.sources.len() {
            0 | 1 => first,
            n => format!("{first} + {} more", n - 1),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobState {
    Pending,
    Running,
    Completed,
    /// Finished, but some files were skipped or failed.
    CompletedWithErrors,
    /// Could not run at all (bad source, not enough space, …).
    Failed(String),
    Cancelled,
}

impl JobState {
    pub fn is_finished(&self) -> bool {
        !matches!(self, JobState::Pending | JobState::Running)
    }
}

/// Snapshot of one job, safe to clone into a UI.
#[derive(Debug, Clone)]
pub struct JobInfo {
    pub id: JobId,
    pub label: String,
    pub sources: Vec<PathBuf>,
    pub target: Target,
    pub state: JobState,
    /// Held by [`Queue::pause_job`]. A paused pending job is skipped by the
    /// scheduler; a paused running job holds the worker at its next chunk.
    pub paused: bool,
    pub files_total: u64,
    pub bytes_total: u64,
    pub files_copied: u64,
    pub files_skipped: u64,
    pub files_failed: u64,
    pub bytes_copied: u64,
    /// Latest telemetry (speed, ETA, percentage, current file).
    pub telemetry: Option<Snapshot>,
    pub queued_at: SystemTime,
    pub elapsed: Option<Duration>,
    pub conflict: Option<PendingConflict>,
}

#[derive(Debug, Clone)]
pub struct PendingConflict {
    pub id: u64,
    pub details: Conflict,
}

/// Totals reported when a job ends.
#[derive(Debug, Clone, Default)]
pub struct JobSummary {
    pub files_copied: u64,
    pub files_skipped: u64,
    pub files_failed: u64,
    pub bytes_copied: u64,
    pub elapsed: Duration,
    pub throughput_bps: f64,
}

/// A question for the user. Answer with [`respond`](Self::respond); dropping
/// it unanswered counts as **Skip**.
#[derive(Debug)]
pub struct DecisionRequest {
    pub job: JobId,
    pub path: PathBuf,
    pub operation: Operation,
    pub class: ErrorClass,
    pub message: String,
    pub offset: Option<u64>,
    pub attempt: u32,
    reply: Sender<ErrorAction>,
}

impl DecisionRequest {
    pub fn respond(self, action: ErrorAction) {
        let _ = self.reply.send(action);
    }
}

/// Everything the queue tells its front end.
#[derive(Debug)]
pub enum Event {
    Queued { job: JobId, label: String },
    Started { job: JobId, files: u64, bytes: u64, warnings: Vec<String> },
    /// Throttled telemetry (see [`QueueConfig::telemetry_interval`]).
    Progress { job: JobId, telemetry: Snapshot },
    FileCopied {
        job: JobId,
        src: PathBuf,
        dst: PathBuf,
        bytes: u64,
        digest: Option<String>,
        verified: bool,
        warnings: Vec<String>,
    },
    FileSkipped { job: JobId, src: PathBuf, reason: String },
    FileFailed { job: JobId, src: PathBuf, reason: String },
    Decision(DecisionRequest),
    Conflict { job: JobId },
    Paused,
    Resumed,
    JobPaused { job: JobId },
    JobResumed { job: JobId },
    SpeedLimit(Option<u64>),
    Finished { job: JobId, state: JobState, summary: JobSummary },
    Removed { job: JobId },
    /// Nothing pending or running.
    Idle,
}

/// How the queue answers recoverable errors.
#[derive(Debug, Clone)]
pub enum ErrorPolicy {
    /// Retry transient errors with back-off, skip the rest, cancel on disk-full.
    Auto(RetryPolicy),
    Skip,
    Cancel,
    /// Ask the front end via [`Event::Decision`].
    Ask,
}

pub struct QueueConfig {
    pub options: CopyOptions,
    pub policy: ErrorPolicy,
    pub plan: PlanOptions,
    pub telemetry_interval: Duration,
    pub speed_limit: Option<u64>,
    /// Called after every event is sent: wire this to `egui::Context::request_repaint`
    /// (or a Tauri emit, or a no-op).
    pub on_event: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            options: CopyOptions::default(),
            policy: ErrorPolicy::Auto(RetryPolicy::default()),
            plan: PlanOptions::default(),
            telemetry_interval: Duration::from_millis(100),
            speed_limit: None,
            on_event: None,
        }
    }
}

// ---------------------------------------------------------------------------

struct Job {
    info: JobInfo,
    request: TransferRequest,
    control: Arc<JobControl>,
}

#[derive(Default)]
struct QState {
    jobs: Vec<Job>,
    next_id: JobId,
    paused: bool,
    shutdown: bool,
    active: Option<JobId>,
    next_conflict: u64,
    conflict_reply: Option<(JobId, u64, Sender<ConflictChoice>)>,
}

impl QState {
    fn job_mut(&mut self, id: JobId) -> Option<&mut Job> {
        self.jobs.iter_mut().find(|j| j.info.id == id)
    }
    fn active_control(&self) -> Option<(JobId, Arc<JobControl>)> {
        let id = self.active?;
        self.jobs.iter().find(|j| j.info.id == id).map(|j| (id, j.control.clone()))
    }
    /// A job the scheduler could start now (pending and not held).
    fn has_pending(&self) -> bool {
        self.jobs.iter().any(|j| j.info.state == JobState::Pending && !j.info.paused)
    }
}

struct Inner {
    state: Mutex<QState>,
    /// Signals: new job, resume, job finished, shutdown.
    wake: Condvar,
    events: Sender<Event>,
    notify: Option<Arc<dyn Fn() + Send + Sync>>,
    limiter: Arc<RateLimiter>,
    /// Read on every error, so it can change while a job runs.
    policy: RwLock<ErrorPolicy>,
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, QState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn emit(&self, ev: Event) {
        // A front end that dropped its receiver simply stops hearing events.
        let _ = self.events.send(ev);
        if let Some(n) = &self.notify {
            n();
        }
    }

    fn update(&self, id: JobId, f: impl FnOnce(&mut JobInfo)) {
        if let Some(j) = self.lock().job_mut(id) {
            f(&mut j.info);
        }
    }

    fn ask_conflict(&self, job: JobId, details: &Conflict, control: &JobControl) -> ConflictChoice {
        let cancel = ConflictChoice { action: ConflictAction::Cancel, apply_to_all: false };
        let (tx, rx) = mpsc::channel();
        {
            let mut state = self.lock();
            state.next_conflict += 1;
            let id = state.next_conflict;
            let Some(entry) = state.job_mut(job) else { return cancel; };
            entry.info.conflict = Some(PendingConflict { id, details: details.clone() });
            state.conflict_reply = Some((job, id, tx));
        }
        let delivered = self.events.send(Event::Conflict { job }).is_ok();
        if let Some(notify) = &self.notify { notify(); }
        let answer = loop {
            if !delivered || control.is_cancelled() { break cancel; }
            if control.take_skip() { break ConflictChoice { action: ConflictAction::Skip, apply_to_all: false }; }
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(answer) => break answer,
                Err(RecvTimeoutError::Disconnected) => break cancel,
                Err(RecvTimeoutError::Timeout) => {}
            }
        };
        {
            let mut state = self.lock();
            state.conflict_reply = None;
            if let Some(entry) = state.job_mut(job) { entry.info.conflict = None; }
        }
        self.emit(Event::Conflict { job });
        answer
    }
}

/// Handle to a running queue. Cheap to clone; all clones control the same queue.
#[derive(Clone)]
pub struct Queue {
    inner: Arc<Inner>,
    worker: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl Queue {
    /// Start the worker thread. Returns the handle and the event stream.
    pub fn start(config: QueueConfig) -> (Queue, Receiver<Event>) {
        let (tx, rx) = mpsc::channel();
        let limiter = Arc::new(RateLimiter::new(config.speed_limit));
        let inner = Arc::new(Inner {
            state: Mutex::new(QState { next_id: 1, ..Default::default() }),
            wake: Condvar::new(),
            events: tx,
            notify: config.on_event.clone(),
            limiter: limiter.clone(),
            policy: RwLock::new(config.policy),
        });

        let mut options = config.options;
        options.rate_limit = Some(limiter.clone());
        let handler: Arc<dyn ErrorHandler> = Arc::new(QueueHandler::new(inner.clone()));
        let copier = Copier::new(options, handler.clone());
        let engines = Engines { default: copier, handler, limiter };

        let w_inner = inner.clone();
        let plan_opts = config.plan;
        let interval = config.telemetry_interval;
        let worker = thread::Builder::new()
            .name("fc-queue".into())
            .spawn(move || worker_loop(w_inner, engines, plan_opts, interval))
            .expect("spawn queue worker");

        (Queue { inner, worker: Arc::new(Mutex::new(Some(worker))) }, rx)
    }

    /// Enqueue a transfer. Runs when everything before it has finished.
    pub fn add(&self, request: TransferRequest) -> JobId {
        let label = request.display_label();
        let id = {
            let mut st = self.inner.lock();
            let id = st.next_id;
            st.next_id += 1;
            st.jobs.push(Job {
                info: JobInfo {
                    id,
                    label: label.clone(),
                    sources: request.sources.clone(),
                    target: request.target.clone(),
                    state: JobState::Pending,
                    paused: false,
                    files_total: 0,
                    bytes_total: 0,
                    files_copied: 0,
                    files_skipped: 0,
                    files_failed: 0,
                    bytes_copied: 0,
                    telemetry: None,
                    queued_at: SystemTime::now(),
                    elapsed: None,
                    conflict: None,
                },
                request,
                control: Arc::new(JobControl::new()),
            });
            id
        };
        self.inner.wake.notify_all();
        self.inner.emit(Event::Queued { job: id, label });
        id
    }

    /// Pause the active transfer at its next chunk boundary and hold the queue.
    pub fn pause(&self) {
        {
            let mut st = self.inner.lock();
            if st.paused {
                return;
            }
            st.paused = true;
            if let Some((_, c)) = st.active_control() {
                c.pause();
            }
        }
        self.inner.emit(Event::Paused);
    }

    /// Continue exactly where the transfer stopped.
    pub fn resume(&self) {
        {
            let mut st = self.inner.lock();
            if !st.paused {
                return;
            }
            st.paused = false;
            // A job the user paused on its own stays paused.
            if let Some((id, c)) = st.active_control() {
                if !st.job_mut(id).is_some_and(|j| j.info.paused) {
                    c.resume();
                }
            }
        }
        self.inner.wake.notify_all();
        self.inner.emit(Event::Resumed);
    }

    /// Hold one job. A pending job is passed over until it is resumed, so
    /// later jobs run first. A running job is paused at its next chunk, which
    /// holds the queue there (jobs run one at a time). Returns false if the job
    /// is unknown or already finished.
    pub fn pause_job(&self, id: JobId) -> bool {
        {
            let mut st = self.inner.lock();
            let Some(job) = st.job_mut(id) else { return false };
            if job.info.state.is_finished() {
                return false;
            }
            if job.info.paused {
                return true;
            }
            job.info.paused = true;
            if job.info.state == JobState::Running {
                job.control.pause();
            }
        }
        self.inner.emit(Event::JobPaused { job: id });
        true
    }

    /// Release a job held by [`pause_job`](Self::pause_job). A running job
    /// continues from the exact byte. Has no effect while the whole queue is
    /// paused, because that pause still applies.
    pub fn resume_job(&self, id: JobId) -> bool {
        {
            let mut st = self.inner.lock();
            let global_paused = st.paused;
            let Some(job) = st.job_mut(id) else { return false };
            if !job.info.paused {
                return true;
            }
            job.info.paused = false;
            if job.info.state == JobState::Running && !global_paused {
                job.control.resume();
            }
        }
        self.inner.wake.notify_all();
        self.inner.emit(Event::JobResumed { job: id });
        true
    }

    pub fn is_paused(&self) -> bool {
        self.inner.lock().paused
    }

    /// Abandon the file currently being copied and move on to the next one.
    pub fn skip_current_file(&self) -> bool {
        match self.inner.lock().active_control() {
            Some((_, c)) => {
                c.request_skip();
                true
            }
            None => false,
        }
    }

    /// Cancel a pending or running job. Returns false if it was already finished.
    pub fn cancel(&self, id: JobId) -> bool {
        let pending_cancelled = {
            let mut st = self.inner.lock();
            let Some(job) = st.job_mut(id) else { return false };
            match job.info.state {
                JobState::Pending => {
                    job.info.state = JobState::Cancelled;
                    true
                }
                JobState::Running => {
                    job.control.cancel();
                    false // the worker reports Finished
                }
                _ => return false,
            }
        };
        if pending_cancelled {
            self.inner.wake.notify_all();
            self.inner.emit(Event::Finished { job: id, state: JobState::Cancelled, summary: JobSummary::default() });
        }
        true
    }

    /// Cancel everything pending and running.
    pub fn cancel_all(&self) {
        let ids: Vec<JobId> = self.inner.lock().jobs.iter().filter(|j| !j.info.state.is_finished()).map(|j| j.info.id).collect();
        for id in ids {
            self.cancel(id);
        }
    }

    /// Remove a pending or finished job from the list.
    pub fn remove(&self, id: JobId) -> bool {
        let removed = {
            let mut st = self.inner.lock();
            let before = st.jobs.len();
            st.jobs.retain(|j| j.info.id != id || j.info.state == JobState::Running);
            st.jobs.len() != before
        };
        if removed {
            self.inner.emit(Event::Removed { job: id });
        }
        removed
    }

    /// Drop all finished jobs from the list.
    pub fn clear_finished(&self) {
        let ids: Vec<JobId> = self.inner.lock().jobs.iter().filter(|j| j.info.state.is_finished()).map(|j| j.info.id).collect();
        for id in ids {
            self.remove(id);
        }
    }

    /// How recoverable errors are answered from now on. Applies to the file
    /// in progress as well as later ones.
    pub fn set_error_policy(&self, policy: ErrorPolicy) {
        *self.inner.policy.write().unwrap_or_else(PoisonError::into_inner) = policy;
    }

    pub fn error_policy(&self) -> ErrorPolicy {
        self.inner.policy.read().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Cap total throughput (bytes/s); `None` removes the cap. Applies immediately.
    pub fn set_speed_limit(&self, bytes_per_sec: Option<u64>) {
        self.inner.limiter.set_limit(bytes_per_sec);
        self.inner.emit(Event::SpeedLimit(self.inner.limiter.limit()));
    }

    pub fn speed_limit(&self) -> Option<u64> {
        self.inner.limiter.limit()
    }

    pub fn jobs(&self) -> Vec<JobInfo> {
        self.inner.lock().jobs.iter().map(|j| j.info.clone()).collect()
    }

    pub fn job(&self, id: JobId) -> Option<JobInfo> {
        self.inner.lock().jobs.iter().find(|j| j.info.id == id).map(|j| j.info.clone())
    }

    pub fn active_job(&self) -> Option<JobInfo> {
        let st = self.inner.lock();
        let id = st.active?;
        st.jobs.iter().find(|j| j.info.id == id).map(|j| j.info.clone())
    }

    /// Reject stale replies rather than applying them to the next file.
    pub fn resolve_conflict(&self, job: JobId, id: u64, choice: ConflictChoice) -> bool {
        let mut state = self.inner.lock();
        let valid = state.jobs.iter().find(|j| j.info.id == job).is_some_and(|j| {
            !j.control.is_cancelled() && j.info.conflict.as_ref().is_some_and(|c| {
                c.id == id && !(c.details.different && choice.action == ConflictAction::Compare)
            })
        });
        if !valid { return false; }
        match state.conflict_reply.take() {
            Some((pending_job, pending_id, sender)) if pending_job == job && pending_id == id => sender.send(choice).is_ok(),
            other => { state.conflict_reply = other; false }
        }
    }

    /// Block until nothing is running and nothing runnable is pending.
    pub fn wait_idle(&self) {
        let mut st = self.inner.lock();
        while st.active.is_some() || (!st.paused && st.has_pending()) {
            st = self.inner.wake.wait(st).unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Cancel the active job, stop the worker and wait for it to exit.
    pub fn shutdown(&self) {
        {
            let mut st = self.inner.lock();
            st.shutdown = true;
            if let Some((_, c)) = st.active_control() {
                c.cancel();
            }
        }
        self.inner.wake.notify_all();
        if let Some(h) = self.worker.lock().unwrap_or_else(PoisonError::into_inner).take() {
            let _ = h.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

/// The default engine plus what's needed to build per-job ones.
struct Engines {
    default: Copier,
    handler: Arc<dyn ErrorHandler>,
    limiter: Arc<RateLimiter>,
}

fn worker_loop(inner: Arc<Inner>, engines: Engines, plan_opts: PlanOptions, interval: Duration) {
    loop {
        // Wait for something runnable.
        let (id, request, control) = {
            let mut st = inner.lock();
            loop {
                if st.shutdown {
                    return;
                }
                if !st.paused {
                    if let Some(i) = st.jobs.iter().position(|j| j.info.state == JobState::Pending && !j.info.paused) {
                        let job = &mut st.jobs[i];
                        job.info.state = JobState::Running;
                        let picked = (job.info.id, job.request.clone(), job.control.clone());
                        st.active = Some(picked.0);
                        break picked;
                    }
                }
                st = inner.wake.wait(st).unwrap_or_else(PoisonError::into_inner);
            }
        };

        let started = Instant::now();
        let custom = request.options.clone()
            .or_else(|| (request.conflict_policy.is_some() || engines.default.options().conflicts.is_some()).then(|| engines.default.options().clone()))
            .map(|mut o| {
            o.rate_limit = Some(engines.limiter.clone());
            if let Some(policy) = request.conflict_policy { o.conflicts = Some(Arc::new(ConflictResolver::new(policy))); }
            if let Some(resolver) = &o.conflicts {
                let inner = inner.clone();
                o.conflicts = Some(Arc::new(ConflictResolver::new(resolver.policy).with_handler(Arc::new(
                    move |details, control| inner.ask_conflict(id, details, control)
                ))));
            }
            Copier::new(o, engines.handler.clone())
        });
        let copier = custom.as_ref().unwrap_or(&engines.default);
        let (state, summary) = run_job(&inner, copier, &plan_opts, interval, id, &request, &control);

        let idle = {
            let mut st = inner.lock();
            if let Some(j) = st.job_mut(id) {
                j.info.state = state.clone();
                j.info.elapsed = Some(started.elapsed());
            }
            st.active = None;
            !st.has_pending()
        };
        inner.wake.notify_all();
        inner.emit(Event::Finished { job: id, state, summary });
        if idle {
            inner.emit(Event::Idle);
        }
    }
}

fn run_job(
    inner: &Arc<Inner>,
    copier: &Copier,
    plan_opts: &PlanOptions,
    interval: Duration,
    id: JobId,
    request: &TransferRequest,
    control: &JobControl,
) -> (JobState, JobSummary) {
    let plan = match &request.target {
        Target::IntoDir(dir) => plan_copy_with(&request.sources, dir, plan_opts).map_err(|e| CopyError::Backend(e.to_string())),
        Target::FileTo(dst) if request.sources.len() == 1 => plan_file_to(&request.sources[0], dst).map_err(|e| CopyError::Backend(e.to_string())),
        Target::FileTo(_) => Err(CopyError::Backend("a file target needs exactly one source; use a directory target".into())),
        // Relative destinations: the backend decides where they land.
        Target::Backend(backend) => backend.plan(&request.sources, plan_opts, control),
    };
    let plan = match plan {
        Ok(p) => p,
        Err(_) if control.is_cancelled() => return (JobState::Cancelled, JobSummary::default()),
        Err(e) => return (JobState::Failed(e.to_string()), JobSummary::default()),
    };

    inner.update(id, |j| {
        j.files_total = plan.stats.files;
        j.bytes_total = plan.stats.bytes;
    });
    inner.emit(Event::Started { job: id, files: plan.stats.files, bytes: plan.stats.bytes, warnings: plan.warnings.clone() });

    let progress = {
        let inner = inner.clone();
        Progress::with_observer(interval, move |snap| {
            inner.update(id, |j| j.telemetry = Some(snap.clone()));
            inner.emit(Event::Progress { job: id, telemetry: snap.clone() });
        })
    };

    let mut on_file = |ev: FileEvent<'_>| match ev {
        FileEvent::Copied(r) => {
            inner.update(id, |j| {
                j.files_copied += 1;
                j.bytes_copied += r.bytes;
            });
            inner.emit(Event::FileCopied {
                job: id,
                src: r.src.clone(),
                dst: r.dst.clone(),
                bytes: r.bytes,
                digest: r.source_digest.as_ref().map(|d| d.to_hex()),
                verified: r.verified,
                warnings: r.warnings.clone(),
            });
        }
        FileEvent::Skipped(item, e) => {
            inner.update(id, |j| j.files_skipped += 1);
            inner.emit(Event::FileSkipped { job: id, src: item.src.clone(), reason: e.to_string() });
        }
        FileEvent::Failed(item, e) => {
            inner.update(id, |j| j.files_failed += 1);
            inner.emit(Event::FileFailed { job: id, src: item.src.clone(), reason: e.to_string() });
        }
    };

    let result = match &request.target {
        Target::Backend(backend) => {
            let ctx = BackendContext { progress: &progress, control, options: copier.options() };
            backend.run(&plan, &ctx, &mut on_file)
        }
        _ => copier.run_plan_observed(&plan, &plan.dest_root, &progress, control, &mut on_file),
    };
    inner.update(id, |j| j.telemetry = Some(progress.snapshot()));

    match result {
        Err(CopyError::Cancelled) => (JobState::Cancelled, JobSummary::default()),
        Err(e) => (JobState::Failed(e.to_string()), JobSummary::default()),
        Ok(r) => {
            let summary = JobSummary {
                files_copied: r.copied.len() as u64,
                files_skipped: r.skipped.len() as u64,
                files_failed: r.failed.len() as u64,
                bytes_copied: r.bytes_copied,
                elapsed: r.elapsed,
                throughput_bps: r.throughput_bps(),
            };
            let state = if r.cancelled || control.is_cancelled() {
                JobState::Cancelled
            } else if r.is_clean() {
                JobState::Completed
            } else {
                JobState::CompletedWithErrors
            };
            (state, summary)
        }
    }
}

// ---------------------------------------------------------------------------
// Error handler bridging the engine to the front end
// ---------------------------------------------------------------------------

struct QueueHandler {
    inner: Arc<Inner>,
}

impl QueueHandler {
    fn new(inner: Arc<Inner>) -> Self {
        Self { inner }
    }

    fn ask(&self, ctx: &ErrorContext<'_>) -> ErrorAction {
        let Some((job, control)) = self.inner.lock().active_control() else {
            return ErrorAction::Skip;
        };
        let (tx, rx) = mpsc::channel();
        self.inner.emit(Event::Decision(DecisionRequest {
            job,
            path: ctx.path.to_path_buf(),
            operation: ctx.operation,
            class: ctx.class,
            message: ctx.error.to_string(),
            offset: ctx.offset,
            attempt: ctx.attempt,
            reply: tx,
        }));
        // Wait for the answer, but stay responsive to Cancel / Skip buttons.
        loop {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(action) => return action,
                Err(RecvTimeoutError::Timeout) => {
                    if control.is_cancelled() {
                        return ErrorAction::Cancel;
                    }
                    if control.take_skip() {
                        return ErrorAction::Skip;
                    }
                }
                Err(RecvTimeoutError::Disconnected) => return ErrorAction::Skip,
            }
        }
    }
}

impl ErrorHandler for QueueHandler {
    fn on_error(&self, ctx: &ErrorContext<'_>) -> ErrorAction {
        let policy = self.inner.policy.read().unwrap_or_else(PoisonError::into_inner).clone();
        match policy {
            ErrorPolicy::Auto(p) => AutoHandler::new(p).on_error(ctx),
            ErrorPolicy::Skip => ErrorAction::Skip,
            ErrorPolicy::Cancel => ErrorAction::Cancel,
            ErrorPolicy::Ask => self.ask(ctx),
        }
    }

    fn retry_delay(&self, ctx: &ErrorContext<'_>) -> Duration {
        let policy = self.inner.policy.read().unwrap_or_else(PoisonError::into_inner).clone();
        match policy {
            ErrorPolicy::Auto(p) => AutoHandler::new(p).retry_delay(ctx),
            _ => Duration::ZERO,
        }
    }
}

// Compile-time guarantee that the handle can be shared with UI threads.
#[allow(dead_code)]
fn _assert_send_sync() {
    fn is<T: Send + Sync>() {}
    is::<Queue>();
}

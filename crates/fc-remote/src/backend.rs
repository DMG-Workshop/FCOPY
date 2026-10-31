//! A remote agent as a destination for the transfer queue.
//!
//! [`RemoteBackend`] takes the same [`Plan`] a local copy would, and sends it to
//! a paired agent:
//!
//! 1. Directories, in plan order, so every parent exists before its files.
//! 2. Files under [`SMALL_FILE`] concurrently, [`SMALL_PARALLEL`] at a time,
//!    over one session (each file has its own QUIC stream, so they don't block
//!    each other).
//! 3. Larger files one at a time, so each one shows exact progress.
//!
//! A dropped connection is reopened with the session token first, then the
//! password. Backoff runs from [`BACKOFF_START`] to [`BACKOFF_MAX`], and gives
//! up after [`RECONNECT_BUDGET`]. The file in flight resumes from the offset the
//! server already holds. Cancel and skip interrupt waits at once.
//!
//! Remote transfers always verify: the server re-reads the file and its digest
//! must match. The copy engine's `verify` setting therefore doesn't apply here.
//!
//! # Downloads
//!
//! [`RemoteBackend::pull`] is the other direction, outside the transfer queue. It
//! lists folders, then downloads their files one at a time into `name.fcpart`
//! and renames each into place after the digest matches. It reconnects and
//! resumes the same way, and never trusts a server-supplied name: anything that
//! is not one plain component is reported as a failure instead of written.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{StreamExt, stream};
use tokio::runtime::Runtime;
use tokio::sync::Mutex as AsyncMutex;

use fc_engine::{
    Algorithm, BackendContext, BatchReport, CopyError, CopyItem, CopyReport, FileEvent, JobControl, LinkStats,
    Operation, Plan, PlanOptions, Progress, RateLimiter, TransferBackend,
};

use crate::client::{
    ClientError, Connector, PullObserver, PullOutcome, PullRequest, PushControl, PushObserver, PushRequest, Session,
    fresh_source_id,
};
use crate::protocol::{DirEntry, EntryKind, HashAlgo, RejectReason};

/// Files below this size go through the concurrent lane.
pub const SMALL_FILE: u64 = 4 * 1024 * 1024;
/// Small files in flight at once.
pub const SMALL_PARALLEL: usize = 8;
/// First wait after a failed connection attempt.
pub const BACKOFF_START: Duration = Duration::from_millis(500);
/// Longest single wait between attempts.
pub const BACKOFF_MAX: Duration = Duration::from_secs(10);
/// Give up reconnecting after this long in total.
pub const RECONNECT_BUDGET: Duration = Duration::from_secs(120);
/// Most times one file is retried after connection drops before it is failed.
const MAX_FILE_ATTEMPTS: u32 = 8;

/// Sends a plan to one paired agent. Create one per destination and keep it:
/// it holds the session and the token, so later jobs skip the password.
pub struct RemoteBackend {
    connector: Connector,
    runtime: Option<Runtime>,
    /// The live session, if any. Held across a reconnect, so concurrent files
    /// wait for one attempt instead of each starting their own.
    slot: AsyncMutex<Option<Arc<Session>>>,
    last_token: Mutex<Option<[u8; 32]>>,
    /// Sessions opened so far. Every one after the first is a reconnect.
    opens: AtomicU32,
    /// Job-level wire counters, reset at the start of each run.
    sent: AtomicU64,
    acked: AtomicU64,
    restart_files: bool,
}

impl RemoteBackend {
    /// Browse shares (empty path) or a directory without blocking a UI thread.
    /// Call this synchronous adapter from a background worker.
    pub fn browse(&self, path: &str) -> Result<Vec<DirEntry>, ClientError> {
        self.runtime.as_ref().expect("runtime lives until drop").block_on(async {
            tokio::time::timeout(Duration::from_secs(60), async {
                let session = self.session(&JobControl::new()).await?;
                session.browse(path).await
            }).await.map_err(|_| ClientError::Connection("remote browsing timed out".into()))?
        })
    }

    /// The connector must carry the fingerprint the user confirmed at pairing. There is
    /// no trust-on-first-use here, so an unpaired destination can never receive a job.
    pub fn new(connector: Connector) -> std::io::Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("fc-remote")
            .enable_all()
            .build()?;
        Ok(Self {
            connector,
            runtime: Some(runtime),
            slot: AsyncMutex::new(None),
            last_token: Mutex::new(None),
            opens: AtomicU32::new(0),
            sent: AtomicU64::new(0),
            acked: AtomicU64::new(0),
            restart_files: false,
        })
    }

    /// Retry starts each file fresh, but connection-loss retries can still resume it.
    pub fn with_restart_files(mut self, restart: bool) -> Self {
        self.restart_files = restart;
        self
    }

    pub fn recovery_destination(&self, local_download: Option<PathBuf>) -> std::io::Result<crate::recovery::Destination> {
        let target = self.connector.spec().to_string();
        let fingerprint = self.connector.pinned_fingerprint()
            .ok_or_else(|| std::io::Error::other("recovery requires a confirmed remote fingerprint"))?;
        Ok(match local_download {
            Some(path) => crate::recovery::Destination::Download { target, fingerprint, path },
            None => crate::recovery::Destination::Upload { target, fingerprint },
        })
    }

    fn reconnects(&self) -> u32 {
        self.opens.load(Ordering::Relaxed).saturating_sub(1)
    }

    /// The session to use right now, reconnecting if it has dropped.
    async fn session(&self, control: &JobControl) -> Result<Arc<Session>, ClientError> {
        let mut slot = self.slot.lock().await;
        if let Some(s) = slot.as_ref() {
            if s.is_connected() {
                return Ok(s.clone());
            }
        }
        *slot = None;
        let s = Arc::new(self.reconnect(control).await?);
        *slot = Some(s.clone());
        Ok(s)
    }

    /// Forget `dead` so the next [`session`](Self::session) call reconnects. Does
    /// nothing if another task has already replaced it.
    async fn drop_session(&self, dead: &Arc<Session>) {
        let mut slot = self.slot.lock().await;
        if slot.as_ref().is_some_and(|s| Arc::ptr_eq(s, dead)) {
            *slot = None;
        }
    }

    async fn reconnect(&self, control: &JobControl) -> Result<Session, ClientError> {
        let started = Instant::now();
        let mut delay = BACKOFF_START;
        loop {
            if control.is_cancelled() {
                return Err(ClientError::Cancelled);
            }
            let token = *self.last_token.lock().unwrap_or_else(|e| e.into_inner());
            match Session::open(&self.connector, token).await {
                Ok(s) => {
                    self.opens.fetch_add(1, Ordering::Relaxed);
                    *self.last_token.lock().unwrap_or_else(|e| e.into_inner()) = Some(s.token());
                    return Ok(s);
                }
                // Refused logins and changed identities won't fix themselves.
                Err(e) if !e.is_retryable() => return Err(e),
                Err(e) => {
                    let left = RECONNECT_BUDGET.saturating_sub(started.elapsed());
                    if left.is_zero() {
                        return Err(e);
                    }
                    if !wait_unless_cancelled(delay.min(left), control).await {
                        return Err(ClientError::Cancelled);
                    }
                    delay = (delay * 2).min(BACKOFF_MAX);
                }
            }
        }
    }

    fn publish_link(&self, progress: &Progress, downloading: bool) {
        let mut link = match self.slot.try_lock() {
            Ok(slot) => slot.as_ref().map(|s| s.link_stats()),
            Err(_) => None,
        }
        .unwrap_or_else(|| LinkStats { peer: self.connector.spec().host_port(), ..LinkStats::default() });
        link.sent_bytes = self.sent.load(Ordering::Relaxed);
        link.acked_bytes = self.acked.load(Ordering::Relaxed);
        if downloading {
            link.received_bytes = Some(link.acked_bytes);
            link.acked_bytes = 0;
        }
        link.reconnects = self.reconnects();
        progress.set_link(link);
    }

    /// Create one remote directory, reconnecting if the connection drops.
    async fn ensure_dir(&self, control: &JobControl, remote: &str) -> Result<(), ClientError> {
        let mut attempts = 0;
        loop {
            let s = self.session(control).await?;
            match s.make_dir(remote).await {
                Ok(()) => return Ok(()),
                Err(e) if e.is_retryable() && attempts < MAX_FILE_ATTEMPTS => {
                    attempts += 1;
                    self.drop_session(&s).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Upload one file. Retries across reconnects resume from the server's offset.
    async fn send_file(&self, ctx: &BackendContext<'_>, item: &CopyItem, remote: &str) -> Result<CopyReport, CopyError> {
        let ctl = PushControl { control: ctx.control, limiter: ctx.options.rate_limit.as_deref() };
        let (remote, _, overwrite) = self.conflict_destination(
            remote, &item.src, true, ctx.options.overwrite, ctx.options.conflicts.as_deref(), &ctl,
        ).await.map_err(|e| to_copy_error(e, &item.src))?;
        let started = Instant::now();
        let tally = Tally {
            sent: &self.sent,
            acked: &self.acked,
            progress: ctx.progress,
            sent_hw: AtomicU64::new(0),
            acked_hw: AtomicU64::new(0),
        };
        let mut attempts = 0;
        let mut source_id = self.restart_files.then(fresh_source_id);
        let mut mismatch_retried = false;
        loop {
            let s = self.session(ctx.control).await.map_err(|e| to_copy_error(e, &item.src))?;
            let req = PushRequest {
                local: &item.src,
                remote: &remote,
                algo: wire_algo(ctx.options.algorithm),
                overwrite,
                source_id,
            };
            let ctl = PushControl { control: ctx.control, limiter: ctx.options.rate_limit.as_deref() };
            match s.push_file(&req, &ctl, &tally).await {
                Ok(out) => {
                    return Ok(CopyReport {
                        src: item.src.clone(),
                        dst: PathBuf::from(&remote),
                        bytes: out.size,
                        source_digest: Some(out.digest),
                        verified: true,
                        retries: attempts,
                        elapsed: started.elapsed(),
                        warnings: vec![],
                    });
                }
                Err(e) if e.is_retryable() && attempts < MAX_FILE_ATTEMPTS => {
                    attempts += 1;
                    self.drop_session(&s).await;
                }
                // The server's partial bytes didn't match this source. Start the file over once.
                Err(ClientError::Mismatch { .. }) if !mismatch_retried => {
                    mismatch_retried = true;
                    attempts += 1;
                    source_id = Some(fresh_source_id());
                }
                Err(e) => return Err(to_copy_error(e, &item.src)),
            }
        }
    }

    /// Record one finished file: counters, the caller's event, and the report.
    fn record(
        &self,
        ctx: &BackendContext<'_>,
        item: &CopyItem,
        result: Result<CopyReport, CopyError>,
        report: &mut BatchReport,
        on_file: &mut dyn FnMut(FileEvent<'_>),
    ) -> bool {
        match result {
            Ok(rep) => {
                ctx.progress.count_file(false, 0);
                report.bytes_copied += rep.bytes;
                on_file(FileEvent::Copied(&rep));
                report.copied.push(rep);
            }
            Err(CopyError::Cancelled) => {
                report.cancelled = true;
                return false;
            }
            Err(CopyError::SkippedByUser) => {
                ctx.progress.count_file(true, item.size);
                on_file(FileEvent::Skipped(item, &CopyError::SkippedByUser));
                report.skipped.push((item.clone(), CopyError::SkippedByUser));
            }
            Err(e) => {
                ctx.progress.count_file(true, item.size);
                on_file(FileEvent::Failed(item, &e));
                report.failed.push((item.clone(), e));
            }
        }
        self.publish_link(ctx.progress, false);
        true
    }

    async fn run_plan(
        &self,
        plan: &Plan,
        ctx: &BackendContext<'_>,
        on_file: &mut dyn FnMut(FileEvent<'_>),
    ) -> Result<BatchReport, CopyError> {
        let mut report = BatchReport { warnings: plan.warnings.clone(), ..Default::default() };
        let root = self.connector.spec().clone();

        self.ensure_dir(ctx.control, &root.path).await.map_err(|e| to_copy_error(e, Path::new(&root.path)))?;
        for dir in &plan.dirs {
            if dir.as_os_str().is_empty() {
                continue;
            }
            let remote = root.join(dir);
            self.ensure_dir(ctx.control, &remote).await.map_err(|e| to_copy_error(e, Path::new(&remote)))?;
        }

        let (small, large): (Vec<&CopyItem>, Vec<&CopyItem>) = plan.items.iter().partition(|i| i.size < SMALL_FILE);

        // Small files: concurrent, each on its own stream.
        let mut lane = stream::iter(small.into_iter().map(|item| {
            let remote = root.join(&item.dst);
            async move {
                let result = self.send_file(ctx, item, &remote).await;
                (item, result)
            }
        }))
        .buffer_unordered(if ctx.options.conflicts.is_some() { 1 } else { SMALL_PARALLEL });
        while let Some((item, result)) = lane.next().await {
            if !self.record(ctx, item, result, &mut report, on_file) {
                return Ok(report);
            }
        }
        drop(lane);

        // Large files: one at a time, so progress is exact.
        for item in large {
            if ctx.control.is_cancelled() {
                report.cancelled = true;
                break;
            }
            let remote = root.join(&item.dst);
            ctx.progress.begin_file(&item.src, item.size, false);
            let result = self.send_file(ctx, item, &remote).await;
            ctx.progress.finish_file();
            if !self.record(ctx, item, result, &mut report, on_file) {
                break;
            }
        }
        Ok(report)
    }
}

// ---------------------------------------------------------------------------
// Download (pull)
// ---------------------------------------------------------------------------

/// Deepest folder nesting a download follows. Symlinks inside a share can form
/// loops, and the server is not trusted to be finite.
const MAX_PULL_DEPTH: usize = 64;

#[derive(Debug, Clone)]
pub struct PullOptions {
    pub algo: HashAlgo,
    /// Replace files that already exist locally.
    pub overwrite: bool,
    pub conflicts: Option<Arc<fc_engine::conflict::ConflictResolver>>,
    /// Copy modification times and (on Unix) permissions from the server.
    pub preserve: bool,
}

impl Default for PullOptions {
    fn default() -> Self {
        Self { algo: HashAlgo::Xxh3, overwrite: false, preserve: true, conflicts: None }
    }
}

/// What a download reports as it goes. Called from the transfer thread.
pub trait PullProgress: Send + Sync {
    /// A file is about to be received.
    fn begin(&self, _remote: &str, _size: u64) {}
    /// `bytes` more bytes of the current file are on disk.
    fn advance(&self, _bytes: u64) {}
    /// The file finished (`Ok(size)`) or failed.
    fn finish(&self, _remote: &str, _result: Result<u64, &ClientError>) {}
}

impl PullProgress for () {}

#[derive(Debug, Default)]
pub struct PullReport {
    pub files: u64,
    pub dirs: u64,
    pub bytes: u64,
    /// Skipped by the user (remote path, reason).
    pub skipped: Vec<String>,
    /// `(remote path, reason)`.
    pub failed: Vec<(String, String)>,
    pub cancelled: bool,
    pub elapsed: Duration,
}

struct PullCtx<'a> {
    opts: &'a PullOptions,
    control: &'a JobControl,
    limiter: Option<&'a RateLimiter>,
    progress: &'a dyn PullProgress,
}

/// A name from the server becomes a name on our disk, so it must be one
/// ordinary component that this OS accepts as is.
fn safe_local_name(name: &str) -> bool {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\\', '\0']) {
        return false;
    }
    if cfg!(windows) && (name.contains(':') || fc_engine::platform::names::sanitize_for_windows(name) != name) {
        return false;
    }
    true
}

/// A remote source and a local destination, scheduled by the ordinary queue.
pub struct PullBackend {
    remote: Arc<RemoteBackend>,
    destination: PathBuf,
}

impl PullBackend {
    pub fn new(remote: Arc<RemoteBackend>, destination: PathBuf) -> Self {
        Self { remote, destination }
    }

    async fn plan_remote(&self, sources: &[PathBuf], control: &JobControl) -> Result<Plan, ClientError> {
        let invalid = |message: &str| ClientError::Protocol(message.into());
        if sources.is_empty() || self.destination.as_os_str().is_empty() {
            return Err(invalid("downloads need sources and a local destination folder"));
        }
        let mut plan = Plan { dest_root: self.destination.clone(), ..Default::default() };
        let mut stack = Vec::new();
        let mut targets = std::collections::HashSet::new();
        for source in sources.iter().rev() {
            let path = source.to_str().ok_or_else(|| invalid("remote paths must be UTF-8"))?;
            let spec = crate::client::RemoteSpec::parse(&format!("host:/{path}"))
                .map_err(|e| invalid(&e.to_string()))?;
            let name = spec.path.rsplit('/').next().expect("nonempty path");
            if !safe_local_name(name) {
                return Err(invalid("unsafe remote source name"));
            }
            stack.push((spec.path.clone(), self.destination.join(name), 0));
        }
        while let Some((source, dest, depth)) = stack.pop() {
            control.checkpoint().map_err(|_| ClientError::Cancelled)?;
            // Reject ambiguous mappings rather than overwrite two remote entries
            // with the same local filename on case-insensitive filesystems.
            let key = dest.to_string_lossy().to_string();
            let key = if cfg!(any(windows, target_os = "macos")) { key.to_lowercase() } else { key };
            if !targets.insert(key) {
                return Err(invalid("remote sources map to duplicate local paths"));
            }
            validate_download_path(&dest)?;
            let session = self.remote.session(control).await?;
            match session.file_size(&source).await {
                Ok(size) => {
                    plan.stats.files += 1;
                    plan.stats.bytes = plan.stats.bytes.checked_add(size).ok_or_else(|| invalid("download is too large"))?;
                    plan.items.push(CopyItem { src: source.into(), dst: dest, size });
                }
                Err(ClientError::Rejected { reason: RejectReason::IsDirectory, .. }) => {
                    if depth >= MAX_PULL_DEPTH {
                        return Err(invalid("remote folders nested too deeply"));
                    }
                    let entries = self.remote.list_retrying(control, &source).await?;
                    plan.stats.dirs += 1;
                    plan.dirs.push(dest.clone());
                    for entry in entries.into_iter().rev() {
                        if !safe_local_name(&entry.name) {
                            return Err(invalid("unsafe filename sent by the server"));
                        }
                        stack.push((format!("{source}/{}", entry.name), dest.join(entry.name), depth + 1));
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Ok(plan)
    }

    async fn receive_plan(&self, plan: &Plan, ctx: &BackendContext<'_>, on_file: &mut dyn FnMut(FileEvent<'_>)) -> Result<BatchReport, CopyError> {
        let started = Instant::now();
        let mut report = BatchReport::default();
        ctx.progress.set_job_totals(plan.stats.files, plan.stats.bytes, false);
        for dir in &plan.dirs {
            ctx.control.checkpoint().map_err(|_| CopyError::Cancelled)?;
            validate_download_path(dir).map_err(|e| to_copy_error(e, dir))?;
            tokio::fs::create_dir_all(dir).await
                .map_err(|e| CopyError::Backend(format!("{}: {e}", dir.display())))?;
        }
        for item in &plan.items {
            if ctx.control.is_cancelled() {
                report.cancelled = true;
                break;
            }
            ctx.progress.begin_file(&item.src, item.size, false);
            ctx.progress.set_phase(fc_engine::Phase::Copying);
            let progress = QueuedPullProgress { progress: ctx.progress, remote: &self.remote };
            let opts = PullOptions {
                algo: wire_algo(ctx.options.algorithm),
                overwrite: ctx.options.overwrite,
                conflicts: ctx.options.conflicts.clone(),
                preserve: ctx.options.preserve_mtime || ctx.options.preserve_permissions,
            };
            let pull_ctx = PullCtx { opts: &opts, control: ctx.control, limiter: ctx.options.rate_limit.as_deref(), progress: &progress };
            let file_started = Instant::now();
            let paths = [
                item.dst.clone(),
                crate::server::sidecar(&item.dst, crate::server::TEMP_SUFFIX),
                crate::server::sidecar(&item.dst, crate::server::ID_SUFFIX),
            ];
            let result = match paths.iter().try_for_each(|path| validate_download_path(path)) {
                Ok(()) => self.remote.pull_file_retrying(&pull_ctx, &item.src.to_string_lossy(), &item.dst).await,
                Err(e) => Err(e),
            };
            match result {
                Ok(out) => {
                    let rep = CopyReport {
                        src: item.src.clone(), dst: out.local.clone(), bytes: out.size,
                        source_digest: Some(out.digest), verified: true, retries: 0,
                        elapsed: file_started.elapsed(), warnings: vec![],
                    };
                    ctx.progress.finish_file();
                    report.bytes_copied += rep.bytes;
                    on_file(FileEvent::Copied(&rep));
                    report.copied.push(rep);
                }
                Err(ClientError::Cancelled) => { report.cancelled = true; break; }
                Err(ClientError::Skipped) => {
                    ctx.progress.skip_file(item.size);
                    on_file(FileEvent::Skipped(item, &CopyError::SkippedByUser));
                    report.skipped.push((item.clone(), CopyError::SkippedByUser));
                }
                Err(e) => {
                    let e = to_copy_error(e, &item.dst);
                    ctx.progress.skip_file(item.size);
                    on_file(FileEvent::Failed(item, &e));
                    report.failed.push((item.clone(), e));
                }
            }
        }
        report.elapsed = started.elapsed();
        Ok(report)
    }
}

// Do not descend through an existing link/junction in a download tree.
fn validate_download_path(path: &Path) -> Result<(), ClientError> {
    for ancestor in path.ancestors().filter(|p| !p.as_os_str().is_empty()) {
        match std::fs::symlink_metadata(ancestor) {
            Ok(meta) => {
                #[cfg(windows)]
                let link = {
                    use std::os::windows::fs::MetadataExt;
                    meta.file_attributes() & 0x400 != 0
                };
                #[cfg(not(windows))]
                let link = meta.file_type().is_symlink();
                if link {
                    return Err(ClientError::Local { path: ancestor.into(), source: std::io::Error::other("download paths cannot traverse links or junctions") });
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(ClientError::Local { path: ancestor.into(), source }),
        }
    }
    Ok(())
}

struct QueuedPullProgress<'a> {
    progress: &'a Progress,
    remote: &'a RemoteBackend,
}

impl PullProgress for QueuedPullProgress<'_> {
    fn advance(&self, bytes: u64) {
        self.progress.add_copied(bytes);
        self.remote.acked.fetch_add(bytes, Ordering::Relaxed);
    }
}

impl TransferBackend for PullBackend {
    fn describe(&self) -> String {
        format!("{} (from {})", self.destination.display(), self.remote.connector.spec().host_port())
    }

    fn plan(&self, sources: &[PathBuf], _options: &PlanOptions, control: &JobControl) -> Result<Plan, CopyError> {
        self.remote.runtime.as_ref().expect("runtime lives until drop").block_on(async {
            let cancel = async {
                loop {
                    if control.is_cancelled() { break; }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            };
            tokio::select! {
                result = self.plan_remote(sources, control) => result.map_err(|e| to_copy_error(e, &self.destination)),
                () = cancel => Err(CopyError::Cancelled),
            }
        })
    }

    fn run(&self, plan: &Plan, ctx: &BackendContext<'_>, on_file: &mut dyn FnMut(FileEvent<'_>)) -> Result<BatchReport, CopyError> {
        self.remote.sent.store(0, Ordering::Relaxed);
        self.remote.acked.store(0, Ordering::Relaxed);
        self.remote.runtime.as_ref().expect("runtime lives until drop").block_on(async {
            let telemetry = async {
                loop {
                    self.remote.publish_link(ctx.progress, true);
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            };
            tokio::select! {
                result = self.receive_plan(plan, ctx, on_file) => {
                    self.remote.publish_link(ctx.progress, true);
                    result
                },
                () = telemetry => unreachable!(),
            }
        })
    }
}

impl RemoteBackend {
    /// Download `remote` (`share/path`, a file or a folder) into `dest`.
    ///
    /// A file goes to `dest` itself, or into it when `dest` is an existing
    /// folder or ends with a path separator. A folder is copied *into* `dest`
    /// under its own name, like a local folder copy. Failures of single files are
    /// collected in the report; only problems that stop everything (login,
    /// an unreachable server, a bad top-level path) are returned as errors.
    pub fn pull(
        &self,
        remote: &str,
        dest: &Path,
        opts: &PullOptions,
        control: &JobControl,
        limiter: Option<&RateLimiter>,
        progress: &dyn PullProgress,
    ) -> Result<PullReport, ClientError> {
        let started = Instant::now();
        let ctx = PullCtx { opts, control, limiter, progress };
        let mut report = PullReport::default();
        let runtime = self.runtime.as_ref().expect("runtime lives until drop");
        match runtime.block_on(self.pull_tree(&ctx, remote.trim_matches('/'), dest, &mut report)) {
            Ok(()) => {}
            Err(ClientError::Cancelled) => report.cancelled = true,
            Err(e) => return Err(e),
        }
        report.elapsed = started.elapsed();
        Ok(report)
    }

    async fn pull_tree(&self, ctx: &PullCtx<'_>, remote: &str, dest: &Path, report: &mut PullReport) -> Result<(), ClientError> {
        let name = remote.rsplit('/').next().unwrap_or(remote);
        let into_dir = dest.is_dir() || dest.as_os_str().to_string_lossy().ends_with(['/', std::path::MAIN_SEPARATOR]);
        let file_dest = if into_dir { dest.join(name) } else { dest.to_path_buf() };

        // Most targets are files, and asking is how we learn it is a folder.
        match self.pull_file_retrying(ctx, remote, &file_dest).await {
            Ok(out) => {
                report.files += 1;
                report.bytes += out.size;
                return Ok(());
            }
            Err(ClientError::Rejected { reason: RejectReason::IsDirectory, .. }) => {}
            Err(e) => return Err(e),
        }

        let mut stack = vec![(remote.to_string(), dest.join(name), 0usize)];
        let mut first = true;
        while let Some((rdir, ldir, depth)) = stack.pop() {
            if ctx.control.is_cancelled() {
                report.cancelled = true;
                return Ok(());
            }
            if let Err(e) = tokio::fs::create_dir_all(&ldir).await {
                report.failed.push((rdir, format!("{}: {e}", ldir.display())));
                continue;
            }
            report.dirs += 1;
            let entries = match self.list_retrying(ctx.control, &rdir).await {
                Ok(e) => e,
                Err(e) if first => return Err(e),
                Err(ClientError::Cancelled) => {
                    report.cancelled = true;
                    return Ok(());
                }
                Err(e) => {
                    report.failed.push((rdir, e.to_string()));
                    continue;
                }
            };
            first = false;
            for entry in entries {
                let rpath = format!("{rdir}/{}", entry.name);
                if !safe_local_name(&entry.name) {
                    report.failed.push((rpath, "unsafe file name sent by the server".into()));
                    continue;
                }
                match entry.kind {
                    EntryKind::Dir if depth >= MAX_PULL_DEPTH => report.failed.push((rpath, "folders nested too deeply".into())),
                    EntryKind::Dir => stack.push((rpath, ldir.join(&entry.name), depth + 1)),
                    EntryKind::File => match self.pull_file_retrying(ctx, &rpath, &ldir.join(&entry.name)).await {
                        Ok(out) => {
                            report.files += 1;
                            report.bytes += out.size;
                        }
                        Err(ClientError::Cancelled) => {
                            report.cancelled = true;
                            return Ok(());
                        }
                        Err(ClientError::Skipped) => report.skipped.push(rpath),
                        Err(e) => report.failed.push((rpath, e.to_string())),
                    },
                }
            }
        }
        Ok(())
    }

    async fn list_retrying(&self, control: &JobControl, remote: &str) -> Result<Vec<DirEntry>, ClientError> {
        let mut attempts = 0;
        loop {
            let s = self.session(control).await?;
            match s.list_dir(remote).await {
                Ok(v) => return Ok(v),
                Err(e) if e.is_retryable() && attempts < MAX_FILE_ATTEMPTS => {
                    attempts += 1;
                    self.drop_session(&s).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn remote_exists(&self, remote: &str, control: &JobControl) -> Result<bool, ClientError> {
        match self.remote_size(remote, control).await {
            Ok(_) | Err(ClientError::Rejected { reason: RejectReason::IsDirectory, .. }) => Ok(true),
            Err(ClientError::Rejected { reason: RejectReason::NotFound, .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    async fn remote_size(&self, remote: &str, control: &JobControl) -> Result<u64, ClientError> {
        let mut attempts = 0;
        loop {
            if control.is_cancelled() { return Err(ClientError::Cancelled); }
            if control.take_skip() { return Err(ClientError::Skipped); }
            let session = self.session(control).await?;
            let abort = async {
                loop {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    if control.is_cancelled() { return ClientError::Cancelled; }
                    if control.take_skip() { return ClientError::Skipped; }
                }
            };
            let result = tokio::select! {
                result = session.file_size(remote) => result,
                error = abort => Err(error),
            };
            match result {
                Ok(size) => return Ok(size),
                Err(e) if e.is_retryable() && attempts < MAX_FILE_ATTEMPTS => {
                    attempts += 1;
                    self.drop_session(&session).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn compare_remote(&self, remote: &str, local: &Path, ctl: &PushControl<'_>) -> Result<bool, ClientError> {
        let mut attempts = 0;
        loop {
            let session = self.session(ctl.control).await?;
            match session.matches_local(remote, local, ctl).await {
                Ok(equal) => return Ok(equal),
                Err(e) if e.is_retryable() && attempts < MAX_FILE_ATTEMPTS => {
                    attempts += 1;
                    self.drop_session(&session).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    async fn conflict_destination(
        &self, remote: &str, local: &Path, upload: bool, overwrite: bool,
        resolver: Option<&fc_engine::conflict::ConflictResolver>, ctl: &PushControl<'_>,
    ) -> Result<(String, PathBuf, bool), ClientError> {
        use fc_engine::conflict::{ConflictAction, exists, keep_both_path};
        let overwrite = resolver.map_or(overwrite, |r| r.policy == fc_engine::conflict::ConflictPolicy::Replace);
        let original = || (remote.to_owned(), local.to_path_buf(), overwrite);
        let Some(resolver) = resolver else { return Ok(original()); };
        let present = if upload {
            self.remote_exists(remote, ctl.control).await?
        } else {
            // Directory offers must still reach the recursive-download planner.
            self.remote_size(remote, ctl.control).await?;
            exists(local).map_err(|e| ClientError::Local { path: local.into(), source: e })?
        };
        if !present { return Ok(original()); }
        let (source, destination) = if upload { (local, Path::new(remote)) } else { (Path::new(remote), local) };
        let mut different = false;
        loop {
            let action = tokio::task::block_in_place(|| resolver.choose(source, destination, different, ctl.control))
                .map_err(|e| match e {
                    CopyError::Cancelled => ClientError::Cancelled,
                    CopyError::SkippedByUser => ClientError::Skipped,
                    _ => ClientError::Remote(e.to_string()),
                })?;
            match action {
                ConflictAction::Replace => return Ok((remote.into(), local.into(), true)),
                ConflictAction::Skip => return Err(ClientError::Skipped),
                ConflictAction::Cancel => return Err(ClientError::Cancelled),
                ConflictAction::Compare => {
                    if self.compare_remote(remote, local, ctl).await? { return Err(ClientError::Skipped); }
                    different = true;
                }
                ConflictAction::KeepBoth => {
                    for number in 1..u64::MAX {
                        if ctl.control.is_cancelled() { return Err(ClientError::Cancelled); }
                        let candidate = keep_both_path(destination, number);
                        if upload {
                            let candidate = candidate.to_string_lossy().replace('\\', "/");
                            if !self.remote_exists(&candidate, ctl.control).await? { return Ok((candidate, local.into(), false)); }
                        } else if !exists(&candidate).map_err(|e| ClientError::Local { path: candidate.clone(), source: e })? {
                            validate_download_path(&candidate)?;
                            return Ok((remote.into(), candidate, false));
                        }
                    }
                    return Err(ClientError::Remote("keep-both suffixes exhausted".into()));
                }
            }
        }
    }

    /// Download one file. Retries after a dropped connection resume from the bytes on disk.
    async fn pull_file_retrying(&self, ctx: &PullCtx<'_>, remote: &str, local: &Path) -> Result<PullOutcome, ClientError> {
        let result = self.pull_file_with_conflicts(ctx, remote, local).await;
        // Directory discovery is not a completed file.
        if !matches!(result, Err(ClientError::Rejected { reason: RejectReason::IsDirectory, .. })) {
            ctx.progress.finish(remote, result.as_ref().map(|o| o.size));
        }
        result
    }

    async fn pull_file_with_conflicts(&self, ctx: &PullCtx<'_>, remote: &str, local: &Path) -> Result<PullOutcome, ClientError> {
        let ctl = PushControl { control: ctx.control, limiter: ctx.limiter };
        let (_, destination, overwrite) = self.conflict_destination(
            remote, local, false, ctx.opts.overwrite, ctx.opts.conflicts.as_deref(), &ctl,
        ).await?;
        let local = destination.as_path();
        if ctx.opts.conflicts.is_some() {
            for path in [destination.clone(), crate::server::sidecar(local, crate::server::TEMP_SUFFIX), crate::server::sidecar(local, crate::server::ID_SUFFIX)] {
                validate_download_path(&path)?;
            }
        }
        let tally = PullTally { progress: ctx.progress, remote, started: AtomicBool::new(false), high_water: AtomicU64::new(0) };
        let mut attempts = 0;
        let mut mismatch_retried = false;
        loop {
            let s = match self.session(ctx.control).await {
                Ok(s) => s,
                Err(e) => break Err(e),
            };
            let req = PullRequest { remote, local, algo: ctx.opts.algo, overwrite, preserve: ctx.opts.preserve };
            let ctl = PushControl { control: ctx.control, limiter: ctx.limiter };
            let resume = !self.restart_files || attempts > 0 || mismatch_retried;
            match s.pull_file_with_resume(&req, &ctl, &tally, resume).await {
                Ok(out) => break Ok(out),
                Err(e) if e.is_retryable() && attempts < MAX_FILE_ATTEMPTS => {
                    attempts += 1;
                    self.drop_session(&s).await;
                }
                // The partial file is deleted on a mismatch, so one more try starts clean.
                Err(ClientError::PullMismatch { .. }) if !mismatch_retried => {
                    mismatch_retried = true;
                }
                Err(e) => break Err(e),
            }
        }
    }
}

/// Turns absolute offsets (which restart at the resume point after a
/// reconnect) into byte deltas for [`PullProgress`].
struct PullTally<'a> {
    progress: &'a dyn PullProgress,
    remote: &'a str,
    started: AtomicBool,
    high_water: AtomicU64,
}

impl PullObserver for PullTally<'_> {
    fn meta(&self, size: u64) {
        if !self.started.swap(true, Ordering::Relaxed) {
            self.progress.begin(self.remote, size);
        }
    }

    fn received(&self, offset: u64) {
        let prev = self.high_water.fetch_max(offset, Ordering::Relaxed);
        if offset > prev {
            self.progress.advance(offset - prev);
        }
    }
}

impl TransferBackend for RemoteBackend {
    fn describe(&self) -> String {
        self.connector.spec().to_string()
    }

    fn run(&self, plan: &Plan, ctx: &BackendContext<'_>, on_file: &mut dyn FnMut(FileEvent<'_>)) -> Result<BatchReport, CopyError> {
        ctx.progress.set_job_totals(plan.stats.files, plan.stats.bytes, false);
        self.sent.store(0, Ordering::Relaxed);
        self.acked.store(0, Ordering::Relaxed);
        self.publish_link(ctx.progress, false);
        let started = Instant::now();
        let runtime = self.runtime.as_ref().expect("runtime lives until drop");
        let result = runtime.block_on(self.run_plan(plan, ctx, on_file));
        result.map(|mut r| {
            r.elapsed = started.elapsed();
            r
        })
    }
}

/// Counts bytes for one file across retries. The high-water marks make sure a
/// resumed transfer doesn't count bytes already sent.
struct Tally<'a> {
    sent: &'a AtomicU64,
    acked: &'a AtomicU64,
    progress: &'a Progress,
    sent_hw: AtomicU64,
    acked_hw: AtomicU64,
}

impl PushObserver for Tally<'_> {
    fn sent(&self, offset: u64) {
        let prev = self.sent_hw.fetch_max(offset, Ordering::Relaxed);
        if offset > prev {
            self.sent.fetch_add(offset - prev, Ordering::Relaxed);
            self.progress.add_copied(offset - prev);
        }
    }

    fn acked(&self, offset: u64) {
        let prev = self.acked_hw.fetch_max(offset, Ordering::Relaxed);
        if offset > prev {
            self.acked.fetch_add(offset - prev, Ordering::Relaxed);
        }
        // Bytes the server already held (the resume point) count as progress, not as wire traffic.
        let prev_sent = self.sent_hw.fetch_max(offset, Ordering::Relaxed);
        if offset > prev_sent {
            self.progress.add_copied(offset - prev_sent);
        }
    }
}

/// The wire supports SHA-256 and XXH3. MD5 isn't offered remotely, so it falls back to XXH3.
fn wire_algo(a: Algorithm) -> HashAlgo {
    match a {
        Algorithm::Sha256 => HashAlgo::Sha256,
        _ => HashAlgo::Xxh3,
    }
}

fn to_copy_error(e: ClientError, path: &Path) -> CopyError {
    match e {
        ClientError::Cancelled => CopyError::Cancelled,
        ClientError::Skipped => CopyError::SkippedByUser,
        ClientError::Local { path, source } => CopyError::Io { op: Operation::Read, path, source },
        other => CopyError::Backend(format!("{}: {other}", path.display())),
    }
}

/// Sleep for `d`, checking `control` about every 100 ms. Returns false if cancelled.
async fn wait_unless_cancelled(d: Duration, control: &JobControl) -> bool {
    let until = Instant::now() + d;
    loop {
        if control.is_cancelled() {
            return false;
        }
        let now = Instant::now();
        if now >= until {
            return true;
        }
        tokio::time::sleep((until - now).min(Duration::from_millis(100))).await;
    }
}

impl Drop for RemoteBackend {
    /// Shut the runtime down without waiting. A backend can be dropped from
    /// inside async code (a web request removing a job), and dropping a runtime
    /// there would panic.
    fn drop(&mut self) {
        if let Some(rt) = self.runtime.take() {
            rt.shutdown_background();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_supplied_names_cannot_leave_the_download_folder() {
        for evil in ["", ".", "..", "a/b", "../x", "a\\b", "x\0y", "/etc"] {
            assert!(!safe_local_name(evil), "{evil:?}");
        }
        for fine in ["a.txt", "photo 1.jpg", "a..b", ".hidden", "ünïcode.txt"] {
            assert!(safe_local_name(fine), "{fine:?}");
        }
        if cfg!(windows) {
            for evil in ["C:", "a:b", "CON", "name."] {
                assert!(!safe_local_name(evil), "{evil:?}");
            }
        }
    }
}

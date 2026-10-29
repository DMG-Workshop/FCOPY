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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::{StreamExt, stream};
use tokio::runtime::Runtime;
use tokio::sync::Mutex as AsyncMutex;

use fc_engine::{
    Algorithm, BackendContext, BatchReport, CopyError, CopyItem, CopyReport, FileEvent, JobControl, LinkStats,
    Operation, Plan, Progress, TransferBackend,
};

use crate::client::{ClientError, Connector, PushControl, PushObserver, PushRequest, Session, fresh_source_id};
use crate::protocol::HashAlgo;

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
}

impl RemoteBackend {
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

    fn publish_link(&self, progress: &Progress) {
        let mut link = match self.slot.try_lock() {
            Ok(slot) => slot.as_ref().map(|s| s.link_stats()),
            Err(_) => None,
        }
        .unwrap_or_else(|| LinkStats { peer: self.connector.spec().host_port(), ..LinkStats::default() });
        link.sent_bytes = self.sent.load(Ordering::Relaxed);
        link.acked_bytes = self.acked.load(Ordering::Relaxed);
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
        let started = Instant::now();
        let tally = Tally {
            sent: &self.sent,
            acked: &self.acked,
            progress: ctx.progress,
            sent_hw: AtomicU64::new(0),
            acked_hw: AtomicU64::new(0),
        };
        let mut attempts = 0;
        let mut source_id = None;
        let mut mismatch_retried = false;
        loop {
            let s = self.session(ctx.control).await.map_err(|e| to_copy_error(e, &item.src))?;
            let req = PushRequest {
                local: &item.src,
                remote,
                algo: wire_algo(ctx.options.algorithm),
                overwrite: ctx.options.overwrite,
                source_id,
            };
            let ctl = PushControl { control: ctx.control, limiter: ctx.options.rate_limit.as_deref() };
            match s.push_file(&req, &ctl, &tally).await {
                Ok(out) => {
                    return Ok(CopyReport {
                        src: item.src.clone(),
                        dst: PathBuf::from(remote),
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
        self.publish_link(ctx.progress);
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
        .buffer_unordered(SMALL_PARALLEL);
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

impl TransferBackend for RemoteBackend {
    fn describe(&self) -> String {
        self.connector.spec().to_string()
    }

    fn run(&self, plan: &Plan, ctx: &BackendContext<'_>, on_file: &mut dyn FnMut(FileEvent<'_>)) -> Result<BatchReport, CopyError> {
        ctx.progress.set_job_totals(plan.stats.files, plan.stats.bytes, false);
        self.sent.store(0, Ordering::Relaxed);
        self.acked.store(0, Ordering::Relaxed);
        self.publish_link(ctx.progress);
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

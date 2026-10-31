//! Download (pull) end to end: a real agent on loopback QUIC, driven by
//! `Session::pull_file` / `list_dir` and by `RemoteBackend::pull`. Plain
//! `#[test]`s: the backend owns a runtime, and dropping one inside async code
//! would panic. Session-level tests use the agent's runtime through `block_on`.

use std::fs;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

use fc_engine::{Event, JobControl, JobState, Queue, QueueConfig, TransferRequest};
use fc_remote::backend::{PullBackend, PullOptions, PullProgress, RemoteBackend};
use fc_remote::client::{ClientError, Connector, PullRequest, PushControl, RemoteSpec, Session};
use fc_remote::config::{AccountKind, AgentConfig, Share};
use fc_remote::protocol::{EntryKind, HashAlgo, KdfParams, RejectReason};
use fc_remote::server::{Server, ServerHandle};
use fc_remote::tls::Identity;
use tokio::runtime::Runtime;

const MIB: usize = 1024 * 1024;

struct Agent {
    rt: Runtime,
    handle: ServerHandle,
    port: u16,
    fingerprint: String,
    /// Backs both the writable `backup` share and the read-only `ro` share.
    share: tempfile::TempDir,
}

fn agent() -> Agent {
    let rt = Runtime::new().unwrap();
    let share = tempfile::tempdir().unwrap();
    let (handle, port, fingerprint) = rt.block_on(async {
        let mut cfg = AgentConfig::new("nas");
        cfg.listen = "127.0.0.1:0".parse().unwrap();
        cfg.kdf = KdfParams { m_cost: 256, t_cost: 1, p_cost: 1 };
        cfg.shares.push(Share { name: "backup".into(), path: share.path().into(), writable: true });
        cfg.shares.push(Share { name: "ro".into(), path: share.path().into(), writable: false });
        cfg.set_account("alice", AccountKind::Interactive, "password1", None).unwrap();
        let identity = Identity::generate("127.0.0.1").unwrap();
        let server = Server::bind(cfg, &identity).unwrap();
        let handle = server.handle();
        let port = handle.local_addr().unwrap().port();
        let fingerprint = handle.fingerprint().to_string();
        tokio::spawn(server.run());
        (handle, port, fingerprint)
    });
    Agent { rt, handle, port, fingerprint, share }
}

impl Agent {
    fn connector(&self) -> Connector {
        let spec = RemoteSpec::parse(&format!("alice@127.0.0.1:{}:/backup", self.port)).unwrap();
        Connector::new(spec).password("password1").pinned(Some(self.fingerprint.clone()))
    }

    fn backend(&self) -> RemoteBackend {
        RemoteBackend::new(self.connector()).unwrap()
    }

    fn login(&self, token: Option<[u8; 32]>) -> Session {
        self.rt.block_on(Session::open(&self.connector(), token)).expect("login")
    }
}

fn data(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed.wrapping_mul(2_654_435_761).wrapping_add(1);
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

fn set_mtime(path: &Path, secs: u64) {
    let f = fs::OpenOptions::new().write(true).open(path).unwrap();
    f.set_modified(UNIX_EPOCH + Duration::from_secs(secs)).unwrap();
}

fn mtime_secs(path: &Path) -> u64 {
    fs::metadata(path).unwrap().modified().unwrap().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn request<'a>(remote: &'a str, local: &'a Path, overwrite: bool) -> PullRequest<'a> {
    PullRequest { remote, local, algo: HashAlgo::Sha256, overwrite, preserve: true }
}

fn no_leftovers(dir: &Path) {
    for e in walk(dir) {
        let name = e.file_name().unwrap().to_string_lossy().to_string();
        assert!(!name.ends_with(".fcpart") && !name.ends_with(".fcpart.id"), "leftover {}", e.display());
    }
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(walk(&p));
        }
        out.push(p);
    }
    out
}

#[derive(Default)]
struct Counts {
    begun: AtomicU64,
    bytes: AtomicU64,
}

impl PullProgress for Counts {
    fn begin(&self, _remote: &str, _size: u64) {
        self.begun.fetch_add(1, Ordering::Relaxed);
    }
    fn advance(&self, bytes: u64) {
        self.bytes.fetch_add(bytes, Ordering::Relaxed);
    }
}

#[test]
fn a_folder_downloads_byte_identical_with_times_and_empty_folders() {
    let a = agent();
    let tree = a.share.path().join("tree");
    fs::create_dir_all(tree.join("sub/empty")).unwrap();
    fs::write(tree.join("a.txt"), b"hello").unwrap();
    fs::write(tree.join("zero"), b"").unwrap();
    fs::write(tree.join("sub/small.bin"), data(300_000, 1)).unwrap();
    fs::write(tree.join("big.bin"), data(5 * MIB + 123, 2)).unwrap();
    set_mtime(&tree.join("a.txt"), 1_577_836_800);

    let out = tempfile::tempdir().unwrap();
    let progress = Counts::default();
    let report = a
        .backend()
        .pull("backup/tree", out.path(), &PullOptions::default(), &JobControl::new(), None, &progress)
        .expect("pull");
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!((report.files, report.bytes), (4, 5 + 300_000 + 5 * MIB as u64 + 123));
    assert_eq!(progress.begun.load(Ordering::Relaxed), 4);
    assert_eq!(progress.bytes.load(Ordering::Relaxed), report.bytes);

    let got = out.path().join("tree");
    for rel in ["a.txt", "zero", "sub/small.bin", "big.bin"] {
        assert_eq!(fs::read(got.join(rel)).unwrap(), fs::read(tree.join(rel)).unwrap(), "{rel}");
    }
    assert!(got.join("sub/empty").is_dir());
    assert_eq!(mtime_secs(&got.join("a.txt")), 1_577_836_800, "modification time is preserved");
    no_leftovers(out.path());
    a.handle.shutdown();
}

#[test]
fn a_file_goes_to_the_exact_path_or_into_an_existing_folder() {
    let a = agent();
    fs::write(a.share.path().join("one.bin"), data(70_000, 5)).unwrap();
    let out = tempfile::tempdir().unwrap();
    let b = a.backend();
    let opts = PullOptions::default();

    let exact = out.path().join("renamed.bin");
    b.pull("backup/one.bin", &exact, &opts, &JobControl::new(), None, &()).unwrap();
    assert_eq!(fs::read(&exact).unwrap(), fs::read(a.share.path().join("one.bin")).unwrap());

    let folder = out.path().join("into");
    fs::create_dir(&folder).unwrap();
    b.pull("backup/one.bin", &folder, &opts, &JobControl::new(), None, &()).unwrap();
    assert!(folder.join("one.bin").is_file());
    a.handle.shutdown();
}

#[test]
fn an_existing_file_is_kept_unless_overwrite_is_asked_for() {
    let a = agent();
    fs::write(a.share.path().join("f.txt"), b"from the server").unwrap();
    let out = tempfile::tempdir().unwrap();
    let local = out.path().join("f.txt");
    fs::write(&local, b"mine").unwrap();
    let b = a.backend();

    let err = b.pull("backup/f.txt", &local, &PullOptions::default(), &JobControl::new(), None, &()).unwrap_err();
    assert!(matches!(err, ClientError::Local { .. }), "{err:?}");
    assert_eq!(fs::read(&local).unwrap(), b"mine");

    let opts = PullOptions { overwrite: true, ..Default::default() };
    b.pull("backup/f.txt", &local, &opts, &JobControl::new(), None, &()).unwrap();
    assert_eq!(fs::read(&local).unwrap(), b"from the server");
    no_leftovers(out.path());
    a.handle.shutdown();
}

#[test]
fn repulling_a_folder_over_an_existing_copy_walks_into_it() {
    let a = agent();
    fs::create_dir_all(a.share.path().join("d")).unwrap();
    fs::write(a.share.path().join("d/x.txt"), b"x").unwrap();
    let out = tempfile::tempdir().unwrap();
    let b = a.backend();
    let opts = PullOptions { overwrite: true, ..Default::default() };
    for _ in 0..2 {
        let r = b.pull("backup/d", out.path(), &opts, &JobControl::new(), None, &()).unwrap();
        assert!(r.failed.is_empty(), "{:?}", r.failed);
        assert_eq!(fs::read(out.path().join("d/x.txt")).unwrap(), b"x");
    }
    a.handle.shutdown();
}

#[test]
fn missing_paths_traversal_and_unknown_shares_are_refused() {
    let a = agent();
    let out = tempfile::tempdir().unwrap();
    let s = a.login(None);
    let control = JobControl::new();
    let ctl = PushControl { control: &control, limiter: None };

    for (remote, want) in [
        ("backup/missing.bin", RejectReason::NotFound),
        ("backup/../etc/passwd", RejectReason::BadPath),
        ("nope/x", RejectReason::BadPath),
    ] {
        let local = out.path().join("x");
        let err = a.rt.block_on(s.pull_file(&request(remote, &local, true), &ctl, &())).unwrap_err();
        match err {
            ClientError::Rejected { reason, .. } => assert_eq!(reason, want, "{remote}"),
            other => panic!("{remote}: {other:?}"),
        }
        assert!(!local.exists());
    }
    let err = a.rt.block_on(s.list_dir("backup/missing")).unwrap_err();
    assert!(matches!(err, ClientError::Rejected { reason: RejectReason::NotFound, .. }), "{err:?}");
    fs::write(a.share.path().join("plain"), b"p").unwrap();
    let err = a.rt.block_on(s.list_dir("backup/plain")).unwrap_err();
    assert!(matches!(err, ClientError::Rejected { reason: RejectReason::NotDirectory, .. }), "{err:?}");
    a.handle.shutdown();
}

#[test]
fn a_read_only_share_can_be_downloaded_from() {
    let a = agent();
    fs::write(a.share.path().join("r.bin"), data(1000, 9)).unwrap();
    let out = tempfile::tempdir().unwrap();
    a.backend().pull("ro/r.bin", &out.path().join("r.bin"), &PullOptions::default(), &JobControl::new(), None, &()).unwrap();
    assert_eq!(fs::read(out.path().join("r.bin")).unwrap(), fs::read(a.share.path().join("r.bin")).unwrap());
    a.handle.shutdown();
}

#[cfg(unix)]
#[test]
fn a_symlink_out_of_the_share_is_neither_listed_nor_served() {
    let a = agent();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret"), b"secret").unwrap();
    std::os::unix::fs::symlink(outside.path().join("secret"), a.share.path().join("link")).unwrap();
    std::os::unix::fs::symlink(outside.path(), a.share.path().join("dirlink")).unwrap();
    fs::write(a.share.path().join("ok"), b"ok").unwrap();

    let s = a.login(None);
    let names: Vec<_> = a.rt.block_on(s.list_dir("backup")).unwrap().into_iter().map(|e| e.name).collect();
    assert_eq!(names, ["ok"], "escaping links are hidden");

    let control = JobControl::new();
    let ctl = PushControl { control: &control, limiter: None };
    let out = tempfile::tempdir().unwrap();
    for remote in ["backup/link", "backup/dirlink/secret"] {
        let err = a.rt.block_on(s.pull_file(&request(remote, &out.path().join("s"), true), &ctl, &())).unwrap_err();
        assert!(matches!(err, ClientError::Rejected { reason: RejectReason::BadPath, .. }), "{remote}: {err:?}");
    }
    assert!(!out.path().join("s").exists());
    a.handle.shutdown();
}

#[test]
fn listings_are_sorted_typed_and_hide_partial_uploads() {
    let a = agent();
    fs::create_dir(a.share.path().join("b_dir")).unwrap();
    fs::write(a.share.path().join("c.txt"), b"12345").unwrap();
    fs::write(a.share.path().join("a.txt"), b"1").unwrap();
    fs::write(a.share.path().join("half.bin.fcpart"), b"partial").unwrap();
    fs::write(a.share.path().join("half.bin.fcpart.id"), [0u8; 16]).unwrap();

    let s = a.login(None);
    let list = a.rt.block_on(s.list_dir("backup")).unwrap();
    let got: Vec<_> = list.iter().map(|e| (e.name.as_str(), e.kind, e.size)).collect();
    assert_eq!(got, [("a.txt", EntryKind::File, 1), ("b_dir", EntryKind::Dir, 0), ("c.txt", EntryKind::File, 5)]);
    a.handle.shutdown();
}

#[test]
fn a_dropped_connection_resumes_the_download_from_the_partial_file() {
    let a = agent();
    let bytes = data(6 * MIB, 3);
    fs::write(a.share.path().join("big.bin"), &bytes).unwrap();
    let out = tempfile::tempdir().unwrap();
    let local = out.path().join("big.bin");
    let control = JobControl::new();
    let ctl = PushControl { control: &control, limiter: None };

    a.handle.hooks().drop_pull_after_bytes.store((5 * MIB / 2) as u64, Ordering::SeqCst);
    let s = a.login(None);
    let token = s.token();
    let err = a.rt.block_on(s.pull_file(&request("backup/big.bin", &local, false), &ctl, &())).unwrap_err();
    assert!(err.is_retryable(), "{err:?}");
    assert!(!local.exists(), "nothing is committed before the file is complete");
    let partial = fs::metadata(out.path().join("big.bin.fcpart")).unwrap().len();
    assert!(partial > 0, "the partial file is kept for resume");

    let s = a.login(Some(token));
    let done = a.rt.block_on(s.pull_file(&request("backup/big.bin", &local, false), &ctl, &())).unwrap();
    assert!(done.resumed_from > 0 && done.resumed_from <= partial, "resumed from {} (partial {partial})", done.resumed_from);
    assert_eq!(fs::read(&local).unwrap(), bytes, "byte-identical after the resume");
    no_leftovers(out.path());
    a.handle.shutdown();
}

#[test]
fn a_changed_remote_file_is_not_resumed_from_a_stale_partial() {
    let a = agent();
    let remote_file = a.share.path().join("f.bin");
    fs::write(&remote_file, data(4 * MIB, 1)).unwrap();
    set_mtime(&remote_file, 1_600_000_000);
    let out = tempfile::tempdir().unwrap();
    let local = out.path().join("f.bin");
    let control = JobControl::new();
    let ctl = PushControl { control: &control, limiter: None };

    a.handle.hooks().drop_pull_after_bytes.store(2 * MIB as u64, Ordering::SeqCst);
    let s = a.login(None);
    assert!(a.rt.block_on(s.pull_file(&request("backup/f.bin", &local, false), &ctl, &())).is_err());

    // Same size, different content and time: a new version.
    let new_bytes = data(4 * MIB, 2);
    fs::write(&remote_file, &new_bytes).unwrap();
    set_mtime(&remote_file, 1_700_000_000);
    let s = a.login(None);
    let done = a.rt.block_on(s.pull_file(&request("backup/f.bin", &local, false), &ctl, &())).unwrap();
    assert_eq!(done.resumed_from, 0, "the stale partial is discarded");
    assert_eq!(fs::read(&local).unwrap(), new_bytes);
    a.handle.shutdown();
}

#[test]
fn retry_discards_compatible_partial_but_keeps_real_identity_for_reconnect() {
    fn leave_partial(a: &Agent, req: &PullRequest<'_>, resume: bool) {
        struct StopAfterPrefix<'a>(&'a JobControl);
        impl fc_remote::client::PullObserver for StopAfterPrefix<'_> {
            fn received(&self, bytes: u64) {
                if bytes >= (2 * MIB) as u64 { self.0.cancel(); }
            }
        }
        // A QUIC close may discard even acknowledged data before the reader sees
        // it. Cancel after application progress to create a deterministic fixture.
        let control = JobControl::new();
        let ctl = PushControl { control: &control, limiter: None };
        let s = a.login(None);
        let result = a.rt.block_on(s.pull_file_with_resume(req, &ctl, &StopAfterPrefix(&control), resume));
        assert!(matches!(result, Err(ClientError::Cancelled)), "{result:?}");
    }

    let a = agent();
    let bytes = data(6 * MIB, 7);
    fs::write(a.share.path().join("retry.bin"), &bytes).unwrap();
    let out = tempfile::tempdir().unwrap();
    let local = out.path().join("retry.bin");
    let control = JobControl::new();
    let ctl = PushControl { control: &control, limiter: None };
    let req = request("backup/retry.bin", &local, false);
    for resume in [true, false] {
        leave_partial(&a, &req, resume);
        assert!(fs::metadata(out.path().join("retry.bin.fcpart")).unwrap().len() > 0);
    }
    let s = a.login(None);
    let done = a.rt.block_on(s.pull_file(&req, &ctl, &())).unwrap();
    assert!(done.resumed_from > 0, "a reconnect after a fresh retry can resume");
    assert_eq!(fs::read(local).unwrap(), bytes);
    no_leftovers(out.path());

    // A compatible partial must be ignored when Retry is explicitly chosen.
    let local = out.path().join("retry-again.bin");
    let req = request("backup/retry.bin", &local, false);
    leave_partial(&a, &req, true);
    let s = a.login(None);
    let done = a.rt.block_on(s.pull_file_with_resume(&req, &ctl, &(), false)).unwrap();
    assert_eq!(done.resumed_from, 0);
    assert_eq!(fs::read(local).unwrap(), bytes);
    a.handle.shutdown();
}

#[test]
fn pausing_the_agent_stalls_a_download_until_it_resumes() {
    let a = agent();
    let bytes = data(3 * MIB, 4);
    fs::write(a.share.path().join("p.bin"), &bytes).unwrap();
    let out = tempfile::tempdir().unwrap();
    let local = out.path().join("p.bin");

    a.handle.pause_all();
    let s = a.login(None);
    let control = JobControl::new();
    let ctl = PushControl { control: &control, limiter: None };
    let result = a.rt.block_on(async {
        let req = request("backup/p.bin", &local, false);
        let pull = s.pull_file(&req, &ctl, &());
        tokio::pin!(pull);
        let stalled = tokio::time::timeout(Duration::from_millis(600), &mut pull).await;
        assert!(stalled.is_err(), "a paused agent must not deliver the file");
        assert!(!local.exists());
        a.handle.resume_all();
        tokio::time::timeout(Duration::from_secs(20), pull).await.expect("resumes").expect("pull")
    });
    assert_eq!(result.size, bytes.len() as u64);
    assert_eq!(fs::read(&local).unwrap(), bytes);
    a.handle.shutdown();
}

#[test]
fn cancel_stops_a_download_and_keeps_the_session() {
    let a = agent();
    fs::write(a.share.path().join("c.bin"), data(8 * MIB, 6)).unwrap();
    let out = tempfile::tempdir().unwrap();
    let local = out.path().join("c.bin");

    let s = a.login(None);
    let control = JobControl::new();
    control.cancel();
    let ctl = PushControl { control: &control, limiter: None };
    let err = a.rt.block_on(s.pull_file(&request("backup/c.bin", &local, false), &ctl, &())).unwrap_err();
    assert!(matches!(err, ClientError::Cancelled), "{err:?}");
    assert!(!local.exists());
    assert!(s.is_connected(), "cancelling a file does not end the session");
    a.handle.shutdown();
}

fn queued_pull(a: &Agent, queue: &Queue, sources: &[&str], dest: &Path) -> u64 {
    let backend = PullBackend::new(std::sync::Arc::new(a.backend()), dest.into());
    queue.add(TransferRequest::to_backend(sources.iter().map(Into::into).collect(), std::sync::Arc::new(backend)))
}

#[test]
fn download_conflicts_preserve_originals_and_report_keep_both_path() {
    use fc_engine::conflict::{ConflictPolicy as P, ConflictAction as A, ConflictChoice};
    use std::sync::Arc;
    let a = agent();
    fs::write(a.share.path().join("note.txt"), b"new").unwrap();
    let out = tempfile::tempdir().unwrap();
    for (policy, identical) in [(P::Skip, false), (P::Replace, false), (P::KeepBoth, false), (P::Compare, true), (P::Compare, false)] {
        fs::write(out.path().join("note.txt"), if identical { b"new" } else { b"old" }).unwrap();
        let (q, events) = Queue::start(QueueConfig::default());
        let backend = PullBackend::new(Arc::new(a.backend()), out.path().into());
        let id = q.add(TransferRequest::to_backend(vec!["backup/note.txt".into()], Arc::new(backend)).with_conflict_policy(policy));
        if policy == P::Compare && !identical {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(conflict) = q.job(id).unwrap().conflict {
                    assert!(conflict.details.different);
                    assert!(q.resolve_conflict(id, conflict.id, ConflictChoice { action: A::KeepBoth, apply_to_all: false }));
                    break;
                }
                let event = events.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())).unwrap();
                assert!(!matches!(event, Event::Finished { .. }), "{event:?}");
            }
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        let mut copied_path = None;
        let state = loop {
            match events.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())).unwrap() {
                Event::FileCopied { dst, .. } => copied_path = Some(dst),
                Event::Finished { state, .. } => break state,
                _ => {}
            }
        };
        assert_eq!(state, if policy == P::Skip || identical { JobState::CompletedWithErrors } else { JobState::Completed });
        assert_eq!(fs::read(out.path().join("note.txt")).unwrap(), if policy == P::Replace || identical { b"new" } else { b"old" });
        if policy == P::KeepBoth || (policy == P::Compare && !identical) {
            let n = if policy == P::KeepBoth { 1 } else { 2 };
            let dest = out.path().join(format!("note (copy {n}).txt"));
            assert_eq!(copied_path.as_ref(), Some(&dest));
            assert_eq!(fs::read(dest).unwrap(), b"new");
        }
        assert_eq!(q.job(id).unwrap().telemetry.unwrap().files_done, 1);
        q.shutdown();
    }
    a.handle.shutdown();
}

#[test]
fn direct_pull_conflicts_and_comparison_cancel_are_responsive() {
    use fc_engine::conflict::{ConflictPolicy as P, ConflictResolver};
    use std::sync::Arc;
    let a = agent();
    fs::write(a.share.path().join("note"), b"new").unwrap();
    let out = tempfile::tempdir().unwrap();
    let local = out.path().join("note");
    fs::write(&local, b"old").unwrap();
    let backend = a.backend();
    let opts = PullOptions { conflicts: Some(Arc::new(ConflictResolver::new(P::KeepBoth))), ..Default::default() };
    let report = backend.pull("backup/note", &local, &opts, &JobControl::new(), None, &()).unwrap();
    assert_eq!(report.files, 1);
    assert!(report.failed.is_empty());
    assert_eq!(fs::read(&local).unwrap(), b"old");
    assert_eq!(fs::read(out.path().join("note (copy 1)")).unwrap(), b"new");
    let opts = PullOptions { conflicts: Some(Arc::new(ConflictResolver::new(P::Compare))), ..Default::default() };
    let error = backend.pull("backup/note", &local, &opts, &JobControl::new(), None, &()).unwrap_err();
    assert!(matches!(error, ClientError::Remote(ref message) if message.contains("interactive decision")), "{error:?}");
    assert_eq!(fs::read(&local).unwrap(), b"old");

    let bytes = data(4 * MIB, 24);
    fs::write(&local, &bytes).unwrap();
    fs::write(a.share.path().join("note"), &bytes).unwrap();
    let session = a.login(None);
    let control = JobControl::new();
    let limiter = fc_engine::RateLimiter::new(Some(1024));
    a.rt.block_on(async {
        let cancel = async {
            tokio::time::sleep(Duration::from_millis(100)).await;
            control.cancel();
        };
        let ctl = PushControl { control: &control, limiter: Some(&limiter) };
        let compare = session.matches_local("backup/note", &local, &ctl);
        let (result, ()) = tokio::time::timeout(Duration::from_secs(3), async { tokio::join!(compare, cancel) }).await.unwrap();
        assert!(matches!(result, Err(ClientError::Cancelled)), "{result:?}");
    });
    no_leftovers(out.path());
    a.handle.shutdown();
}

fn wait_finished(events: &std::sync::mpsc::Receiver<Event>, id: u64) -> (JobState, fc_engine::JobSummary) {
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let event = events.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())).expect("job timed out");
        if let Event::Finished { job, state, summary } = event {
            if job == id { return (state, summary); }
        }
    }
}

#[test]
fn queued_download_plans_remote_sources_reports_totals_and_recovers_a_drop() {
    let a = agent();
    fs::create_dir_all(a.share.path().join("tree/sub/empty")).unwrap();
    let bytes = data(6 * MIB, 13);
    fs::write(a.share.path().join("tree/big.bin"), &bytes).unwrap();
    fs::write(a.share.path().join("tree/sub/note"), b"hello").unwrap();
    a.handle.hooks().drop_pull_after_bytes.store(3 * MIB as u64, Ordering::SeqCst);
    let out = tempfile::tempdir().unwrap();
    let (queue, events) = Queue::start(QueueConfig::default());
    let id = queued_pull(&a, &queue, &["backup/tree"], out.path());
    let (state, summary) = wait_finished(&events, id);
    assert_eq!(state, JobState::Completed);
    assert_eq!((summary.files_copied, summary.bytes_copied), (2, bytes.len() as u64 + 5));
    let job = queue.job(id).unwrap();
    assert_eq!((job.files_total, job.bytes_total), (2, summary.bytes_copied));
    let telemetry = job.telemetry.unwrap();
    assert_eq!(telemetry.files_done, 2, "each file counted once");
    assert_eq!(telemetry.bytes_copied, summary.bytes_copied, "resume is not double counted");
    assert!(telemetry.link.unwrap().reconnects >= 1);
    assert_eq!(fs::read(out.path().join("tree/big.bin")).unwrap(), bytes);
    assert!(out.path().join("tree/sub/empty").is_dir());
    no_leftovers(out.path());
    queue.shutdown();
    a.handle.shutdown();
}

#[test]
fn queued_download_obeys_pause_resume_live_limit_skip_and_continues_queue() {
    let a = agent();
    fs::write(a.share.path().join("big"), data(4 * MIB, 14)).unwrap();
    fs::write(a.share.path().join("small"), b"next").unwrap();
    let out = tempfile::tempdir().unwrap();
    let (queue, events) = Queue::start(QueueConfig {
        speed_limit: Some(MIB as u64), telemetry_interval: Duration::from_millis(20), ..Default::default()
    });
    queue.pause();
    let id = queued_pull(&a, &queue, &["backup/big"], out.path());
    let next = queued_pull(&a, &queue, &["backup/small"], out.path());
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(queue.job(id).unwrap().state, JobState::Pending);
    assert!(!out.path().join("big.fcpart").exists());
    queue.resume();
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        assert!(std::time::Instant::now() < deadline);
        if queue.job(id).unwrap().telemetry.is_some_and(|s| s.bytes_copied >= MIB as u64) { break; }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!out.path().join("big").exists(), "rate limit must delay completion");
    queue.pause_job(id);
    std::thread::sleep(Duration::from_millis(1200));
    let before = queue.job(id).unwrap().telemetry.unwrap().bytes_copied;
    std::thread::sleep(Duration::from_millis(250));
    assert_eq!(queue.job(id).unwrap().telemetry.unwrap().bytes_copied, before);
    queue.resume_job(id);
    assert!(queue.skip_current_file());
    let (state, summary) = wait_finished(&events, id);
    assert_eq!(state, JobState::CompletedWithErrors);
    assert_eq!(summary.files_skipped, 1);
    queue.set_speed_limit(None);
    assert_eq!(wait_finished(&events, next).0, JobState::Completed);
    assert_eq!(fs::read(out.path().join("small")).unwrap(), b"next");
    assert!(!out.path().join("big").exists());
    queue.shutdown();
    a.handle.shutdown();
}

#[test]
fn queued_download_cancels_while_remote_paused_and_rejects_duplicate_targets() {
    let a = agent();
    fs::write(a.share.path().join("file"), data(2 * MIB, 15)).unwrap();
    let out = tempfile::tempdir().unwrap();
    a.handle.pause_all();
    let (queue, events) = Queue::start(QueueConfig::default());
    let id = queued_pull(&a, &queue, &["backup/file"], out.path());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while queue.job(id).unwrap().telemetry.is_none() {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }

    queue.cancel(id);
    assert_eq!(wait_finished(&events, id).0, JobState::Cancelled);
    assert!(!out.path().join("file").exists());
    a.handle.resume_all();
    let id = queued_pull(&a, &queue, &["backup/file", "ro/file"], out.path());
    assert!(matches!(wait_finished(&events, id).0, JobState::Failed(e) if e.contains("duplicate")));
    queue.shutdown();
    a.handle.shutdown();
}

#[test]
fn raising_the_live_download_limit_interrupts_the_old_reservation() {
    let a = agent();
    fs::write(a.share.path().join("big"), data(2 * MIB, 16)).unwrap();
    let out = tempfile::tempdir().unwrap();
    let (queue, events) = Queue::start(QueueConfig {
        speed_limit: Some(32 * 1024), telemetry_interval: Duration::from_millis(20), ..Default::default()
    });
    let id = queued_pull(&a, &queue, &["backup/big"], out.path());
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !queue.job(id).unwrap().telemetry.is_some_and(|s| s.bytes_copied >= MIB as u64) {
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(!out.path().join("big").exists());
    let changed = std::time::Instant::now();
    queue.set_speed_limit(None);
    assert_eq!(wait_finished(&events, id).0, JobState::Completed);
    assert!(changed.elapsed() < Duration::from_secs(5), "old 32-second reservation must be interrupted");
    queue.shutdown();
    a.handle.shutdown();
}

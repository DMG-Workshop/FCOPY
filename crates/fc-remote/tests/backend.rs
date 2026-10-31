//! A whole job from the transfer queue, through [`RemoteBackend`], to a real
//! agent on loopback QUIC. Plain `#[test]`s: the backend owns a runtime, and
//! dropping one inside async code would panic.

use std::sync::Arc;
use std::time::Duration;

use fc_engine::{CopyOptions, Event, JobId, JobState, Queue, QueueConfig, TransferRequest};
use fc_remote::backend::RemoteBackend;
use fc_remote::client::{Connector, RemoteSpec};
use fc_remote::config::{AccountKind, AgentConfig, Share};
use fc_remote::protocol::KdfParams;
use fc_remote::server::{Server, ServerHandle};
use fc_remote::tls::Identity;
use tokio::runtime::Runtime;

struct Agent {
    _rt: Runtime,
    handle: ServerHandle,
    port: u16,
    fingerprint: String,
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
        cfg.set_account("alice", AccountKind::Interactive, "agent-pass-1", None).unwrap();
        let identity = Identity::generate("127.0.0.1").unwrap();
        let server = Server::bind(cfg, &identity).unwrap();
        let handle = server.handle();
        let port = handle.local_addr().unwrap().port();
        let fingerprint = handle.fingerprint().to_string();
        tokio::spawn(server.run());
        (handle, port, fingerprint)
    });
    Agent { _rt: rt, handle, port, fingerprint, share }
}

fn backend(a: &Agent, fingerprint: &str) -> Arc<RemoteBackend> {
    let spec = RemoteSpec::parse(&format!("alice@127.0.0.1:{}:/backup/dest", a.port)).unwrap();
    let connector = Connector::new(spec).password("agent-pass-1").pinned(Some(fingerprint.to_string()));
    Arc::new(RemoteBackend::new(connector).unwrap())
}

#[test]
fn upload_conflict_policies_and_compare_different_keep_both() {
    use fc_engine::conflict::{ConflictPolicy as P, ConflictAction as A, ConflictChoice};
    let a = agent();
    let src_dir = tempfile::tempdir().unwrap();
    let src = src_dir.path().join("note.txt");
    let dest_dir = a.share.path().join("dest");
    std::fs::create_dir(&dest_dir).unwrap();
    std::fs::write(&src, b"new").unwrap();
    for (policy, identical) in [(P::Skip, false), (P::Replace, false), (P::KeepBoth, false), (P::Compare, true), (P::Compare, false)] {
        std::fs::write(dest_dir.join("note.txt"), if identical { b"new" } else { b"old" }).unwrap();
        let (q, rx) = queue();
        let job = q.add(TransferRequest::to_backend(vec![src.clone()], backend(&a, &a.fingerprint)).with_conflict_policy(policy));
        if policy == P::Compare && !identical {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(conflict) = q.job(job).unwrap().conflict {
                    assert!(conflict.details.different);
                    assert!(q.resolve_conflict(job, conflict.id, ConflictChoice { action: A::KeepBoth, apply_to_all: true }));
                    break;
                }
                let event = rx.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())).unwrap();
                assert!(!matches!(event, Event::Finished { .. }), "{event:?}");
            }
        }
        assert_eq!(finished(&rx, job), if policy == P::Skip || identical { JobState::CompletedWithErrors } else { JobState::Completed });
        assert_eq!(std::fs::read(dest_dir.join("note.txt")).unwrap(), if policy == P::Replace || identical { b"new" } else { b"old" });
        if policy == P::KeepBoth || (policy == P::Compare && !identical) {
            let n = if policy == P::KeepBoth { 1 } else { 2 };
            assert_eq!(std::fs::read(dest_dir.join(format!("note (copy {n}).txt"))).unwrap(), b"new");
        }
        q.shutdown();
    }
    a.handle.shutdown();
}

fn queue() -> (Queue, std::sync::mpsc::Receiver<Event>) {
    Queue::start(QueueConfig {
        telemetry_interval: Duration::from_millis(20),
        options: CopyOptions { chunk_size: 64 * 1024, ..Default::default() },
        ..Default::default()
    })
}

/// Wait for a job to finish and return its state.
fn finished(rx: &std::sync::mpsc::Receiver<Event>, job: JobId) -> JobState {
    let deadline = std::time::Instant::now() + Duration::from_secs(60);
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        match rx.recv_timeout(left).expect("job did not finish in time") {
            Event::Finished { job: j, state, .. } if j == job => return state,
            _ => {}
        }
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

#[test]
fn a_tree_job_lands_on_the_agent_byte_identical() {
    let a = agent();
    let src_root = tempfile::tempdir().unwrap();
    let src = src_root.path().join("src");
    std::fs::create_dir_all(src.join("sub/empty")).unwrap();
    std::fs::write(src.join("a.txt"), b"hello").unwrap();
    std::fs::write(src.join("sub/small.bin"), data(300_000, 1)).unwrap();
    std::fs::write(src.join("big.bin"), data(6 * 1024 * 1024, 2)).unwrap();

    let (q, rx) = queue();
    let job = q.add(TransferRequest::to_backend(vec![src.clone()], backend(&a, &a.fingerprint)));
    assert_eq!(finished(&rx, job), JobState::Completed);
    q.shutdown();

    let dest = a.share.path().join("dest/src");
    assert_eq!(std::fs::read(dest.join("a.txt")).unwrap(), b"hello");
    assert_eq!(std::fs::read(dest.join("sub/small.bin")).unwrap(), std::fs::read(src.join("sub/small.bin")).unwrap());
    assert_eq!(std::fs::read(dest.join("big.bin")).unwrap(), std::fs::read(src.join("big.bin")).unwrap());
    assert!(dest.join("sub/empty").is_dir(), "empty folders are created");
    a.handle.shutdown();
}

#[test]
fn a_dropped_connection_mid_file_reconnects_and_resumes() {
    let a = agent();
    let src_root = tempfile::tempdir().unwrap();
    let src = src_root.path().join("big.bin");
    let bytes = data(6 * 1024 * 1024, 3);
    std::fs::write(&src, &bytes).unwrap();

    // The agent cuts the connection once 2.5 MiB has arrived.
    a.handle.hooks().drop_after_bytes.store((5 * 1024 * 1024 / 2) as u64, std::sync::atomic::Ordering::SeqCst);

    let (q, rx) = queue();
    let job = q.add(TransferRequest::to_backend(vec![src.clone()], backend(&a, &a.fingerprint)));
    assert_eq!(finished(&rx, job), JobState::Completed, "job should recover on its own");

    let landed = std::fs::read(a.share.path().join("dest/big.bin")).unwrap();
    assert_eq!(landed, bytes, "byte-identical after the reconnect");

    let link = q.job(job).unwrap().telemetry.and_then(|t| t.link).expect("link telemetry");
    assert!(link.reconnects >= 1, "reconnect is counted: {link:?}");
    q.shutdown();
    a.handle.shutdown();
}

#[test]
fn recovered_upload_resumes_but_retry_starts_at_zero_with_a_new_backend() {
    use fc_remote::client::{PushControl, PushRequest, Session};
    use fc_remote::protocol::HashAlgo;
    use fc_remote::recovery::{Action, Job, RecoveryStore};

    let a = agent();
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("recover.bin");
    let bytes = data(6 * 1024 * 1024, 17);
    std::fs::write(&source, &bytes).unwrap();
    for action in [Action::Resume, Action::Retry] {
        let remote = backend(&a, &a.fingerprint);
        let descriptor = remote.recovery_destination(None).unwrap();
        let request = TransferRequest::to_backend(vec![source.clone()], remote);
        let job = Job::new(&request, descriptor, None, &CopyOptions::default()).unwrap();
        let path = root.path().join("recovery.toml");
        let mut store = RecoveryStore::open(Some(path.clone())).unwrap();
        let saved_id = store.add(job).unwrap();
        drop(request);
        drop(store);

        // A previous process left a compatible, fsynced partial upload.
        a._rt.block_on(async {
            let spec = RemoteSpec::parse(&format!("alice@127.0.0.1:{}:/backup/dest", a.port)).unwrap();
            let connector = Connector::new(spec).password("agent-pass-1").pinned(Some(a.fingerprint.clone()));
            let session = Session::open(&connector, None).await.unwrap();
            session.make_dir("backup/dest").await.unwrap();
            a.handle.hooks().drop_after_bytes.store(2 * 1024 * 1024, std::sync::atomic::Ordering::SeqCst);
            let control = fc_engine::JobControl::new();
            let req = PushRequest { local: &source, remote: "backup/dest/recover.bin", algo: HashAlgo::Xxh3, overwrite: true, source_id: None };
            assert!(session.push_file(&req, &PushControl { control: &control, limiter: None }, &()).await.unwrap_err().is_retryable());
        });
        assert!(a.share.path().join("dest/recover.bin.fcpart").exists());
        let prior_sessions: std::collections::HashSet<_> = a.handle.sessions().iter().map(|session| session.id).collect();
        let store = RecoveryStore::open(Some(path)).unwrap();
        let request = store.get(saved_id).unwrap().request(action, "agent-pass-1".into()).unwrap();
        let (queue, events) = queue();
        let id = queue.add(request);
        assert_eq!(finished(&events, id), JobState::Completed);
        let active = a.handle.sessions();
        let received: u64 = active.iter().filter(|s| !prior_sessions.contains(&s.id) && s.files_completed > 0).map(|s| s.bytes_received).sum();
        if action == Action::Resume {
            assert!(received > 0 && received < bytes.len() as u64, "resume must transmit only the remaining bytes, received {received}");
        } else {
            assert_eq!(received, bytes.len() as u64, "Retry must transmit from offset zero");
        }
        assert_eq!(std::fs::read(a.share.path().join("dest/recover.bin")).unwrap(), bytes);
        queue.shutdown();
        drop(queue);
        std::fs::remove_file(a.share.path().join("dest/recover.bin")).unwrap();
    }
    a.handle.shutdown();
}

#[test]
fn an_identity_change_fails_the_job_without_retrying_forever() {
    let a = agent();
    let src_root = tempfile::tempdir().unwrap();
    let src = src_root.path().join("x.txt");
    std::fs::write(&src, b"x").unwrap();

    let (q, rx) = queue();
    let started = std::time::Instant::now();
    let job = q.add(TransferRequest::to_backend(vec![src], backend(&a, "SHA256:not-this-agent")));
    match finished(&rx, job) {
        JobState::Failed(msg) => assert!(msg.contains("HOST IDENTITY") || msg.contains("identity"), "{msg}"),
        other => panic!("expected Failed, got {other:?}"),
    }
    assert!(started.elapsed() < Duration::from_secs(30), "a refused identity is not retried for minutes");
    q.shutdown();
    a.handle.shutdown();
}

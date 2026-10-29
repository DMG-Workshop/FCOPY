//! End-to-end tests: a real agent on 127.0.0.1 (QUIC + TLS 1.3) driven by the
//! client. Covers authentication, pinning, path security, pause, cancel/skip,
//! and a forced connection drop in the middle of a file with resume.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use fc_engine::{hash_file, Algorithm, JobControl, RateLimiter};
use fc_remote::client::{ClientError, Connector, PushControl, PushOutcome, PushRequest, RemoteSpec, Session};
use fc_remote::config::{AccountKind, AgentConfig, Share};
use fc_remote::protocol::{HashAlgo, KdfParams, RejectReason};
use fc_remote::server::{Server, ServerHandle};
use fc_remote::tls::Identity;

const MIB: usize = 1024 * 1024;

/// A running agent with one share (`backup`, writable) and one read-only share (`ro`).
struct Harness {
    /// Keeps the share directory alive for the test.
    root: tempfile::TempDir,
    handle: ServerHandle,
    port: u16,
    fingerprint: String,
}

async fn start() -> Harness {
    let root = tempfile::tempdir().unwrap();
    let mut cfg = AgentConfig::new("nas");
    cfg.listen = "127.0.0.1:0".parse().unwrap();
    // Cheap KDF so tests run fast; production uses the default.
    cfg.kdf = KdfParams { m_cost: 256, t_cost: 1, p_cost: 1 };
    cfg.shares.push(Share { name: "backup".into(), path: root.path().into(), writable: true });
    cfg.shares.push(Share { name: "ro".into(), path: root.path().into(), writable: false });
    cfg.set_account("alice", AccountKind::Interactive, "password1", None).unwrap();

    let identity = Identity::generate("127.0.0.1").unwrap();
    let server = Server::bind(cfg, &identity).unwrap();
    let handle = server.handle();
    let port = handle.local_addr().unwrap().port();
    let fingerprint = handle.fingerprint().to_string();
    tokio::spawn(server.run());
    Harness { root, handle, port, fingerprint }
}

impl Harness {
    /// Connector for `user` at this agent, pinned to the agent's real fingerprint.
    fn connector(&self, user: &str, password: &str) -> Connector {
        let spec = RemoteSpec::parse(&format!("{user}@127.0.0.1:{}:/backup", self.port)).unwrap();
        Connector::new(spec).password(password).pinned(Some(self.fingerprint.clone()))
    }

    async fn login(&self) -> Session {
        Session::open(&self.connector("alice", "password1"), None).await.expect("login")
    }
}

/// Deterministic pseudo-random bytes, so a failure reproduces.
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

fn write_source(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, bytes).unwrap();
    p
}

async fn push(s: &Session, local: &Path, remote: &str) -> Result<PushOutcome, ClientError> {
    let control = JobControl::new();
    let req = PushRequest { local, remote, algo: HashAlgo::Sha256, overwrite: true, source_id: None };
    s.push_file(&req, &PushControl { control: &control, limiter: None }, &()).await
}

// ---------------------------------------------------------------------------
// Authentication and pinning
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn login_succeeds_and_reports_the_server() {
    let h = start().await;
    let s = h.login().await;
    assert_eq!(s.fingerprint(), h.fingerprint);
    assert_eq!(s.server_name(), "nas");
    assert!(s.shares().iter().any(|x| x.name == "backup" && x.writable));
    assert!(!s.newly_trusted(), "pin was supplied, nothing new to trust");
    s.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn wrong_password_is_refused() {
    let h = start().await;
    let err = Session::open(&h.connector("alice", "wrong-password"), None).await.unwrap_err();
    assert!(matches!(err, ClientError::AuthFailed { .. }), "{err:?}");
    assert!(!err.is_retryable(), "a bad password must not be retried");
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_user_gets_the_same_refusal_as_a_wrong_password() {
    let h = start().await;
    let wrong_pw = Session::open(&h.connector("alice", "wrong-password"), None).await.unwrap_err();
    let no_user = Session::open(&h.connector("mallory", "password1"), None).await.unwrap_err();
    match (wrong_pw, no_user) {
        (ClientError::AuthFailed { reason: a, .. }, ClientError::AuthFailed { reason: b, .. }) => {
            assert_eq!(a, b, "usernames must not be enumerable");
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn changed_fingerprint_is_a_hard_failure() {
    let h = start().await;
    let spec = RemoteSpec::parse(&format!("alice@127.0.0.1:{}:/backup", h.port)).unwrap();
    let c = Connector::new(spec).password("password1").pinned(Some("SHA256:not-the-agent".into()));
    match Session::open(&c, None).await {
        Err(ClientError::HostKeyChanged { presented, .. }) => assert_eq!(presented, h.fingerprint),
        other => panic!("expected HostKeyChanged, got {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_host_is_refused_without_a_trust_decision() {
    let h = start().await;
    let spec = RemoteSpec::parse(&format!("alice@127.0.0.1:{}:/backup", h.port)).unwrap();
    let c = Connector::new(spec).password("password1");
    assert!(matches!(Session::open(&c, None).await, Err(ClientError::HostKeyNotTrusted { .. })));
}

#[tokio::test(flavor = "multi_thread")]
async fn trust_on_first_use_pins_the_fingerprint_for_reconnects() {
    let h = start().await;
    let spec = RemoteSpec::parse(&format!("alice@127.0.0.1:{}:/backup", h.port)).unwrap();
    let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = asked.clone();
    let c = Connector::new(spec).password("password1").on_unknown_host(Arc::new(move |_, _| {
        counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        true
    }));

    let s = Session::open(&c, None).await.unwrap();
    assert!(s.newly_trusted());
    assert_eq!(s.fingerprint(), h.fingerprint);
    assert_eq!(c.pinned_fingerprint().as_deref(), Some(h.fingerprint.as_str()));
    s.close().await;

    // Second open: the pin is already stored, so the user is not asked again.
    let s2 = Session::open(&c, None).await.unwrap();
    assert!(!s2.newly_trusted());
    assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------
// Transfers
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn push_is_byte_identical_and_digest_verified() {
    let h = start().await;
    let s = h.login().await;
    let src_dir = tempfile::tempdir().unwrap();
    let bytes = data(3 * MIB + 12_345, 1); // not a multiple of anything useful
    let src = write_source(src_dir.path(), "big.bin", &bytes);

    s.make_dir("backup/dest/nested").await.unwrap();
    let out = push(&s, &src, "backup/dest/nested/big.bin").await.unwrap();

    assert_eq!(out.size, bytes.len() as u64);
    assert_eq!(out.resumed_from, 0);
    let on_disk = h.root.path().join("dest/nested/big.bin");
    assert_eq!(std::fs::read(&on_disk).unwrap(), bytes);
    assert_eq!(out.digest, hash_file(&src, Algorithm::Sha256).unwrap());
    assert!(!h.root.path().join("dest/nested/big.bin.fcpart").exists(), "temp file renamed away");
}

#[tokio::test(flavor = "multi_thread")]
async fn empty_file_round_trips() {
    let h = start().await;
    let s = h.login().await;
    let src_dir = tempfile::tempdir().unwrap();
    let src = write_source(src_dir.path(), "empty", b"");
    push(&s, &src, "backup/empty.dat").await.unwrap();
    assert_eq!(std::fs::read(h.root.path().join("empty.dat")).unwrap(), b"");
}

#[tokio::test(flavor = "multi_thread")]
async fn path_traversal_is_rejected_by_the_server() {
    let h = start().await;
    let s = h.login().await;
    let src_dir = tempfile::tempdir().unwrap();
    let src = write_source(src_dir.path(), "x", b"payload");

    for remote in ["backup/../escape.txt", "nowhere/x.txt", "backup/a\\b.txt"] {
        match push(&s, &src, remote).await {
            Err(ClientError::Rejected { reason: RejectReason::BadPath, .. }) => {}
            other => panic!("{remote}: {other:?}"),
        }
    }
    assert!(!h.root.path().parent().unwrap().join("escape.txt").exists());
    assert!(s.is_connected(), "a rejected file does not close the session");
}

#[tokio::test(flavor = "multi_thread")]
async fn read_only_share_is_denied() {
    let h = start().await;
    let s = h.login().await;
    let src_dir = tempfile::tempdir().unwrap();
    let src = write_source(src_dir.path(), "x", b"payload");
    match push(&s, &src, "ro/x.txt").await {
        Err(ClientError::Rejected { reason: RejectReason::Denied, .. }) => {}
        other => panic!("{other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn pause_stalls_the_sender_until_resume() {
    let h = start().await;
    let s = h.login().await;
    let src_dir = tempfile::tempdir().unwrap();
    let src = write_source(src_dir.path(), "p.bin", &data(2 * MIB, 2));

    h.handle.pause_all();
    let pause_then_resume = async {
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert!(s.remote_paused(), "client should learn the server paused");
        h.handle.resume_all();
    };
    let started = Instant::now();
    let (result, ()) = tokio::join!(push(&s, &src, "backup/p.bin"), pause_then_resume);
    result.unwrap();
    assert!(started.elapsed() >= Duration::from_millis(400), "transfer ran while paused");
    assert!(!s.remote_paused());
    assert_eq!(std::fs::read(h.root.path().join("p.bin")).unwrap(), std::fs::read(&src).unwrap());
}

#[tokio::test(flavor = "multi_thread")]
async fn skip_before_start_ends_the_file_and_keeps_the_session() {
    let h = start().await;
    let s = h.login().await;
    let src_dir = tempfile::tempdir().unwrap();
    let src = write_source(src_dir.path(), "s.bin", &data(MIB, 3));

    let control = JobControl::new();
    control.request_skip();
    let req = PushRequest { local: &src, remote: "backup/s.bin", algo: HashAlgo::Sha256, overwrite: true, source_id: None };
    let r = s.push_file(&req, &PushControl { control: &control, limiter: None }, &()).await;
    assert!(matches!(r, Err(ClientError::Skipped)), "{r:?}");
    assert!(s.is_connected());
    // The session is still usable for the next file.
    push(&s, &src, "backup/s2.bin").await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_mid_file_stops_quickly_and_keeps_the_session() {
    let h = start().await;
    let s = h.login().await;
    let src_dir = tempfile::tempdir().unwrap();
    let src = write_source(src_dir.path(), "c.bin", &data(3 * MIB, 4));

    // 1 MiB/s, so a 3 MiB file takes seconds; cancel after 300 ms.
    let control = Arc::new(JobControl::new());
    let limiter = RateLimiter::new(Some(MIB as u64));
    let canceller = {
        let control = control.clone();
        async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            control.cancel();
        }
    };
    let req = PushRequest { local: &src, remote: "backup/c.bin", algo: HashAlgo::Sha256, overwrite: true, source_id: None };
    let ctl = PushControl { control: &control, limiter: Some(&limiter) };
    let started = Instant::now();
    let (r, ()) = tokio::join!(s.push_file(&req, &ctl, &()), canceller);
    assert!(matches!(r, Err(ClientError::Cancelled)), "{r:?}");
    assert!(started.elapsed() < Duration::from_secs(2), "cancel took {:?}", started.elapsed());
    assert!(s.is_connected());
    push(&s, &src, "backup/c2.bin").await.unwrap();
}

// ---------------------------------------------------------------------------
// Forced reconnect mid-file
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn connection_drop_mid_file_resumes_on_reconnect_with_token() {
    let h = start().await;
    let src_dir = tempfile::tempdir().unwrap();
    let bytes = data(6 * MIB, 5);
    let src = write_source(src_dir.path(), "big.bin", &bytes);

    // First session: the server drops the connection once 2.5 MiB has arrived.
    let c = h.connector("alice", "password1");
    let s1 = Session::open(&c, None).await.unwrap();
    s1.make_dir("backup/dest").await.unwrap();
    h.handle.hooks().drop_after_bytes.store((2 * MIB + MIB / 2) as u64, std::sync::atomic::Ordering::SeqCst);

    let err = push(&s1, &src, "backup/dest/big.bin").await.unwrap_err();
    assert!(err.is_retryable(), "a dropped connection must be retryable: {err:?}");
    let token = s1.token();
    drop(s1);

    // Reconnect with the token and NO password: proves the fast path works.
    let wrong = h.connector("alice", "not-the-password");
    let s2 = Session::open(&wrong, Some(token)).await.expect("token reconnect");
    let out = push(&s2, &src, "backup/dest/big.bin").await.unwrap();

    assert!(out.resumed_from > 0, "the second attempt must resume, not restart");
    assert!(out.resumed_from < out.size);
    assert_eq!(std::fs::read(h.root.path().join("dest/big.bin")).unwrap(), bytes, "byte-identical after resume");
    assert_eq!(out.digest, hash_file(&src, Algorithm::Sha256).unwrap());
    assert!(!h.root.path().join("dest/big.bin.fcpart").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_changed_source_is_not_resumed_from_a_stale_partial() {
    let h = start().await;
    let src_dir = tempfile::tempdir().unwrap();
    let first = data(4 * MIB, 6);
    let src = write_source(src_dir.path(), "v.bin", &first);

    let c = h.connector("alice", "password1");
    let s1 = Session::open(&c, None).await.unwrap();
    s1.make_dir("backup/d").await.unwrap();
    h.handle.hooks().drop_after_bytes.store(MIB as u64, std::sync::atomic::Ordering::SeqCst);
    assert!(push(&s1, &src, "backup/d/v.bin").await.is_err());
    let token = s1.token();
    drop(s1);

    // Same path, different content and mtime: the source_id changes, so the
    // server must not splice the old partial with the new data.
    std::thread::sleep(Duration::from_millis(1100));
    let second = data(4 * MIB, 7);
    std::fs::write(&src, &second).unwrap();

    let s2 = Session::open(&c, Some(token)).await.unwrap();
    let out = push(&s2, &src, "backup/d/v.bin").await.unwrap();
    assert_eq!(out.resumed_from, 0, "stale partial must not be resumed");
    assert_eq!(std::fs::read(h.root.path().join("d/v.bin")).unwrap(), second);
}

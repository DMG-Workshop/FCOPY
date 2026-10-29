//! The `fcopy` remote destination syntax, end to end on loopback.

use std::process::{Command, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_fcopy");

#[test]
fn a_remote_destination_without_a_password_is_refused_before_anything_starts() {
    let src = tempfile::tempdir().unwrap();
    let file = src.path().join("a.txt");
    std::fs::write(&file, b"x").unwrap();
    let out = Command::new(BIN)
        .arg("copy")
        .arg(&file)
        .arg("alice@127.0.0.1:47800:/backup")
        .env_remove("FCOPY_PASSWORD")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("no password"));
}

#[test]
fn a_local_destination_is_not_mistaken_for_a_remote_one() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let file = src.path().join("a.txt");
    std::fs::write(&file, b"hello").unwrap();
    let out = Command::new(BIN).arg("copy").arg(&file).arg(dst.path()).env_remove("FCOPY_PASSWORD").output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(std::fs::read(dst.path().join("a.txt")).unwrap(), b"hello");
}

use fc_remote::config::{AccountKind, AgentConfig, Share};
use fc_remote::protocol::KdfParams;
use fc_remote::server::{Server, ServerHandle};
use fc_remote::tls::Identity;

/// A real agent on loopback, running on its own runtime for the test's lifetime.
struct Agent {
    _rt: tokio::runtime::Runtime,
    handle: ServerHandle,
    port: u16,
    fingerprint: String,
    share: tempfile::TempDir,
}

fn agent() -> Agent {
    let rt = tokio::runtime::Runtime::new().unwrap();
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

fn copy_to(agent: &Agent, config_dir: &std::path::Path, file: &std::path::Path, extra: &[&str]) -> std::process::Output {
    let target = format!("alice@127.0.0.1:{}:/backup/cli", agent.port);
    let pw = config_dir.join("pw");
    std::fs::write(&pw, "agent-pass-1\n").unwrap();
    Command::new(BIN)
        .arg("copy")
        .arg(file)
        .arg(&target)
        .arg("--password-file")
        .arg(&pw)
        .args(extra)
        .env("FCOPY_CONFIG_DIR", config_dir)
        .env_remove("FCOPY_PASSWORD")
        .stdin(Stdio::null())
        .output()
        .unwrap()
}

#[test]
fn copy_to_a_pc_trusts_it_once_and_refuses_a_wrong_fingerprint() {
    let a = agent();
    let config = tempfile::tempdir().unwrap();
    let src = tempfile::tempdir().unwrap();
    let file = src.path().join("note.txt");
    std::fs::write(&file, b"from the cli").unwrap();

    // A wrong fingerprint is refused, and nothing is trusted.
    let wrong = copy_to(&a, config.path(), &file, &["--accept-fingerprint", "SHA256:wrong"]);
    assert!(!wrong.status.success(), "{}", String::from_utf8_lossy(&wrong.stderr));
    assert!(!config.path().join("known_hosts").exists(), "a refused PC is not saved");

    // The right fingerprint: the copy works and the trust is saved.
    let first = copy_to(&a, config.path(), &file, &["--accept-fingerprint", &a.fingerprint]);
    assert!(first.status.success(), "{}", String::from_utf8_lossy(&first.stderr));
    assert_eq!(std::fs::read(a.share.path().join("cli/note.txt")).unwrap(), b"from the cli");
    assert!(std::fs::read_to_string(config.path().join("known_hosts")).unwrap().contains(&a.fingerprint));

    // The next copy needs no fingerprint flag: the saved trust is used.
    std::fs::write(&file, b"second copy").unwrap();
    let second = copy_to(&a, config.path(), &file, &["--overwrite"]);
    assert!(second.status.success(), "{}", String::from_utf8_lossy(&second.stderr));
    assert_eq!(std::fs::read(a.share.path().join("cli/note.txt")).unwrap(), b"second copy");
    a.handle.shutdown();
}

//! The `fcopy-agent` command line, run as a user would run it.

use std::io::{BufRead, BufReader};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_fcopy-agent");

fn agent(dir: &std::path::Path, args: &[&str], password: Option<&str>) -> Output {
    let mut cmd = Command::new(BIN);
    cmd.arg("--dir").arg(dir).args(args);
    cmd.env_remove("FCOPY_PASSWORD");
    if let Some(p) = password {
        cmd.env("FCOPY_PASSWORD", p);
    }
    cmd.output().expect("run fcopy-agent")
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

#[test]
fn init_creates_identity_and_config_then_refuses_to_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let init = agent(dir.path(), &["init", "--name", "nas", "--port", "0"], None);
    assert!(init.status.success(), "{}", text(&init));
    assert!(dir.path().join("agent.toml").exists());
    assert!(dir.path().join("identity.der").exists());

    let shown = agent(dir.path(), &["fingerprint"], None);
    let fp = String::from_utf8_lossy(&shown.stdout).trim().to_string();
    assert!(fp.starts_with("SHA256:"), "{fp}");
    assert!(text(&init).contains(&fp), "init shows the same fingerprint");

    let again = agent(dir.path(), &["init", "--name", "nas"], None);
    assert!(!again.status.success());
    assert!(text(&again).contains("already exists"));
}

#[test]
fn shares_accounts_and_allow_rules_persist() {
    let dir = tempfile::tempdir().unwrap();
    let share_dir = tempfile::tempdir().unwrap();
    assert!(agent(dir.path(), &["init", "--name", "nas", "--port", "0"], None).status.success());

    let add = agent(dir.path(), &["share", "add", "backup", share_dir.path().to_str().unwrap()], None);
    assert!(add.status.success(), "{}", text(&add));
    assert!(text(&agent(dir.path(), &["share", "list"], None)).contains("backup\tread-write"));
    assert!(!agent(dir.path(), &["share", "del", "nope"], None).status.success());

    let short = agent(dir.path(), &["user", "add", "alice"], Some("short"));
    assert!(!short.status.success(), "a short password is refused");
    let ok = agent(dir.path(), &["user", "add", "alice", "--shares", "backup"], Some("password1"));
    assert!(ok.status.success(), "{}", text(&ok));
    assert!(text(&agent(dir.path(), &["user", "list"], None)).contains("alice\tbackup"));
    let stored = std::fs::read_to_string(dir.path().join("agent.toml")).unwrap();
    assert!(!stored.contains("password1"), "the password is never written down");

    let bad_cidr = agent(dir.path(), &["allow", "add", "not-an-address"], None);
    assert!(!bad_cidr.status.success());
    assert!(agent(dir.path(), &["allow", "add", "192.168.1.0/24"], None).status.success());
    assert!(text(&agent(dir.path(), &["allow", "list"], None)).contains("192.168.1.0/24"));
    assert!(agent(dir.path(), &["allow", "del", "192.168.1.0/24"], None).status.success());
    assert!(!text(&agent(dir.path(), &["allow", "list"], None)).contains("192.168.1.0/24"));
}

#[test]
fn an_unattended_key_is_printed_once_and_not_stored() {
    let dir = tempfile::tempdir().unwrap();
    assert!(agent(dir.path(), &["init", "--name", "nas", "--port", "0"], None).status.success());
    let out = agent(dir.path(), &["unattended", "enable", "scripts"], None);
    assert!(out.status.success(), "{}", text(&out));
    let key = String::from_utf8_lossy(&out.stdout).lines().last().unwrap().trim().to_string();
    assert!(key.len() >= 30, "a long key: {key}");
    let stored = std::fs::read_to_string(dir.path().join("agent.toml")).unwrap();
    assert!(!stored.contains(&key), "the key itself is never stored");
}

#[test]
fn serve_prints_its_address_and_runs() {
    let dir = tempfile::tempdir().unwrap();
    assert!(agent(dir.path(), &["init", "--name", "nas", "--port", "0"], None).status.success());
    let mut child = Command::new(BIN)
        .arg("--dir")
        .arg(dir.path())
        .arg("serve")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let listening = rx.recv_timeout(Duration::from_secs(10)).expect("serve prints its address");
    assert!(listening.starts_with("listening on "), "{listening}");
    child.kill().unwrap();
    let _ = child.wait();
}

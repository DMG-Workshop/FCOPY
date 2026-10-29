//! Runs the real `fcopy` binary.

use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Instant;

fn fcopy() -> Command {
    Command::new(env!("CARGO_BIN_EXE_fcopy"))
}

fn data(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 2654435761usize) >> 13) as u8).collect()
}

#[test]
fn copies_a_tree_with_readback_and_writes_a_checkable_manifest() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("photos");
    fs::create_dir_all(src.join("2026")).unwrap();
    fs::write(src.join("a.jpg"), data(300_000)).unwrap();
    fs::write(src.join("2026/b.jpg"), data(1_500_000)).unwrap();
    let dest = dir.path().join("backup");
    let manifest = dest.join("photos.sha256");

    let out = fcopy()
        .args(["copy", "--verify", "readback", "--algo", "sha256", "--manifest"])
        .arg(&manifest)
        .arg(&src)
        .arg(&dest)
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{err}");
    assert!(err.contains("copied 2 file(s)") && err.contains("verified"), "{err}");
    assert_eq!(fs::read(dest.join("photos/2026/b.jpg")).unwrap(), data(1_500_000));

    // The manifest uses sha256sum format and verifies with `fcopy check`.
    let text = fs::read_to_string(&manifest).unwrap();
    assert_eq!(text.lines().count(), 2);
    let check = fcopy().args(["check", "--algo", "sha256"]).arg(&manifest).output().unwrap();
    assert!(check.status.success(), "{}", String::from_utf8_lossy(&check.stderr));

    // Corrupt a copy: check must fail.
    let mut f = fs::OpenOptions::new().write(true).open(dest.join("photos/a.jpg")).unwrap();
    f.write_all(b"X").unwrap();
    drop(f);
    let check = fcopy().args(["check", "--algo", "sha256"]).arg(&manifest).output().unwrap();
    assert_eq!(check.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&check.stdout).contains("a.jpg: FAILED"));
}

#[test]
fn single_file_to_new_name_with_speed_limit() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("big.iso");
    fs::write(&src, data(3_000_000)).unwrap();
    let dst = dir.path().join("copy-of-big.iso");
    // Warm-up launch: on Windows the first start of a freshly linked exe can
    // stall for seconds while antivirus scans it, which is not copy time.
    fcopy().arg("--version").output().unwrap();
    let t = Instant::now();
    let out = fcopy().args(["copy", "--limit", "10MB"]).arg(&src).arg(&dst).output().unwrap();
    let secs = t.elapsed().as_secs_f64();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!((0.25..1.0).contains(&secs), "3 MB at 10 MB/s took {secs}s");
    assert_eq!(fs::read(&dst).unwrap(), data(3_000_000));
}

#[test]
fn existing_destination_fails_without_overwrite() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("f");
    let dst = dir.path().join("g");
    fs::write(&src, b"new").unwrap();
    fs::write(&dst, b"old").unwrap();
    let out = fcopy().arg("copy").arg(&src).arg(&dst).output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("FAILED"));
    assert_eq!(fs::read(&dst).unwrap(), b"old");
    let out = fcopy().args(["copy", "--overwrite"]).arg(&src).arg(&dst).output().unwrap();
    assert!(out.status.success());
    assert_eq!(fs::read(&dst).unwrap(), b"new");
}

#[test]
fn ask_mode_reads_answers_from_stdin() {
    // Parent of the destination is a file, so directory creation fails and asks.
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("f");
    fs::write(&src, b"x").unwrap();
    let blocker = dir.path().join("blocker");
    fs::write(&blocker, b"file").unwrap();

    let mut child = fcopy()
        .args(["copy", "--on-error", "ask"])
        .arg(&src)
        .arg(blocker.join("sub/f"))
        .stdin(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"r\ns\n").unwrap(); // Retry, then Skip
    let out = child.wait_with_output().unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(err.matches("[r]etry").count(), 2, "{err}");
    assert!(err.contains("SKIPPED"), "{err}");
    assert_eq!(out.status.code(), Some(1));
}

#[test]
fn bad_usage_and_missing_source() {
    let out = fcopy().args(["copy", "only-one-path"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2)); // clap usage error
    let out = fcopy().args(["copy", "--limit", "fast", "a", "b"]).output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let dir = tempfile::tempdir().unwrap();
    let out = fcopy().arg("copy").arg(dir.path().join("nope")).arg(dir.path()).output().unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stderr));
}

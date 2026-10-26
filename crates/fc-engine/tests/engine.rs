//! End-to-end tests against the real filesystem.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use fc_engine::*;

fn data(len: usize, seed: u64) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            (x >> 24) as u8
        })
        .collect()
}

fn quick_auto() -> Arc<AutoHandler> {
    Arc::new(AutoHandler::new(RetryPolicy {
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
        ..RetryPolicy::default()
    }))
}

fn copier(verify: VerifyMode) -> Copier {
    Copier::new(CopyOptions { verify, algorithm: Algorithm::Sha256, ..Default::default() }, quick_auto())
}

fn copy_ok(c: &Copier, src: &Path, dst: &Path) -> CopyReport {
    match c.copy_file(src, dst, &Progress::new(), &JobControl::new()).unwrap() {
        CopyOutcome::Copied(r) => r,
        CopyOutcome::Skipped { error, .. } => panic!("unexpected skip: {error}"),
    }
}

fn leftover_temp_files(dir: &Path) -> Vec<PathBuf> {
    fs::read_dir(dir)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.to_string_lossy().ends_with(TEMP_SUFFIX))
        .collect()
}

#[test]
fn large_file_is_identical_and_verified() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("big.bin");
    let dst = dir.path().join("out/big.copy"); // parent created on demand
    fs::write(&src, data(7 * 1024 * 1024 + 12_345, 42)).unwrap();

    let r = copy_ok(&copier(VerifyMode::ReadBack), &src, &dst);

    assert_eq!(fs::read(&src).unwrap(), fs::read(&dst).unwrap());
    assert!(r.verified);
    assert_eq!(r.bytes, 7 * 1024 * 1024 + 12_345);
    assert_eq!(r.source_digest.unwrap(), hash_file(&src, Algorithm::Sha256).unwrap());
    assert!(leftover_temp_files(dst.parent().unwrap()).is_empty());
}

#[test]
fn zero_length_and_tiny_files() {
    let dir = tempfile::tempdir().unwrap();
    let c = copier(VerifyMode::ReadBack);
    for (name, len) in [("zero", 0usize), ("one", 1), ("small", 4097)] {
        let src = dir.path().join(name);
        let dst = dir.path().join(format!("{name}.out"));
        fs::write(&src, data(len, 7)).unwrap();
        let r = copy_ok(&c, &src, &dst);
        assert_eq!(fs::read(&dst).unwrap(), fs::read(&src).unwrap(), "{name}");
        assert!(r.verified, "{name}");
    }
}

#[test]
fn every_verify_mode_and_algorithm_works() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("s");
    fs::write(&src, data(3_000_000, 5)).unwrap();
    for (i, mode) in [VerifyMode::Off, VerifyMode::SourceHash, VerifyMode::ReadBack].into_iter().enumerate() {
        for algo in Algorithm::ALL {
            let c = Copier::new(CopyOptions { verify: mode, algorithm: algo, ..Default::default() }, quick_auto());
            let dst = dir.path().join(format!("d{i}-{algo}"));
            let r = copy_ok(&c, &src, &dst);
            assert_eq!(r.source_digest.is_some(), mode != VerifyMode::Off);
            assert_eq!(r.verified, mode == VerifyMode::ReadBack);
            if let Some(d) = &r.source_digest {
                assert_eq!(*d, hash_file(&src, algo).unwrap(), "{algo} {mode:?}");
            }
            assert_eq!(fs::read(&dst).unwrap(), fs::read(&src).unwrap());
        }
    }
}

#[test]
fn existing_destination_needs_overwrite_and_old_data_survives_a_refusal() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    let dst = dir.path().join("dst");
    fs::write(&src, b"new contents").unwrap();
    fs::write(&dst, b"precious old contents").unwrap();

    let c = copier(VerifyMode::Off);
    let err = c.copy_file(&src, &dst, &Progress::new(), &JobControl::new()).unwrap_err();
    assert!(matches!(err, CopyError::DestinationExists(_)), "{err}");
    assert_eq!(fs::read(&dst).unwrap(), b"precious old contents");

    let c = Copier::new(CopyOptions { overwrite: true, ..Default::default() }, quick_auto());
    copy_ok(&c, &src, &dst);
    assert_eq!(fs::read(&dst).unwrap(), b"new contents");
    assert!(leftover_temp_files(dir.path()).is_empty());
}

#[test]
fn copying_a_file_onto_itself_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let f = dir.path().join("f");
    fs::write(&f, b"data").unwrap();
    let c = Copier::new(CopyOptions { overwrite: true, ..Default::default() }, quick_auto());
    let err = c.copy_file(&f, &f, &Progress::new(), &JobControl::new()).unwrap_err();
    assert!(matches!(err, CopyError::DestinationExists(_)));
    assert_eq!(fs::read(&f).unwrap(), b"data");
}

#[test]
fn directory_as_source_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let err = copier(VerifyMode::Off)
        .copy_file(dir.path(), &dir.path().join("x"), &Progress::new(), &JobControl::new())
        .unwrap_err();
    assert!(matches!(err, CopyError::NotRegularFile(_)));
}

// ----- error handling: Retry / Skip / Cancel -----

#[test]
fn missing_source_is_skipped_by_auto_cancelled_by_cancel_handler() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope");
    let dst = dir.path().join("out");

    let out = copier(VerifyMode::Off).copy_file(&missing, &dst, &Progress::new(), &JobControl::new()).unwrap();
    match out {
        CopyOutcome::Skipped { error, .. } => assert_eq!(error.class(), ErrorClass::NotFound),
        other => panic!("{other:?}"),
    }
    assert!(!dst.exists());

    let c = Copier::new(CopyOptions::default(), Arc::new(CancelOnError));
    let err = c.copy_file(&missing, &dst, &Progress::new(), &JobControl::new()).unwrap_err();
    assert!(matches!(err, CopyError::Cancelled));
}

#[test]
fn user_handler_sees_context_retries_then_skips() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope");
    let asked = Arc::new(AtomicUsize::new(0));
    let a2 = asked.clone();
    let handler = FnHandler(move |ctx: &ErrorContext<'_>| {
        assert_eq!(ctx.operation, Operation::OpenSource);
        assert_eq!(ctx.attempt as usize, a2.fetch_add(1, Ordering::SeqCst) + 1);
        assert!(ctx.path.ends_with("nope"));
        if ctx.attempt < 3 { ErrorAction::Retry } else { ErrorAction::Skip }
    });
    let c = Copier::new(CopyOptions::default(), Arc::new(handler));
    let out = c.copy_file(&missing, &dir.path().join("o"), &Progress::new(), &JobControl::new()).unwrap();
    assert!(matches!(out, CopyOutcome::Skipped { .. }));
    assert_eq!(asked.load(Ordering::SeqCst), 3);
}

#[test]
fn space_precheck_failure_is_routed_to_the_handler() {
    // A failed free-space pre-check is a DiskFull question for the handler
    // (a UI would let the user free space and answer Retry).
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("s");
    fs::write(&src, b"hello").unwrap();
    let dst = dir.path().join("d");

    let asked = Arc::new(AtomicUsize::new(0));
    let a2 = asked.clone();
    let handler = FnHandler(move |ctx: &ErrorContext<'_>| {
        assert_eq!(ctx.class, ErrorClass::DiskFull);
        assert_eq!(ctx.operation, Operation::CheckSpace);
        a2.fetch_add(1, Ordering::SeqCst);
        ErrorAction::Skip
    });
    // reserve of u64::MAX can never be satisfied
    let c = Copier::new(CopyOptions { space_reserve: u64::MAX, ..Default::default() }, Arc::new(handler));
    match c.copy_file(&src, &dst, &Progress::new(), &JobControl::new()).unwrap() {
        CopyOutcome::Skipped { error: CopyError::InsufficientSpace { .. }, .. } => {}
        other => panic!("{other:?}"),
    }
    assert_eq!(asked.load(Ordering::SeqCst), 1);
    assert!(!dst.exists());

    // And the default unattended policy treats a full disk as fatal for the job.
    let c = Copier::new(CopyOptions { space_reserve: u64::MAX, ..Default::default() }, quick_auto());
    assert!(matches!(
        c.copy_file(&src, &dst, &Progress::new(), &JobControl::new()),
        Err(CopyError::Cancelled)
    ));
}

#[test]
fn batch_refuses_up_front_when_the_plan_cannot_fit() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    fs::create_dir(&src).unwrap();
    fs::write(src.join("a"), data(10_000, 1)).unwrap();
    let dest = dir.path().join("dest");

    let c = Copier::new(CopyOptions { space_reserve: u64::MAX, ..Default::default() }, quick_auto());
    let plan = plan_copy(&[src], &dest).unwrap();
    let err = c.run_plan(&plan, &dest, &Progress::new(), &JobControl::new()).unwrap_err();
    assert!(matches!(err, CopyError::InsufficientSpace { .. }));
    assert!(!dest.exists(), "nothing may be created when the pre-check fails");
}

// ----- control: cancel / pause -----

#[test]
fn cancel_mid_copy_leaves_no_partial_file() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("big");
    let dst = dir.path().join("big.out");
    fs::write(&src, data(48 * 1024 * 1024, 3)).unwrap();

    let control = Arc::new(JobControl::new());
    let c2 = control.clone();
    let progress = Progress::with_observer(Duration::from_millis(1), move |s| {
        if s.bytes_copied > 8 * 1024 * 1024 {
            c2.cancel();
        }
    });
    let err = copier(VerifyMode::ReadBack).copy_file(&src, &dst, &progress, &control).unwrap_err();
    assert!(matches!(err, CopyError::Cancelled));
    assert!(!dst.exists());
    assert!(leftover_temp_files(dir.path()).is_empty(), "{:?}", leftover_temp_files(dir.path()));
    assert!(progress.snapshot().bytes_copied < 48 * 1024 * 1024);
}

#[test]
fn pause_halts_progress_and_resume_completes() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("big");
    let dst = dir.path().join("big.out");
    fs::write(&src, data(24 * 1024 * 1024, 4)).unwrap();

    let control = Arc::new(JobControl::new());
    let progress = Arc::new(Progress::new());
    control.pause();
    let (c2, p2, s2, d2) = (control.clone(), progress.clone(), src.clone(), dst.clone());
    let worker = std::thread::spawn(move || copier(VerifyMode::Off).copy_file(&s2, &d2, &p2, &c2));

    std::thread::sleep(Duration::from_millis(200));
    assert!(!worker.is_finished(), "copy must wait while paused");
    assert_eq!(progress.snapshot().bytes_copied, 0);

    control.resume();
    let out = worker.join().unwrap().unwrap();
    assert!(matches!(out, CopyOutcome::Copied(_)));
    assert_eq!(fs::read(&src).unwrap(), fs::read(&dst).unwrap());
}

// ----- batch -----

#[test]
fn tree_copy_preserves_structure_empty_dirs_and_reports_symlinks() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("tree");
    fs::create_dir_all(root.join("a/b")).unwrap();
    fs::create_dir_all(root.join("empty")).unwrap();
    fs::write(root.join("top.txt"), b"top").unwrap();
    fs::write(root.join("a/mid.bin"), data(200_000, 1)).unwrap();
    fs::write(root.join("a/b/deep.bin"), data(2_500_000, 2)).unwrap();
    fs::write(root.join("a/b/zero"), b"").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(root.join("top.txt"), root.join("link")).unwrap();

    let dest = dir.path().join("dest");
    let plan = plan_copy(std::slice::from_ref(&root), &dest).unwrap();
    assert_eq!(plan.stats.files, 4);
    #[cfg(unix)]
    assert!(plan.warnings.iter().any(|w| w.contains("symlink")));

    let progress = Progress::new();
    let c = copier(VerifyMode::ReadBack);
    let report = c.run_plan(&plan, &dest, &progress, &JobControl::new()).unwrap();

    assert!(report.is_clean(), "{report:?}");
    assert_eq!(report.copied.len(), 4);
    assert_eq!(report.bytes_copied, 3 + 200_000 + 2_500_000);
    assert!(report.copied.iter().all(|r| r.verified));
    for rel in ["top.txt", "a/mid.bin", "a/b/deep.bin", "a/b/zero"] {
        assert_eq!(fs::read(root.join(rel)).unwrap(), fs::read(dest.join("tree").join(rel)).unwrap(), "{rel}");
    }
    assert!(dest.join("tree/empty").is_dir());

    let s = progress.snapshot();
    assert_eq!((s.files_done, s.files_total, s.files_skipped), (4, 4, 0));
    assert!((s.fraction - 1.0).abs() < 1e-9, "fraction={}", s.fraction);
    assert_eq!(s.bytes_copied, s.bytes_verified);
}

#[test]
fn batch_continues_past_conflicts_and_reports_them() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    fs::create_dir(&src).unwrap();
    for n in ["a", "b", "c"] {
        fs::write(src.join(n), n.as_bytes()).unwrap();
    }
    let dest = dir.path().join("dest");
    fs::create_dir_all(dest.join("src")).unwrap();
    fs::write(dest.join("src/b"), b"already here").unwrap();

    let plan = plan_copy(&[src], &dest).unwrap();
    let report = copier(VerifyMode::Off).run_plan(&plan, &dest, &Progress::new(), &JobControl::new()).unwrap();
    assert_eq!(report.copied.len(), 2);
    assert_eq!(report.failed.len(), 1);
    assert!(matches!(report.failed[0].1, CopyError::DestinationExists(_)));
    assert_eq!(fs::read(dest.join("src/b")).unwrap(), b"already here");
}

// ----- metadata -----

#[test]
fn mtime_is_preserved() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("s");
    let dst = dir.path().join("d");
    fs::write(&src, b"x").unwrap();
    let when = std::time::SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000_000);
    fs::File::options().write(true).open(&src).unwrap().set_modified(when).unwrap();
    copy_ok(&copier(VerifyMode::Off), &src, &dst);
    assert_eq!(fs::metadata(&dst).unwrap().modified().unwrap(), when);
}

#[cfg(unix)]
#[test]
fn permission_bits_are_preserved_but_setuid_is_not() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("script.sh");
    let dst = dir.path().join("script.copy");
    fs::write(&src, b"#!/bin/sh\n").unwrap();
    fs::set_permissions(&src, fs::Permissions::from_mode(0o4751)).unwrap();
    copy_ok(&copier(VerifyMode::Off), &src, &dst);
    assert_eq!(fs::metadata(&dst).unwrap().permissions().mode() & 0o7777, 0o751);
}

#[test]
fn progress_snapshot_is_sane_for_a_bare_copy_file_call() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("s");
    fs::write(&src, data(2_000_000, 8)).unwrap();
    let progress = Progress::new();
    let c = copier(VerifyMode::ReadBack);
    c.copy_file(&src, &dir.path().join("d"), &progress, &JobControl::new()).unwrap();
    let s = progress.snapshot();
    assert_eq!((s.files_done, s.files_total), (1, 1));
    assert_eq!(s.bytes_total, 2_000_000);
    assert!((s.fraction - 1.0).abs() < 1e-9);
}

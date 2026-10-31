//! Move mode: a source is deleted only after its copy is committed and proven,
//! and every way a copy can fall short keeps the source.

use std::fs;
use std::path::Path;
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

fn mover() -> Copier {
    let handler = Arc::new(AutoHandler::new(RetryPolicy {
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
        ..RetryPolicy::default()
    }));
    Copier::new(CopyOptions { move_source: true, ..Default::default() }, handler)
}

/// `root/photos/a.bin` and `root/photos/2026/b.bin`, returned with their bytes.
fn tree(root: &Path) -> (Vec<u8>, Vec<u8>) {
    let a = data(70_000, 1);
    let b = data(3_000, 2);
    fs::create_dir_all(root.join("photos/2026")).unwrap();
    fs::write(root.join("photos/a.bin"), &a).unwrap();
    fs::write(root.join("photos/2026/b.bin"), &b).unwrap();
    (a, b)
}

#[test]
fn a_move_forces_a_synced_and_verified_copy() {
    let c = Copier::new(
        CopyOptions { move_source: true, verify: VerifyMode::Off, fsync: false, ..Default::default() },
        Arc::new(AutoHandler::new(RetryPolicy::default())),
    );
    assert_eq!(c.options().verify, VerifyMode::ReadBack);
    assert!(c.options().fsync);
}

#[test]
fn a_move_deletes_sources_and_the_folders_it_emptied() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let (a, b) = tree(src.path());

    let plan = plan_copy(&[src.path().join("photos")], dst.path()).unwrap();
    let report = mover().run_plan(&plan, dst.path(), &Progress::new(), &JobControl::new()).unwrap();

    assert!(report.is_clean(), "{:?}", report.failed);
    assert_eq!(report.sources_removed, 2);
    assert_eq!(fs::read(dst.path().join("photos/a.bin")).unwrap(), a);
    assert_eq!(fs::read(dst.path().join("photos/2026/b.bin")).unwrap(), b);
    assert!(report.copied.iter().all(|r| r.verified), "every moved file was read back");
    assert!(!src.path().join("photos").exists(), "the emptied source folders are gone");
}

#[test]
fn a_file_that_was_not_copied_keeps_its_source_and_folder() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    tree(src.path());
    // The destination already has b.bin and overwrite is off, so b.bin is not copied.
    fs::create_dir_all(dst.path().join("photos/2026")).unwrap();
    fs::write(dst.path().join("photos/2026/b.bin"), b"someone else's file").unwrap();

    let plan = plan_copy(&[src.path().join("photos")], dst.path()).unwrap();
    let report = mover().run_plan(&plan, dst.path(), &Progress::new(), &JobControl::new()).unwrap();

    assert_eq!(report.sources_removed, 1);
    assert!(!src.path().join("photos/a.bin").exists(), "a.bin moved");
    assert!(src.path().join("photos/2026/b.bin").exists(), "b.bin was not copied, so it stays");
    assert_eq!(fs::read(dst.path().join("photos/2026/b.bin")).unwrap(), b"someone else's file");
}

#[test]
fn a_cancelled_move_deletes_nothing() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    tree(src.path());

    let plan = plan_copy(&[src.path().join("photos")], dst.path()).unwrap();
    let control = JobControl::new();
    control.cancel();
    let report = mover().run_plan(&plan, dst.path(), &Progress::new(), &control).unwrap();

    assert!(report.cancelled);
    assert_eq!(report.sources_removed, 0);
    assert!(src.path().join("photos/a.bin").exists());
    assert!(src.path().join("photos/2026/b.bin").exists());
}

#[test]
fn a_source_that_changed_size_is_kept() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("grew.bin");
    let dst = dir.path().join("copy.bin");
    fs::write(&src, data(5_000, 3)).unwrap();
    fs::write(&dst, data(4_000, 3)).unwrap();
    // The copy holds 4 000 bytes, but the source has since grown to 5 000.
    let report = CopyReport {
        src: src.clone(),
        dst,
        bytes: 4_000,
        source_digest: None,
        verified: true,
        retries: 0,
        elapsed: Duration::ZERO,
        warnings: vec![],
    };

    let why = remove_moved_source(&report).unwrap_err();
    assert!(why.contains("source was kept"), "{why}");
    assert!(src.exists());
    let why = remove_sent_source(&report).unwrap_err();
    assert!(why.contains("changed size"), "{why}");
    assert!(src.exists());
}

#[test]
fn a_copy_onto_itself_never_deletes_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("same.bin");
    fs::write(&file, data(1_000, 4)).unwrap();
    let report = CopyReport {
        src: file.clone(),
        dst: file.clone(),
        bytes: 1_000,
        source_digest: None,
        verified: true,
        retries: 0,
        elapsed: Duration::ZERO,
        warnings: vec![],
    };
    assert!(remove_moved_source(&report).is_err());
    assert!(file.exists());
}

#[test]
fn a_move_through_the_queue_empties_the_source() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let (a, _) = tree(src.path());

    let (queue, _events) = Queue::start(QueueConfig::default());
    let request = TransferRequest::into_dir(vec![src.path().join("photos")], dst.path())
        .with_options(CopyOptions { move_source: true, ..Default::default() });
    let id = queue.add(request);
    queue.wait_idle();

    assert_eq!(queue.job(id).unwrap().state, JobState::Completed);
    assert_eq!(fs::read(dst.path().join("photos/a.bin")).unwrap(), a);
    assert!(!src.path().join("photos").exists());
    queue.shutdown();
}

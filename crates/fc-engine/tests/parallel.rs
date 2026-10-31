//! Small files on several workers: every file arrives intact, the totals add up,
//! and moves, conflicts and cancels behave as they do one file at a time.

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use fc_engine::conflict::{ConflictPolicy, ConflictResolver};
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

fn copier(options: CopyOptions) -> Copier {
    let handler = Arc::new(AutoHandler::new(RetryPolicy {
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
        ..RetryPolicy::default()
    }));
    Copier::new(options, handler)
}

/// 300 small files across three folders, plus two files above the small-file size.
fn tree(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files = Vec::new();
    for i in 0..300 {
        let rel = format!("set/d{}/f{i:03}.bin", i % 3);
        files.push((rel, data(i * 37 % 5_000 + 1, i as u64)));
    }
    files.push(("set/big1.bin".into(), data(SMALL_FILE as usize + 4_096, 901)));
    files.push(("set/d0/big2.bin".into(), data(SMALL_FILE as usize * 2 + 7, 902)));
    for (rel, bytes) in &files {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }
    files
}

#[test]
fn parallel_small_files_all_arrive_byte_for_byte() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let files = tree(src.path());
    let c = copier(CopyOptions { small_file_workers: 8, verify: VerifyMode::ReadBack, ..Default::default() });

    let plan = plan_copy(&[src.path().join("set")], dst.path()).unwrap();
    let progress = Progress::new();
    let report = c.run_plan(&plan, dst.path(), &progress, &JobControl::new()).unwrap();

    assert!(report.is_clean(), "{:?}", report.failed.first());
    assert_eq!(report.copied.len(), files.len());
    for (rel, bytes) in &files {
        assert_eq!(&fs::read(dst.path().join(rel)).unwrap(), bytes, "{rel}");
    }
    let snap = progress.snapshot();
    assert_eq!(snap.files_done, files.len() as u64, "every file is counted once");
    assert_eq!(snap.bytes_copied, files.iter().map(|(_, b)| b.len() as u64).sum::<u64>());
    assert!((snap.fraction - 1.0).abs() < 1e-9, "the job ends at 100%: {}", snap.fraction);
}

#[test]
fn parallel_move_empties_the_source() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let files = tree(src.path());
    let c = copier(CopyOptions { small_file_workers: 8, move_source: true, ..Default::default() });

    let plan = plan_copy(&[src.path().join("set")], dst.path()).unwrap();
    let report = c.run_plan(&plan, dst.path(), &Progress::new(), &JobControl::new()).unwrap();

    assert!(report.is_clean(), "{:?}", report.failed.first());
    assert_eq!(report.sources_removed, files.len() as u64);
    assert!(!src.path().join("set").exists());
    for (rel, bytes) in &files {
        assert_eq!(&fs::read(dst.path().join(rel)).unwrap(), bytes, "{rel}");
    }
}

#[test]
fn an_asking_conflict_policy_keeps_files_in_order() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    tree(src.path());
    let resolver = Arc::new(ConflictResolver::new(ConflictPolicy::Ask));
    let c = copier(CopyOptions { small_file_workers: 8, conflicts: Some(resolver), ..Default::default() });

    let plan = plan_copy(&[src.path().join("set")], dst.path()).unwrap();
    let mut order = Vec::new();
    c.run_plan_observed(&plan, dst.path(), &Progress::new(), &JobControl::new(), &mut |ev| {
        if let FileEvent::Copied(r) = ev {
            order.push(r.src.clone());
        }
    })
    .unwrap();
    let planned: Vec<_> = plan.items.iter().map(|i| i.src.clone()).collect();
    assert_eq!(order, planned, "a policy that may ask copies one file at a time, in plan order");
}

#[test]
fn a_cancel_stops_parallel_workers_and_reports_cancelled() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    tree(src.path());
    let c = copier(CopyOptions { small_file_workers: 4, ..Default::default() });
    let plan = plan_copy(&[src.path().join("set")], dst.path()).unwrap();
    let control = JobControl::new();

    let mut seen = 0;
    let report = c
        .run_plan_observed(&plan, dst.path(), &Progress::new(), &control, &mut |ev| {
            if matches!(ev, FileEvent::Copied(_)) {
                seen += 1;
                if seen == 20 {
                    control.cancel();
                }
            }
        })
        .unwrap();

    assert!(report.cancelled);
    assert!(report.copied.len() < plan.items.len(), "the workers stopped early");
    // Whatever was reported copied really is there and complete.
    for r in &report.copied {
        assert_eq!(fs::read(&r.dst).unwrap(), fs::read(&r.src).unwrap());
    }
}

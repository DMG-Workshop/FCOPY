use std::fs;
use std::sync::{Arc, mpsc::Receiver};
use std::time::{Duration, Instant};

use fc_engine::conflict::{ConflictAction as A, ConflictChoice, ConflictPolicy as P, ConflictResolver, commit_file, files_equal, local_destination};
use fc_engine::*;

fn pending(q: &Queue, rx: &Receiver<Event>, job: JobId) -> fc_engine::queue::PendingConflict {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(conflict) = q.job(job).unwrap().conflict { return conflict; }
        let event = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
        assert!(!matches!(event, Event::Finished { job: id, .. } if id == job), "finished without a prompt");
    }
}

fn finished(rx: &Receiver<Event>, job: JobId) -> JobState {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap() {
            Event::Finished { job: id, state, .. } if id == job => return state,
            Event::Conflict { .. } => {}
            Event::Decision(d) => panic!("unexpected I/O decision: {d:?}"),
            _ => {}
        }
    }
}

fn choice(action: A, apply_to_all: bool) -> ConflictChoice { ConflictChoice { action, apply_to_all } }

#[test]
fn conflict_policies_preserve_or_replace_exactly_as_requested() {
    for policy in [P::Replace, P::Skip, P::KeepBoth, P::Compare] {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("source");
        let dst = dir.path().join("report.txt");
        fs::write(&src, b"new").unwrap();
        fs::write(&dst, if policy == P::Compare { b"new" } else { b"old" }).unwrap();
        let modified = fs::metadata(&dst).unwrap().modified().unwrap();
        fs::write(dir.path().join("report (copy 1).txt"), b"occupied").unwrap();
        let (q, rx) = Queue::start(QueueConfig::default());
        let id = q.add(TransferRequest::file_to(&src, &dst).with_conflict_policy(policy));
        let state = finished(&rx, id);
        assert_eq!(state, if matches!(policy, P::Skip | P::Compare) { JobState::CompletedWithErrors } else { JobState::Completed });
        if matches!(policy, P::KeepBoth | P::Skip) { assert_eq!(fs::read(&dst).unwrap(), b"old"); }
        else { assert_eq!(fs::read(&dst).unwrap(), b"new"); }
        if policy == P::Compare { assert_eq!(fs::metadata(&dst).unwrap().modified().unwrap(), modified); }
        if policy == P::KeepBoth { assert_eq!(fs::read(dir.path().join("report (copy 2).txt")).unwrap(), b"new"); }
        assert_eq!(fs::read(dir.path().join("report (copy 1).txt")).unwrap(), b"occupied");
        q.shutdown();
    }
}

#[test]
fn conflict_compare_all_and_different_answers_are_separate_and_job_scoped() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    let dst = dir.path().join("dst");
    fs::create_dir(&src).unwrap();
    fs::create_dir(&dst).unwrap();
    for name in ["a", "b", "c"] {
        fs::write(src.join(name), b"new").unwrap();
        fs::write(dst.join(name), if name == "b" { b"new" } else { b"old" }).unwrap();
    }
    let (q, rx) = Queue::start(QueueConfig {
        options: CopyOptions { verify: VerifyMode::ReadBack, algorithm: Algorithm::Sha256, ..Default::default() },
        ..Default::default()
    });
    let id = q.add(TransferRequest::into_dir(vec![src.join("a"), src.join("b"), src.join("c")], &dst).with_conflict_policy(P::Ask));
    let first = pending(&q, &rx, id);
    assert!(!first.details.different);
    assert!(!q.resolve_conflict(id, first.id + 1, choice(A::Replace, false)));
    assert!(q.resolve_conflict(id, first.id, choice(A::Compare, true)));
    let deadline = Instant::now() + Duration::from_secs(10);
    let second = loop {
        let candidate = pending(&q, &rx, id);
        if candidate.id != first.id { break candidate; }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    };
    assert!(second.details.different);
    assert!(!q.resolve_conflict(id, first.id, choice(A::Replace, false)));
    assert!(!q.resolve_conflict(id, second.id, choice(A::Compare, false)));
    assert!(q.resolve_conflict(id, second.id, choice(A::KeepBoth, true)));
    assert_eq!(finished(&rx, id), JobState::CompletedWithErrors);
    for name in ["a", "c"] {
        assert_eq!(fs::read(dst.join(format!("{name} (copy 1)"))).unwrap(), b"new");
        assert_eq!(fs::read(dst.join(name)).unwrap(), b"old");
    }
    assert!(!dst.join("b (copy 1)").exists());
    let id = q.add(TransferRequest::file_to(src.join("a"), dst.join("a")).with_conflict_policy(P::Ask));
    let next = pending(&q, &rx, id);
    assert!(!next.details.different, "apply-all must not leak into another job");
    assert!(q.resolve_conflict(id, next.id, choice(A::Skip, false)));
    assert_eq!(finished(&rx, id), JobState::CompletedWithErrors);
    q.shutdown();
}

#[test]
fn conflict_wait_can_be_cancelled_or_skipped_without_a_reply() {
    for cancel in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        fs::write(&src, b"new").unwrap();
        fs::write(&dst, b"old").unwrap();
        let (q, rx) = Queue::start(QueueConfig::default());
        let id = q.add(TransferRequest::file_to(&src, &dst).with_conflict_policy(P::Ask));
        let conflict = pending(&q, &rx, id);
        assert!(if cancel { q.cancel(id) } else { q.skip_current_file() });
        assert_eq!(finished(&rx, id), if cancel { JobState::Cancelled } else { JobState::CompletedWithErrors });
        assert!(q.job(id).unwrap().conflict.is_none());
        assert!(!q.resolve_conflict(id, conflict.id, choice(A::Replace, true)));
        assert_eq!(fs::read(&dst).unwrap(), b"old");
        q.shutdown();
    }
}

#[test]
fn conflict_commit_does_not_clobber_a_late_destination() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("source");
    let dst = dir.path().join("destination");
    let temp = dir.path().join("temporary");
    fs::write(&src, b"source").unwrap();
    fs::write(&temp, b"incoming").unwrap();
    let resolver = ConflictResolver::new(P::Ask);
    let (_, overwrite) = local_destination(&src, &dst, true, Some(&resolver), &JobControl::new()).unwrap();
    assert!(!overwrite, "Ask overrides a legacy overwrite default");
    fs::write(&dst, b"late arrival").unwrap();
    assert!(commit_file(&temp, &dst, overwrite).is_err());
    assert_eq!(fs::read(&dst).unwrap(), b"late arrival");
    assert_eq!(fs::read(&temp).unwrap(), b"incoming");
    let other = dir.path().join("other");
    commit_file(&temp, &other, false).unwrap();
    assert_eq!(fs::read(other).unwrap(), b"incoming");
    assert!(!temp.exists());
}

#[test]
fn conflict_comparison_empty_files_same_paths_and_invalid_answers() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    let b = dir.path().join("b");
    fs::write(&a, b"").unwrap();
    fs::write(&b, b"").unwrap();
    let control = JobControl::new();
    assert!(files_equal(&a, &b, &control).unwrap());
    assert!(files_equal(&a, dir.path(), &control).is_err());
    assert!(local_destination(&a, &a, true, Some(&ConflictResolver::new(P::Replace)), &control).is_err());
    let (candidate, overwrite) = local_destination(&a, &a, false, Some(&ConflictResolver::new(P::KeepBoth)), &control).unwrap();
    assert_eq!(candidate, dir.path().join("a (copy 1)"));
    assert!(!overwrite);
    let bad = ConflictResolver::new(P::Compare).with_handler(Arc::new(|_, _| choice(A::Compare, true)));
    assert!(bad.choose(&a, &b, true, &control).is_err());
    assert!(ConflictResolver::new(P::Ask).choose(&a, &b, false, &control).is_err());
}

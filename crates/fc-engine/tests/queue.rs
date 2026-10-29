//! Queue manager: ordering, pause/resume, throttling, skip, cancel, decisions.

use std::fs;
use std::path::Path;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

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

fn quick_policy() -> ErrorPolicy {
    ErrorPolicy::Auto(RetryPolicy {
        base_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(2),
        ..RetryPolicy::default()
    })
}

fn start(policy: ErrorPolicy, limit: Option<u64>) -> (Queue, Receiver<Event>) {
    Queue::start(QueueConfig {
        policy,
        speed_limit: limit,
        telemetry_interval: Duration::from_millis(20),
        options: CopyOptions { chunk_size: 64 * 1024, ..Default::default() },
        ..Default::default()
    })
}

/// Drain events until `pred` matches (or panic after `timeout`).
fn wait_for(rx: &Receiver<Event>, timeout: Duration, mut pred: impl FnMut(&Event) -> bool) -> Vec<Event> {
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        let ev = rx.recv_timeout(left).expect("timed out waiting for event");
        let hit = pred(&ev);
        seen.push(ev);
        if hit {
            return seen;
        }
    }
}

fn finished(rx: &Receiver<Event>, job: JobId) -> (JobState, JobSummary, Vec<Event>) {
    let seen = wait_for(rx, Duration::from_secs(30), |e| matches!(e, Event::Finished { job: j, .. } if *j == job));
    match seen.last() {
        Some(Event::Finished { state, summary, .. }) => (state.clone(), summary.clone(), seen),
        _ => unreachable!(),
    }
}

fn same(a: &Path, b: &Path) -> bool {
    fs::read(a).unwrap() == fs::read(b).unwrap()
}

#[test]
fn jobs_run_in_order_and_report_lifecycle() {
    let dir = tempfile::tempdir().unwrap();
    let (a, b) = (dir.path().join("a.bin"), dir.path().join("b.bin"));
    fs::write(&a, data(300_000, 1)).unwrap();
    fs::write(&b, data(200_000, 2)).unwrap();
    let out = dir.path().join("out");

    let (q, rx) = start(quick_policy(), None);
    let j1 = q.add(TransferRequest::into_dir(vec![a.clone()], &out));
    let j2 = q.add(TransferRequest::file_to(&b, out.join("renamed.bin")));

    let (s1, sum1, ev1) = finished(&rx, j1);
    assert_eq!(s1, JobState::Completed);
    assert_eq!(sum1.bytes_copied, 300_000);
    assert!(matches!(ev1[0], Event::Queued { job, .. } if job == j1));
    assert!(ev1.iter().any(|e| matches!(e, Event::Started { job, files: 1, .. } if *job == j1)));
    assert!(ev1.iter().any(|e| matches!(e, Event::FileCopied { job, bytes: 300_000, .. } if *job == j1)));

    let (s2, _, _) = finished(&rx, j2);
    assert_eq!(s2, JobState::Completed);
    assert!(same(&a, &out.join("a.bin")));
    assert!(same(&b, &out.join("renamed.bin")));

    let jobs = q.jobs();
    assert_eq!(jobs.iter().map(|j| j.id).collect::<Vec<_>>(), vec![j1, j2]);
    assert!(jobs.iter().all(|j| j.state == JobState::Completed && j.elapsed.is_some()));
    q.shutdown();
}

#[test]
fn paused_queue_does_not_start_jobs_until_resumed() {
    let dir = tempfile::tempdir().unwrap();
    let a = dir.path().join("a");
    fs::write(&a, b"hello").unwrap();
    let (q, rx) = start(quick_policy(), None);
    q.pause();
    let j = q.add(TransferRequest::into_dir(vec![a], dir.path().join("out")));
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(q.job(j).unwrap().state, JobState::Pending);
    q.resume();
    assert_eq!(finished(&rx, j).0, JobState::Completed);
    q.shutdown();
}

#[test]
fn pause_mid_file_freezes_bytes_and_resume_completes_without_loss() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("big.bin");
    fs::write(&src, data(6_000_000, 3)).unwrap();
    let out = dir.path().join("out");

    // Throttle so the transfer is still running when we pause it.
    let (q, rx) = start(quick_policy(), Some(10_000_000));
    let j = q.add(TransferRequest::into_dir(vec![src.clone()], &out));
    wait_for(&rx, Duration::from_secs(10), |e| matches!(e, Event::Progress { telemetry, .. } if telemetry.bytes_copied > 1_000_000));

    q.pause();
    std::thread::sleep(Duration::from_millis(150)); // let in-flight chunk finish
    let frozen = q.job(j).unwrap().telemetry.unwrap().bytes_copied;
    std::thread::sleep(Duration::from_millis(400));
    let still = q.job(j).unwrap().telemetry.unwrap().bytes_copied;
    assert_eq!(frozen, still, "no bytes may move while paused");
    assert!(frozen < 6_000_000);
    assert_eq!(q.job(j).unwrap().state, JobState::Running);

    q.resume();
    let (state, summary, _) = finished(&rx, j);
    assert_eq!(state, JobState::Completed);
    assert_eq!(summary.bytes_copied, 6_000_000);
    assert!(same(&src, &out.join("big.bin")), "resumed copy must be byte-identical");
    q.shutdown();
}

#[test]
fn speed_limit_caps_throughput_and_can_change_live() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("s");
    fs::write(&src, data(4_000_000, 4)).unwrap();

    let (q, rx) = start(quick_policy(), Some(8_000_000)); // 4 MB @ 8 MB/s ≈ 0.5 s
    let t = Instant::now();
    let j = q.add(TransferRequest::into_dir(vec![src.clone()], dir.path().join("o1")));
    finished(&rx, j);
    let s = t.elapsed().as_secs_f64();
    assert!((0.4..0.9).contains(&s), "took {s}s at 8 MB/s");

    // Lift the limit: same copy should now be far faster.
    q.set_speed_limit(None);
    assert_eq!(q.speed_limit(), None);
    let t = Instant::now();
    let j = q.add(TransferRequest::into_dir(vec![src], dir.path().join("o2")));
    finished(&rx, j);
    assert!(t.elapsed().as_secs_f64() < 0.3);
    q.shutdown();
}

#[test]
fn telemetry_reports_speed_eta_percentage_elapsed() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("s");
    fs::write(&src, data(3_000_000, 5)).unwrap();
    let (q, rx) = start(quick_policy(), Some(6_000_000));
    let j = q.add(TransferRequest::into_dir(vec![src], dir.path().join("o")));
    let (_, _, events) = finished(&rx, j);

    let ticks: Vec<&Snapshot> = events
        .iter()
        .filter_map(|e| match e {
            Event::Progress { telemetry, .. } => Some(telemetry),
            _ => None,
        })
        .collect();
    assert!(ticks.len() >= 5, "only {} telemetry events", ticks.len());
    let mid = ticks.iter().find(|t| t.fraction > 0.3 && t.fraction < 0.8).expect("a mid-transfer sample");
    assert!((3e6..12e6).contains(&mid.rate_bps), "rate {} should be near the 6 MB/s cap", mid.rate_bps);
    let eta = mid.eta.expect("eta while running").as_secs_f64();
    assert!(eta > 0.0 && eta < 2.0, "eta {eta}");
    assert!(mid.elapsed > Duration::ZERO);
    assert!(mid.current_file.as_ref().unwrap().ends_with("s"));
    // fraction is monotonic
    assert!(ticks.windows(2).all(|w| w[1].fraction >= w[0].fraction));
    q.shutdown();
}

#[test]
fn skip_current_file_moves_on_to_the_next() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    fs::create_dir(&src).unwrap();
    fs::write(src.join("1-big"), data(8_000_000, 6)).unwrap();
    fs::write(src.join("2-small"), b"small").unwrap();
    let out = dir.path().join("out");

    let (q, rx) = start(quick_policy(), Some(5_000_000));
    let j = q.add(TransferRequest::into_dir(vec![src.clone()], &out));
    wait_for(&rx, Duration::from_secs(10), |e| matches!(e, Event::Progress { telemetry, .. } if telemetry.bytes_copied > 500_000));
    assert!(q.skip_current_file());

    let (state, summary, events) = finished(&rx, j);
    assert_eq!(state, JobState::CompletedWithErrors);
    assert_eq!((summary.files_copied, summary.files_skipped), (1, 1));
    assert!(events.iter().any(|e| matches!(e, Event::FileSkipped { reason, .. } if reason == "skipped by user")));
    assert!(!out.join("src/1-big").exists());
    assert!(!out.join(format!("src/1-big{TEMP_SUFFIX}")).exists(), "partial file removed");
    assert_eq!(fs::read(out.join("src/2-small")).unwrap(), b"small");
    q.shutdown();
}

#[test]
fn cancel_pending_and_running_jobs() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("s");
    fs::write(&src, data(5_000_000, 7)).unwrap();
    let (q, rx) = start(quick_policy(), Some(2_000_000));
    let running = q.add(TransferRequest::into_dir(vec![src.clone()], dir.path().join("o1")));
    let pending = q.add(TransferRequest::into_dir(vec![src], dir.path().join("o2")));

    assert!(q.cancel(pending));
    assert_eq!(finished(&rx, pending).0, JobState::Cancelled);

    wait_for(&rx, Duration::from_secs(10), |e| matches!(e, Event::Progress { telemetry, .. } if telemetry.bytes_copied > 0));
    assert!(q.cancel(running));
    assert_eq!(finished(&rx, running).0, JobState::Cancelled);
    assert!(!dir.path().join("o1/s").exists());
    assert!(!q.cancel(running), "already finished");
    q.wait_idle();
    q.shutdown();
}

#[test]
fn ask_policy_round_trips_a_decision() {
    // Deterministic trigger: the destination's parent path is an existing
    // *file*, so creating the directory fails (with the same error every time).
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("a");
    fs::write(&src, b"a").unwrap();
    let blocker = dir.path().join("out-is-a-file");
    fs::write(&blocker, b"not a dir").unwrap();

    let (q, rx) = start(ErrorPolicy::Ask, None);
    let j = q.add(TransferRequest::file_to(&src, blocker.join("nested/a")));

    let mut answered = 0;
    let deadline = Instant::now() + Duration::from_secs(10);
    let state = loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).expect("event") {
            Event::Decision(d) => {
                assert_eq!(d.job, j);
                assert_eq!(d.operation, Operation::CreateDir);
                answered += 1;
                d.respond(if answered == 1 { ErrorAction::Retry } else { ErrorAction::Skip });
            }
            Event::Finished { job, state, .. } if job == j => break state,
            _ => {}
        }
    };
    assert_eq!(answered, 2, "Retry then Skip");
    assert_eq!(state, JobState::CompletedWithErrors);
    q.shutdown();
}

#[test]
fn bad_source_fails_the_job_but_not_the_queue() {
    let dir = tempfile::tempdir().unwrap();
    let ok = dir.path().join("ok");
    fs::write(&ok, b"ok").unwrap();
    let (q, rx) = start(quick_policy(), None);
    let bad = q.add(TransferRequest::into_dir(vec![dir.path().join("missing")], dir.path().join("o")));
    let good = q.add(TransferRequest::into_dir(vec![ok], dir.path().join("o")));
    assert!(matches!(finished(&rx, bad).0, JobState::Failed(_)));
    assert_eq!(finished(&rx, good).0, JobState::Completed);
    q.shutdown();
}

#[test]
fn per_job_options_override_queue_defaults() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("s");
    fs::write(&src, data(500_000, 9)).unwrap();
    let (q, rx) = start(quick_policy(), None);
    let opts = CopyOptions { verify: VerifyMode::ReadBack, algorithm: Algorithm::Sha256, ..Default::default() };
    let j = q.add(TransferRequest::into_dir(vec![src.clone()], dir.path().join("o")).with_options(opts).with_label("verified"));
    let (_, _, events) = finished(&rx, j);
    let expected = hash_file(&src, Algorithm::Sha256).unwrap().to_hex();
    assert!(events.iter().any(|e| matches!(e, Event::FileCopied { verified: true, digest: Some(d), .. } if *d == expected)));
    assert_eq!(q.job(j).unwrap().label, "verified");
    q.shutdown();
}

#[test]
fn held_pending_job_is_passed_over_until_resumed() {
    let dir = tempfile::tempdir().unwrap();
    let a_src = dir.path().join("a.bin");
    let b_src = dir.path().join("b.bin");
    fs::write(&a_src, data(200_000, 7)).unwrap();
    fs::write(&b_src, data(200_000, 8)).unwrap();
    let out = dir.path().join("out");

    let (q, rx) = start(quick_policy(), None);
    q.pause(); // hold the queue so both jobs stay pending
    let a = q.add(TransferRequest::into_dir(vec![a_src.clone()], &out));
    let b = q.add(TransferRequest::into_dir(vec![b_src.clone()], &out));
    assert!(q.pause_job(a));
    assert!(q.job(a).unwrap().paused);
    wait_for(&rx, Duration::from_secs(5), |e| matches!(e, Event::JobPaused { job } if *job == a));

    q.resume(); // the held job must not block the one behind it
    let (state, _, _) = finished(&rx, b);
    assert_eq!(state, JobState::Completed);
    assert_eq!(q.job(a).unwrap().state, JobState::Pending, "held job must not start");

    assert!(q.resume_job(a));
    let (state, _, _) = finished(&rx, a);
    assert_eq!(state, JobState::Completed);
    assert!(same(&a_src, &out.join("a.bin")));
    q.shutdown();
}

#[test]
fn paused_running_job_stays_frozen_through_a_global_resume() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("big.bin");
    fs::write(&src, data(6_000_000, 9)).unwrap();
    let out = dir.path().join("out");

    let (q, rx) = start(quick_policy(), Some(10_000_000));
    let j = q.add(TransferRequest::into_dir(vec![src.clone()], &out));
    wait_for(&rx, Duration::from_secs(10), |e| matches!(e, Event::Progress { telemetry, .. } if telemetry.bytes_copied > 1_000_000));

    assert!(q.pause_job(j));
    std::thread::sleep(Duration::from_millis(150));
    let frozen = q.job(j).unwrap().telemetry.unwrap().bytes_copied;

    // A whole-queue pause and resume must not release a job held on its own.
    q.pause();
    q.resume();
    std::thread::sleep(Duration::from_millis(400));
    let still = q.job(j).unwrap().telemetry.unwrap().bytes_copied;
    assert_eq!(frozen, still, "per-job pause must survive a global resume");
    assert_eq!(q.job(j).unwrap().state, JobState::Running);

    assert!(q.resume_job(j));
    let (state, summary, _) = finished(&rx, j);
    assert_eq!(state, JobState::Completed);
    assert_eq!(summary.bytes_copied, 6_000_000);
    assert!(same(&src, &out.join("big.bin")), "resumed copy must be byte-identical");
    q.shutdown();
}

#[test]
fn pause_and_resume_job_report_unknown_and_finished_jobs() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("s.bin");
    fs::write(&src, data(10_000, 10)).unwrap();
    let (q, rx) = start(quick_policy(), None);
    let j = q.add(TransferRequest::into_dir(vec![src], dir.path().join("out")));
    finished(&rx, j);
    assert!(!q.pause_job(j), "finished job cannot be paused");
    assert!(!q.pause_job(9_999), "unknown job");
    assert!(!q.resume_job(9_999));
    q.shutdown();
}

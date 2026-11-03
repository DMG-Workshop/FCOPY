//! The worker binary, driven through the application-side client, over the real
//! stdin and stdout protocol.
//!
//! Every plan here is refused before anything is written: a wrong key, an expired plan,
//! and a disk that is not present. No test in this file can change a disk.
//!
//! The worker opens its audit log under /var/lib/fcopy, which needs root. So these tests
//! run only when `FCOPY_LOOP_TESTS` is set, like the other root-only suites:
//! `FCOPY_LOOP_TESTS=1 cargo test -p fc-storage --features storage-destructive --test worker_binary`

#![cfg(all(target_os = "linux", feature = "storage-destructive"))]

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use fc_storage::model::{DiskIdentity, Transport};
use fc_storage::orchestrate::{Outcome, Request, WipeScope};
use fc_storage::protocol::{Envelope, PlanBody, WorkerEvent, seal};
use fc_storage::safety::Policy;
use fc_storage::wipe::WipeMethod;
use fc_storage::worker_client::run_worker;

const KEY: [u8; 32] = [0x42; 32];

fn enabled() -> bool {
    if std::env::var_os("FCOPY_LOOP_TESTS").is_some() {
        return true;
    }
    eprintln!(
        "skipped: set FCOPY_LOOP_TESTS=1 (the worker writes its audit log under /var/lib/fcopy, so needs root)"
    );
    false
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// A plan for a disk that does not exist on any machine.
fn plan_for_an_absent_disk(expires: u64) -> Envelope {
    let identity = DiskIdentity {
        serial: Some("NOT-PRESENT-SERIAL".into()),
        model: "absent".into(),
        size_bytes: 1 << 30,
    };
    let request = Request {
        disk: fc_storage::model::DiskId::derive(Transport::Usb, &identity),
        confirmed: identity,
        typed_serial: "NOT-PRESENT-SERIAL".into(),
        policy: Policy::default(),
        protected_paths: Vec::new(),
        scope: WipeScope::WholeDevice,
        method: WipeMethod::Clear,
        table: None,
    };
    let body = PlanBody {
        expires_at_unix: expires,
        nonce: "00112233445566778899aabbccddeeff".into(),
        request,
    };
    seal(&KEY, &body).expect("seal")
}

/// The worker binary, started directly. Production starts it through `pkexec`.
fn worker() -> Command {
    Command::new(env!("CARGO_BIN_EXE_fcopy-storage-worker"))
}

fn last_event(events: &[WorkerEvent]) -> Option<&WorkerEvent> {
    events.last()
}

#[test]
fn a_plan_for_a_disk_that_is_not_present_is_reported_and_nothing_runs() {
    if !enabled() {
        return;
    }
    let env = plan_for_an_absent_disk(now() + 600);
    let run = run_worker(worker(), &KEY, &env).expect("the worker runs");

    assert_eq!(run.exit_code, Some(1));
    assert_eq!(
        last_event(&run.events),
        Some(&WorkerEvent::Finished(Outcome::Failed(
            "the disk is no longer present".into()
        )))
    );
}

#[test]
fn a_plan_signed_with_another_key_is_refused_before_it_is_read() {
    if !enabled() {
        return;
    }
    let env = plan_for_an_absent_disk(now() + 600);
    let run = run_worker(worker(), &[0x43; 32], &env).expect("the worker runs");

    assert_eq!(run.exit_code, Some(1));
    assert!(matches!(
        last_event(&run.events),
        Some(WorkerEvent::Finished(Outcome::Failed(reason))) if reason.contains("signature")
    ));
}

#[test]
fn an_expired_plan_is_refused() {
    if !enabled() {
        return;
    }
    let env = plan_for_an_absent_disk(now().saturating_sub(1));
    let run = run_worker(worker(), &KEY, &env).expect("the worker runs");

    assert_eq!(run.exit_code, Some(1));
    assert!(matches!(
        last_event(&run.events),
        Some(WorkerEvent::Finished(Outcome::Failed(reason))) if reason.contains("expired")
    ));
}

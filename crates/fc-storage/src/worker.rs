//! The privileged worker's core. The binary `fcopy-storage-worker` is a thin wrapper
//! around [`serve`]. The worker runs once per operation: it reads one signed plan,
//! runs the guarded pipeline, and streams events back.
//!
//! Input, one item per line:
//! 1. the per-operation key, as 64 hex digits
//! 2. the MAC over the body, as 64 hex digits
//! 3. the plan body, exactly the JSON that was signed
//!
//! Output: one [`WorkerEvent`] per line on the events stream.
//!
//! Each operation is recorded in the audit log before its first write. If that
//! record cannot be written, nothing is changed.

use std::io::{self, BufRead, Write};
use std::sync::atomic::AtomicBool;

use serde_json::json;

use crate::orchestrate::{self, Audit, Outcome, Request, StorageError};
use crate::platform::Platform;
use crate::protocol::{self, Envelope, KEY_LEN, WorkerEvent};
use crate::safety::Refusal;
use crate::wipe::Progress;

/// How the worker ended. The binary turns this into its exit status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Served {
    Completed,
    Failed,
}

/// Real disks only. Partitions, loop devices, and anything that is not a whole disk
/// are refused, so the worker cannot be aimed at a test device or a partition node.
pub fn production_device_allowed(os_path: &str) -> bool {
    let Some(name) = os_path.strip_prefix("/dev/") else {
        return false;
    };
    if name.is_empty() || name.contains('/') {
        return false;
    }
    let letters_after = |prefix: &str| {
        name.strip_prefix(prefix)
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_lowercase()))
    };
    let digits_after = |prefix: &str| {
        name.strip_prefix(prefix)
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
    };
    let nvme = name.strip_prefix("nvme").is_some_and(|rest| {
        rest.split_once('n').is_some_and(|(c, ns)| {
            !c.is_empty()
                && c.bytes().all(|b| b.is_ascii_digit())
                && !ns.is_empty()
                && ns.bytes().all(|b| b.is_ascii_digit())
        })
    });
    letters_after("sd")
        || letters_after("hd")
        || letters_after("vd")
        || letters_after("xvd")
        || digits_after("mmcblk")
        || nvme
}

fn hex32(s: &str) -> Option<[u8; KEY_LEN]> {
    if s.len() != 2 * KEY_LEN || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; KEY_LEN];
    for (i, pair) in s.as_bytes().chunks(2).enumerate() {
        out[i] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(out)
}

fn read_line(input: &mut dyn BufRead) -> io::Result<String> {
    let mut s = String::new();
    input.read_line(&mut s)?;
    Ok(s.trim_end_matches(['\r', '\n']).to_owned())
}

fn emit(events: &mut dyn Write, event: &WorkerEvent) -> io::Result<()> {
    let line = protocol::encode_event(event)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))?;
    writeln!(events, "{line}")?;
    events.flush()
}

/// Appends one JSON record to the audit log and syncs it.
fn record(log: &mut dyn Write, value: serde_json::Value) -> io::Result<()> {
    writeln!(log, "{value}")?;
    log.flush()
}

/// The audit trail for one operation. Its `begin` must be durable before the first write.
struct JsonAudit<'a> {
    events: &'a mut dyn Write,
    log: &'a mut dyn Write,
    now: u64,
}

impl Audit for JsonAudit<'_> {
    fn begin(&mut self, request: &Request) -> Result<(), String> {
        // The disk id identifies the hardware, so it is recorded. The typed serial is
        // never recorded, and neither is any credential.
        record(
            self.log,
            json!({
                "at": self.now,
                "event": "begin",
                "disk": request.disk.as_str(),
                "method": format!("{:?}", request.method),
                "scope": format!("{:?}", request.scope),
            }),
        )
        .map_err(|e| e.to_string())
    }

    fn progress(&mut self, progress: &Progress) {
        let _ = emit(self.events, &WorkerEvent::Progress(*progress));
    }

    fn end(&mut self, outcome: &Outcome) {
        // A failure to record the end is reported on stderr, because the wipe has
        // already happened and cannot be undone by this record.
        if let Err(e) = record(
            self.log,
            json!({ "at": self.now, "event": "end", "outcome": format!("{outcome:?}") }),
        ) {
            eprintln!("warning: the audit log could not record the end of the operation: {e}");
        }
        let _ = emit(self.events, &WorkerEvent::Finished(outcome.clone()));
    }
}

fn fail(events: &mut dyn Write, reason: impl Into<String>) -> io::Result<Served> {
    emit(
        events,
        &WorkerEvent::Finished(Outcome::Failed(reason.into())),
    )?;
    Ok(Served::Failed)
}

/// Runs one operation: reads and verifies the signed plan, checks the device, runs the
/// guarded pipeline, and reports each step on `events`.
///
/// `allowed` decides which device paths the worker may write to. The binary passes
/// [`production_device_allowed`]. `seed` comes from the caller, so tests are reproducible.
pub fn serve(
    platform: &dyn Platform,
    input: &mut dyn BufRead,
    events: &mut dyn Write,
    log: &mut dyn Write,
    now_unix: u64,
    allowed: &dyn Fn(&str) -> bool,
    seed: [u8; 32],
) -> io::Result<Served> {
    let Some(key) = hex32(&read_line(input)?) else {
        return fail(events, "malformed input: the key is not 32 bytes of hex");
    };
    let Some(mac) = hex32(&read_line(input)?) else {
        return fail(events, "malformed input: the MAC is not 32 bytes of hex");
    };
    let body = read_line(input)?;
    if body.is_empty() {
        return fail(events, "malformed input: the plan is empty");
    }
    let envelope = Envelope {
        body: body.into_bytes(),
        mac,
    };
    let plan = match protocol::open(&key, &envelope, now_unix) {
        Ok(plan) => plan,
        Err(e) => return fail(events, e.to_string()),
    };
    let request = plan.request;

    let disks = match platform.enumerate() {
        Ok(disks) => disks,
        Err(e) => return fail(events, e.to_string()),
    };
    let Some(disk) = disks.iter().find(|d| d.id == request.disk) else {
        return fail(events, "the disk is no longer present");
    };
    if !allowed(&disk.os_path) {
        let _ = record(
            log,
            json!({ "at": now_unix, "event": "refused", "disk": request.disk.as_str(), "reason": "device not allowed" }),
        );
        emit(
            events,
            &WorkerEvent::Finished(Outcome::Refused(Refusal::DeviceNotAllowed)),
        )?;
        return Ok(Served::Failed);
    }

    let cancel = AtomicBool::new(false);
    let mut audit = JsonAudit {
        events: &mut *events,
        log: &mut *log,
        now: now_unix,
    };
    match orchestrate::execute(platform, &request, seed, &cancel, &mut audit) {
        Ok(()) => Ok(Served::Completed),
        Err(StorageError::Audit(reason)) => {
            // Nothing was written, so the trail is the record of this refusal.
            fail(
                events,
                format!("the audit record could not be written: {reason}"),
            )
        }
        // The end of every other operation was already reported through the audit.
        Err(_) => Ok(Served::Failed),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::io::Cursor;

    use super::*;
    use crate::fake::FakePlatform;
    use crate::model::{Disk, DiskIdentity, Transport};
    use crate::orchestrate::WipeScope;
    use crate::protocol::{PlanBody, decode_event, seal};
    use crate::safety::Policy;
    use crate::wipe::WipeMethod;

    const MIB: u64 = 1 << 20;
    const FILL: u8 = 0xAA;
    const KEY: [u8; KEY_LEN] = [9u8; KEY_LEN];
    const SEED: [u8; 32] = [4u8; 32];

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn usb(serial: &str) -> Disk {
        Disk::new(
            "/dev/sdb",
            Transport::Usb,
            true,
            DiskIdentity {
                serial: Some(serial.into()),
                model: "Test".into(),
                size_bytes: 4 * MIB,
            },
            512,
            Vec::new(),
        )
    }

    fn machine(disk: &Disk) -> FakePlatform {
        let p = FakePlatform::default();
        p.add_disk(disk.clone(), FILL);
        p.set_system_disks(Some(HashSet::new()));
        p
    }

    fn request(disk: &Disk) -> Request {
        Request {
            disk: disk.id.clone(),
            confirmed: disk.identity.clone(),
            typed_serial: disk.identity.serial.clone().unwrap_or_default(),
            policy: Policy::default(),
            protected_paths: Vec::new(),
            scope: WipeScope::WholeDevice,
            method: WipeMethod::Clear,
            table: None,
        }
    }

    /// The three input lines for a plan signed with `KEY`.
    fn input_for(req: Request, expires: u64) -> String {
        let body = PlanBody {
            expires_at_unix: expires,
            nonce: "00112233445566778899aabbccddeeff".into(),
            request: req,
        };
        let env = seal(&KEY, &body).unwrap();
        format!(
            "{}\n{}\n{}\n",
            hex(&KEY),
            hex(&env.mac),
            String::from_utf8(env.body).unwrap()
        )
    }

    fn production(path: &str) -> bool {
        production_device_allowed(path)
    }

    struct FailingLog;

    impl Write for FailingLog {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("disk full"))
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// Runs `serve` and returns the outcome, the events emitted, and the audit log.
    fn run(
        p: &FakePlatform,
        input: String,
        now: u64,
        allowed: &dyn Fn(&str) -> bool,
    ) -> (Served, Vec<WorkerEvent>, String) {
        let mut events = Vec::new();
        let mut log = Vec::new();
        let served = serve(
            p,
            &mut Cursor::new(input.into_bytes()),
            &mut events,
            &mut log,
            now,
            allowed,
            SEED,
        )
        .unwrap();
        let decoded = String::from_utf8(events)
            .unwrap()
            .lines()
            .map(|l| decode_event(l).unwrap())
            .collect();
        (served, decoded, String::from_utf8(log).unwrap())
    }

    #[test]
    fn a_signed_plan_runs_and_reports_progress_then_completion() {
        let disk = usb("SN-1");
        let p = machine(&disk);
        let (served, events, log) = run(&p, input_for(request(&disk), 2_000), 1_000, &production);

        assert_eq!(served, Served::Completed);
        assert!(
            p.contents(&disk.id).iter().all(|&b| b == 0),
            "the disk was wiped"
        );
        assert!(events.iter().any(|e| matches!(e, WorkerEvent::Progress(_))));
        assert_eq!(
            events.last(),
            Some(&WorkerEvent::Finished(Outcome::Completed))
        );
        assert!(log.contains("\"event\":\"begin\""), "begin recorded: {log}");
        assert!(log.contains("\"event\":\"end\""), "end recorded: {log}");
    }

    #[test]
    fn a_tampered_plan_is_refused_and_nothing_is_written() {
        let disk = usb("SN-1");
        let p = machine(&disk);
        // Change the serial in the body after it was signed.
        let input = input_for(request(&disk), 2_000).replace("\"SN-1\"", "\"SN-2\"");
        let (served, events, _) = run(&p, input, 1_000, &production);

        assert_eq!(served, Served::Failed);
        assert!(matches!(
            events.last(),
            Some(WorkerEvent::Finished(Outcome::Failed(r))) if r.contains("signature")
        ));
        assert!(p.contents(&disk.id).iter().all(|&b| b == FILL));
    }

    #[test]
    fn an_expired_plan_is_refused_and_nothing_is_written() {
        let disk = usb("SN-1");
        let p = machine(&disk);
        let (served, events, _) = run(&p, input_for(request(&disk), 1_000), 1_000, &production);

        assert_eq!(served, Served::Failed);
        assert!(matches!(
            events.last(),
            Some(WorkerEvent::Finished(Outcome::Failed(r))) if r.contains("expired")
        ));
        assert!(p.contents(&disk.id).iter().all(|&b| b == FILL));
    }

    #[test]
    fn a_device_outside_the_allowlist_is_refused_and_logged() {
        let disk = usb("SN-1");
        let p = machine(&disk);
        let (served, events, log) = run(&p, input_for(request(&disk), 2_000), 1_000, &|_| false);

        assert_eq!(served, Served::Failed);
        assert_eq!(
            events.last(),
            Some(&WorkerEvent::Finished(Outcome::Refused(
                Refusal::DeviceNotAllowed
            )))
        );
        assert!(
            log.contains("\"event\":\"refused\""),
            "refusal logged: {log}"
        );
        assert!(p.contents(&disk.id).iter().all(|&b| b == FILL));
    }

    #[test]
    fn an_audit_record_that_cannot_be_written_stops_the_operation_before_any_write() {
        let disk = usb("SN-1");
        let p = machine(&disk);
        let mut events = Vec::new();
        let served = serve(
            &p,
            &mut Cursor::new(input_for(request(&disk), 2_000).into_bytes()),
            &mut events,
            &mut FailingLog,
            1_000,
            &production,
            SEED,
        )
        .unwrap();

        assert_eq!(served, Served::Failed);
        assert!(
            String::from_utf8(events)
                .unwrap()
                .contains("audit record could not be written"),
            "the failure is reported"
        );
        assert!(
            p.contents(&disk.id).iter().all(|&b| b == FILL),
            "nothing was written"
        );
    }

    #[test]
    fn malformed_input_is_reported_not_panicked_on() {
        let p = FakePlatform::default();
        let (served, events, _) = run(&p, "not a key\n".to_owned(), 1_000, &production);
        assert_eq!(served, Served::Failed);
        assert!(matches!(
            events.last(),
            Some(WorkerEvent::Finished(Outcome::Failed(r))) if r.contains("malformed")
        ));
    }

    #[test]
    fn only_whole_real_disks_are_allowed() {
        for ok in [
            "/dev/sda",
            "/dev/sdb",
            "/dev/sdaa",
            "/dev/hdc",
            "/dev/vda",
            "/dev/xvdb",
            "/dev/nvme0n1",
            "/dev/nvme12n3",
            "/dev/mmcblk0",
        ] {
            assert!(production_device_allowed(ok), "{ok} should be allowed");
        }
        for refused in [
            "/dev/loop0",
            "/dev/sda1",
            "/dev/nvme0n1p1",
            "/dev/mmcblk0p1",
            "/dev/sd",
            "/dev/sdB",
            "/dev/mapper/vg-root",
            "/dev/dm-0",
            "/dev/sdb/../mem",
            "/etc/passwd",
            "sdb",
            "",
        ] {
            assert!(
                !production_device_allowed(refused),
                "{refused} must be refused"
            );
        }
    }
}

//! The guarded pipeline for a destructive operation. It is the only path from a
//! confirmed request to a write, and it re-checks everything at execution time:
//! it does not reuse the desktop app's earlier verdict.
//!
//! Order of checks:
//! 1. Re-enumerate; the disk must still be present.
//! 2. Re-evaluate safety against the current system set (fail closed).
//! 3. The typed serial must match the serial the device reports now.
//! 4. The enumerated identity must match the identity the user confirmed.
//! 5. Open exclusively, then re-read identity from that handle (time-of-check
//!    versus time-of-use).
//! 6. Validate the table against the real device size, before any write.
//! 7. Wipe, commit the table, rescan, release the locks, then format.
//!
//! Every path ends with exactly one audit record, and the record is written
//! before the first byte changes on the device.

use std::ops::Range;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use serde::{Deserialize, Serialize};

use crate::model::{DiskId, DiskIdentity};
use crate::plan::{PlanError, TablePlan, table_wipe_ranges, validate};
use crate::platform::{OsError, Platform};
use crate::safety::{Policy, Refusal, SafetyInputs, Verdict, confirm_typed, evaluate};
use crate::wipe::{self, Progress, WipeError, WipeMethod};

/// What the operation touches.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WipeScope {
    /// Overwrite every byte. The default for a full wipe.
    WholeDevice,
    /// Zero the head and tail where partition tables live. Leaves the data area as is.
    TableOnly,
}

/// A confirmed request. The worker receives this inside a signed envelope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    pub disk: DiskId,
    /// The identity the user looked at when they typed the serial.
    pub confirmed: DiskIdentity,
    pub typed_serial: String,
    pub policy: Policy,
    pub protected_paths: Vec<PathBuf>,
    pub scope: WipeScope,
    pub method: WipeMethod,
    pub table: Option<TablePlan>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    Completed,
    Refused(Refusal),
    Cancelled,
    /// A short description of the failure. Never includes the typed serial.
    Failed(String),
}

/// Receives a durable record of each operation. `begin` must not return `Ok` until
/// the record is on stable storage, because the first write follows it. An `Err`
/// from `begin` stops the operation before anything is written.
pub trait Audit {
    fn begin(&mut self, request: &Request) -> Result<(), String>;
    fn progress(&mut self, progress: &Progress);
    fn end(&mut self, outcome: &Outcome);
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("the disk is no longer present")]
    DiskGone,
    #[error("refused: {0:?}")]
    Refused(Refusal),
    #[error("the typed serial does not match")]
    ConfirmationMismatch,
    #[error("the device identity changed since it was confirmed")]
    IdentityChanged,
    #[error("the table is not valid for this device: {0}")]
    Plan(#[from] PlanError),
    #[error(transparent)]
    Os(#[from] OsError),
    #[error(transparent)]
    Wipe(#[from] WipeError),
    #[error("the audit record could not be written, so nothing was changed: {0}")]
    Audit(String),
}

/// Runs a confirmed request. `seed` comes from the caller, so tests are reproducible.
pub fn execute(
    platform: &dyn Platform,
    request: &Request,
    seed: [u8; 32],
    cancel: &AtomicBool,
    audit: &mut dyn Audit,
) -> Result<(), StorageError> {
    if let Err(e) = audit.begin(request) {
        return Err(StorageError::Audit(e));
    }
    let result = run(platform, request, seed, cancel, audit);
    let outcome = match &result {
        Ok(()) => Outcome::Completed,
        Err(StorageError::Refused(r)) => Outcome::Refused(*r),
        Err(StorageError::Wipe(WipeError::Cancelled { .. })) => Outcome::Cancelled,
        Err(e) => Outcome::Failed(e.to_string()),
    };
    audit.end(&outcome);
    result
}

// The whole-device case is a single range, which the slice API takes as one element.
#[allow(clippy::single_range_in_vec_init)]
fn run(
    platform: &dyn Platform,
    req: &Request,
    seed: [u8; 32],
    cancel: &AtomicBool,
    audit: &mut dyn Audit,
) -> Result<(), StorageError> {
    // 1. Re-enumerate. Never reuse a device record from earlier.
    let disks = platform.enumerate()?;
    let disk = disks
        .iter()
        .find(|d| d.id == req.disk)
        .ok_or(StorageError::DiskGone)?;

    // 2. Re-evaluate safety against the current system set.
    let system = platform.system_disks();
    let inputs = SafetyInputs {
        system_disks: system.as_ref(),
        protected_paths: &req.protected_paths,
    };
    let confirmation = match evaluate(disk, &inputs, req.policy) {
        Verdict::Destructive(c) => c,
        Verdict::ReadOnly(r) => return Err(StorageError::Refused(r)),
    };

    // 3. The typed serial must match the serial the device reports now.
    if !confirm_typed(&req.typed_serial, &confirmation) {
        return Err(StorageError::ConfirmationMismatch);
    }

    // 4. The device must still be the one the user confirmed.
    if disk.identity != req.confirmed {
        return Err(StorageError::IdentityChanged);
    }

    // 5. Open exclusively, then check identity on the handle we will write to.
    let mut dev = platform.open_exclusive(disk)?;
    if dev.identity()? != req.confirmed {
        return Err(StorageError::IdentityChanged);
    }

    // 6. Validate the table against the real size, before any byte changes.
    let size = dev.size();
    if let Some(table) = &req.table {
        validate(table, size)?;
    }

    // 7. Wipe the chosen ranges.
    let ranges: Vec<Range<u64>> = match req.scope {
        WipeScope::WholeDevice => vec![0..size],
        WipeScope::TableOnly => table_wipe_ranges(size),
    };
    wipe::run_ranges(dev.as_mut(), &ranges, req.method, seed, cancel, |p| {
        audit.progress(&p)
    })?;

    // 8. Commit the table and rescan, then release the volume locks before formatting.
    let parts = match &req.table {
        Some(table) => {
            dev.commit_table(table)?;
            dev.rescan()?
        }
        None => Vec::new(),
    };
    drop(dev);

    // 9. Format each partition in table order.
    if let Some(table) = &req.table {
        if parts.len() != table.partitions.len() {
            return Err(StorageError::Os(OsError::Other(format!(
                "rescan found {} partitions, expected {}",
                parts.len(),
                table.partitions.len()
            ))));
        }
        for (part, spec) in parts.iter().zip(&table.partitions) {
            platform.format_partition(part, spec.fs, &spec.label)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::AtomicBool;

    use super::*;
    use crate::fake::FakePlatform;
    use crate::model::{Disk, DiskIdentity, Mount, Transport};
    use crate::plan::{FsKind, PartitionSpec, SizeSpec, TableStyle};

    const FILL: u8 = 0xAA;
    const SIZE: u64 = 4 << 20;
    const SEED: [u8; 32] = [3u8; 32];

    #[derive(Default)]
    struct Recorder {
        begun: usize,
        progress: usize,
        outcomes: Vec<Outcome>,
    }

    impl Audit for Recorder {
        fn begin(&mut self, _: &Request) -> Result<(), String> {
            self.begun += 1;
            Ok(())
        }
        fn progress(&mut self, _: &Progress) {
            self.progress += 1;
        }
        fn end(&mut self, outcome: &Outcome) {
            self.outcomes.push(outcome.clone());
        }
    }

    fn identity(serial: Option<&str>) -> DiskIdentity {
        DiskIdentity {
            serial: serial.map(str::to_owned),
            model: "Test Drive".into(),
            size_bytes: SIZE,
        }
    }

    fn usb(serial: Option<&str>, mounts: Vec<Mount>) -> Disk {
        Disk::new(
            "/dev/sdb",
            Transport::Usb,
            true,
            identity(serial),
            512,
            mounts,
        )
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

    fn table() -> TablePlan {
        TablePlan {
            style: TableStyle::Gpt,
            partitions: vec![
                PartitionSpec {
                    size: SizeSpec::Bytes(1 << 20),
                    fs: FsKind::Fat32,
                    label: "BOOT".into(),
                },
                PartitionSpec {
                    size: SizeSpec::Remaining,
                    fs: FsKind::ExFat,
                    label: "DATA".into(),
                },
            ],
        }
    }

    /// A platform with one clean external disk and an empty system set.
    fn machine(disk: &Disk) -> FakePlatform {
        let p = FakePlatform::default();
        p.add_disk(disk.clone(), FILL);
        p.set_system_disks(Some(HashSet::new()));
        p
    }

    use std::collections::HashSet;

    #[test]
    fn happy_path_wipes_commits_the_table_and_formats_both_partitions() {
        let disk = usb(Some("SN-1"), vec![]);
        let p = machine(&disk);
        let mut req = request(&disk);
        req.table = Some(table());
        let mut audit = Recorder::default();
        let cancel = AtomicBool::new(false);

        execute(&p, &req, SEED, &cancel, &mut audit).unwrap();

        assert_eq!(audit.begun, 1);
        assert!(audit.progress > 0);
        assert_eq!(audit.outcomes, vec![Outcome::Completed]);
        assert_eq!(p.committed_table(&disk.id), Some(table()));
        let formatted = p.formatted();
        assert_eq!(formatted.len(), 2);
        assert_eq!(formatted[0].1, FsKind::Fat32);
        assert_eq!(formatted[1].2, "DATA");
    }

    #[test]
    fn system_disk_is_refused_and_nothing_is_written() {
        let disk = usb(Some("SN-1"), vec![]);
        let p = machine(&disk);
        p.set_system_disks(Some([disk.id.clone()].into_iter().collect()));
        let mut audit = Recorder::default();
        let cancel = AtomicBool::new(false);

        let err = execute(&p, &request(&disk), SEED, &cancel, &mut audit).unwrap_err();

        assert!(matches!(err, StorageError::Refused(Refusal::SystemDisk)));
        assert_eq!(audit.outcomes, vec![Outcome::Refused(Refusal::SystemDisk)]);
        assert!(p.contents(&disk.id).iter().all(|&b| b == FILL));
    }

    #[test]
    fn unknown_system_set_fails_closed() {
        let disk = usb(Some("SN-1"), vec![]);
        let p = machine(&disk);
        p.set_system_disks(None);
        let mut audit = Recorder::default();
        let cancel = AtomicBool::new(false);

        let err = execute(&p, &request(&disk), SEED, &cancel, &mut audit).unwrap_err();

        assert!(matches!(
            err,
            StorageError::Refused(Refusal::UnknownSystemSet)
        ));
        assert!(p.contents(&disk.id).iter().all(|&b| b == FILL));
    }

    #[test]
    fn internal_disk_is_refused_by_default() {
        let mut disk = usb(Some("SN-1"), vec![]);
        disk.removable = false;
        let p = machine(&disk);
        let mut audit = Recorder::default();
        let cancel = AtomicBool::new(false);

        let err = execute(&p, &request(&disk), SEED, &cancel, &mut audit).unwrap_err();

        assert!(matches!(
            err,
            StorageError::Refused(Refusal::InternalNotAllowed)
        ));
        assert!(p.contents(&disk.id).iter().all(|&b| b == FILL));
    }

    #[test]
    fn wrong_typed_serial_is_refused_and_nothing_is_written() {
        let disk = usb(Some("SN-1"), vec![]);
        let p = machine(&disk);
        let mut req = request(&disk);
        req.typed_serial = "SN-2".into();
        let mut audit = Recorder::default();
        let cancel = AtomicBool::new(false);

        let err = execute(&p, &req, SEED, &cancel, &mut audit).unwrap_err();

        assert!(matches!(err, StorageError::ConfirmationMismatch));
        assert!(p.contents(&disk.id).iter().all(|&b| b == FILL));
        // The audit trail must not record the typed serial.
        let recorded = format!("{:?}", audit.outcomes);
        assert!(!recorded.contains("SN-2"));
    }

    #[test]
    fn identity_swapped_after_confirmation_is_refused() {
        let disk = usb(Some("SN-1"), vec![]);
        let p = machine(&disk);
        // The device enumerates as confirmed, but reports a different identity on open.
        p.change_identity_on_open(&disk.id, identity(Some("SN-OTHER")));
        let mut audit = Recorder::default();
        let cancel = AtomicBool::new(false);

        let err = execute(&p, &request(&disk), SEED, &cancel, &mut audit).unwrap_err();

        assert!(matches!(err, StorageError::IdentityChanged));
        assert!(p.contents(&disk.id).iter().all(|&b| b == FILL));
    }

    #[test]
    fn disk_removed_before_execution_is_reported_as_gone() {
        let disk = usb(Some("SN-1"), vec![]);
        let p = FakePlatform::default();
        p.set_system_disks(Some(HashSet::new()));
        let mut audit = Recorder::default();
        let cancel = AtomicBool::new(false);

        let err = execute(&p, &request(&disk), SEED, &cancel, &mut audit).unwrap_err();

        assert!(matches!(err, StorageError::DiskGone));
    }

    #[test]
    fn invalid_label_is_rejected_before_any_write() {
        let disk = usb(Some("SN-1"), vec![]);
        let p = machine(&disk);
        let mut req = request(&disk);
        let mut bad = table();
        bad.partitions[1].label = "a:b".into();
        req.table = Some(bad);
        let mut audit = Recorder::default();
        let cancel = AtomicBool::new(false);

        let err = execute(&p, &req, SEED, &cancel, &mut audit).unwrap_err();

        assert!(matches!(
            err,
            StorageError::Plan(PlanError::LabelForbidden(_))
        ));
        assert!(p.contents(&disk.id).iter().all(|&b| b == FILL));
        assert!(p.committed_table(&disk.id).is_none());
    }

    #[test]
    fn cancellation_is_recorded_as_cancelled() {
        let disk = usb(Some("SN-1"), vec![]);
        let p = machine(&disk);
        let mut audit = Recorder::default();
        let cancel = AtomicBool::new(true);

        let err = execute(&p, &request(&disk), SEED, &cancel, &mut audit).unwrap_err();

        assert!(matches!(
            err,
            StorageError::Wipe(WipeError::Cancelled { .. })
        ));
        assert_eq!(audit.outcomes, vec![Outcome::Cancelled]);
    }

    #[test]
    fn format_failure_is_reported_after_the_wipe_and_table() {
        let disk = usb(Some("SN-1"), vec![]);
        let p = machine(&disk);
        p.fail_formats(true);
        let mut req = request(&disk);
        req.table = Some(table());
        let mut audit = Recorder::default();
        let cancel = AtomicBool::new(false);

        let err = execute(&p, &req, SEED, &cancel, &mut audit).unwrap_err();

        assert!(matches!(err, StorageError::Os(_)));
        assert!(matches!(audit.outcomes.as_slice(), [Outcome::Failed(_)]));
        assert_eq!(p.committed_table(&disk.id), Some(table()));
    }

    #[test]
    fn table_only_scope_leaves_the_data_area_untouched() {
        let disk = usb(Some("SN-1"), vec![]);
        let p = machine(&disk);
        let mut req = request(&disk);
        req.scope = WipeScope::TableOnly;
        let mut audit = Recorder::default();
        let cancel = AtomicBool::new(false);

        execute(&p, &req, SEED, &cancel, &mut audit).unwrap();

        let data = p.contents(&disk.id);
        let mib = 1usize << 20;
        assert!(data[..mib].iter().all(|&b| b == 0));
        assert!(data[mib..data.len() - mib].iter().all(|&b| b == FILL));
        assert!(data[data.len() - mib..].iter().all(|&b| b == 0));
    }

    #[test]
    fn mounted_protected_path_is_refused() {
        let mount = Mount {
            point: PathBuf::from("/media/usb"),
            label: None,
        };
        let disk = usb(Some("SN-1"), vec![mount]);
        let p = machine(&disk);
        let mut req = request(&disk);
        req.protected_paths = vec![PathBuf::from("/media/usb/fcopy-state")];
        let mut audit = Recorder::default();
        let cancel = AtomicBool::new(false);

        let err = execute(&p, &req, SEED, &cancel, &mut audit).unwrap_err();

        assert!(matches!(err, StorageError::Refused(Refusal::ProtectedPath)));
        assert!(p.contents(&disk.id).iter().all(|&b| b == FILL));
    }
}

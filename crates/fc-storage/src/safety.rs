//! Guardrails for destructive operations. Pure: every input maps to exactly
//! one verdict. Callers treat `ReadOnly` as a hard stop.

use std::collections::HashSet;
use std::path::PathBuf;

use crate::model::{Disk, DiskId};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// We cannot prove which physical device this is.
    NoSerial,
    /// Hosts the OS, boot partition, swap, the EFI partition, or the running binary.
    SystemDisk,
    /// The system set could not be determined. Fail closed.
    UnknownSystemSet,
    /// Internal disk, and the policy does not allow it.
    InternalNotAllowed,
    /// Hosts an FCOPY state directory or an active job destination.
    ProtectedPath,
}

/// What the user must type back to proceed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirmation {
    pub expected_serial: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    ReadOnly(Refusal),
    Destructive(Confirmation),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Policy {
    /// Off by default. Internal disks stay excluded until Phase 5 policy support.
    pub allow_internal: bool,
}

pub struct SafetyInputs<'a> {
    /// `None` means the OS could not determine the system set.
    pub system_disks: Option<&'a HashSet<DiskId>>,
    /// Paths the operation must not destroy, such as state directories
    /// and active job destinations.
    pub protected_paths: &'a [PathBuf],
}

pub fn evaluate(disk: &Disk, inputs: &SafetyInputs<'_>, policy: Policy) -> Verdict {
    let serial = match disk.identity.serial.as_deref().map(str::trim) {
        Some(s) if !s.is_empty() => s,
        _ => return Verdict::ReadOnly(Refusal::NoSerial),
    };
    let Some(system) = inputs.system_disks else {
        return Verdict::ReadOnly(Refusal::UnknownSystemSet);
    };
    if system.contains(&disk.id) {
        return Verdict::ReadOnly(Refusal::SystemDisk);
    }
    if !disk.removable && !policy.allow_internal {
        return Verdict::ReadOnly(Refusal::InternalNotAllowed);
    }
    let hosts_protected = inputs
        .protected_paths
        .iter()
        .any(|p| disk.mounts.iter().any(|m| p.starts_with(&m.point)));
    if hosts_protected {
        return Verdict::ReadOnly(Refusal::ProtectedPath);
    }
    Verdict::Destructive(Confirmation {
        expected_serial: serial.to_owned(),
        size_bytes: disk.identity.size_bytes,
    })
}

/// The typed confirmation must match the serial exactly. Surrounding
/// whitespace is ignored; case is not.
pub fn confirm_typed(typed: &str, c: &Confirmation) -> bool {
    typed.trim() == c.expected_serial
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;
    use crate::model::{DiskIdentity, Mount, Transport};

    fn disk(serial: Option<&str>, removable: bool, mounts: Vec<Mount>) -> Disk {
        Disk::new(
            "/dev/sdb",
            Transport::Usb,
            removable,
            DiskIdentity {
                serial: serial.map(str::to_owned),
                model: "Test Drive".into(),
                size_bytes: 64_000_000_000,
            },
            512,
            mounts,
        )
    }

    fn mount(point: &str) -> Mount {
        Mount {
            point: PathBuf::from(point),
            label: None,
        }
    }

    fn inputs<'a>(
        system: Option<&'a HashSet<DiskId>>,
        protected: &'a [PathBuf],
    ) -> SafetyInputs<'a> {
        SafetyInputs {
            system_disks: system,
            protected_paths: protected,
        }
    }

    #[test]
    fn missing_serial_is_read_only() {
        let d = disk(None, true, vec![]);
        let empty = HashSet::new();
        let v = evaluate(&d, &inputs(Some(&empty), &[]), Policy::default());
        assert_eq!(v, Verdict::ReadOnly(Refusal::NoSerial));
    }

    #[test]
    fn blank_serial_is_read_only() {
        let d = disk(Some("   "), true, vec![]);
        let empty = HashSet::new();
        let v = evaluate(&d, &inputs(Some(&empty), &[]), Policy::default());
        assert_eq!(v, Verdict::ReadOnly(Refusal::NoSerial));
    }

    #[test]
    fn unknown_system_set_fails_closed_even_for_a_clean_disk() {
        let d = disk(Some("ABC123"), true, vec![]);
        let v = evaluate(&d, &inputs(None, &[]), Policy::default());
        assert_eq!(v, Verdict::ReadOnly(Refusal::UnknownSystemSet));
    }

    #[test]
    fn system_disk_is_refused() {
        let d = disk(Some("ABC123"), true, vec![]);
        let system: HashSet<DiskId> = [d.id.clone()].into_iter().collect();
        let v = evaluate(&d, &inputs(Some(&system), &[]), Policy::default());
        assert_eq!(v, Verdict::ReadOnly(Refusal::SystemDisk));
    }

    #[test]
    fn internal_disk_is_refused_by_default_and_allowed_by_policy() {
        let d = disk(Some("ABC123"), false, vec![]);
        let empty = HashSet::new();
        let default = evaluate(&d, &inputs(Some(&empty), &[]), Policy::default());
        assert_eq!(default, Verdict::ReadOnly(Refusal::InternalNotAllowed));

        let allowed = evaluate(
            &d,
            &inputs(Some(&empty), &[]),
            Policy {
                allow_internal: true,
            },
        );
        assert!(matches!(allowed, Verdict::Destructive(_)));
    }

    #[test]
    fn protected_path_under_a_mount_is_refused() {
        let d = disk(Some("ABC123"), true, vec![mount("/media/usb")]);
        let empty = HashSet::new();
        let protected = [PathBuf::from("/media/usb/fcopy-state")];
        let v = evaluate(&d, &inputs(Some(&empty), &protected), Policy::default());
        assert_eq!(v, Verdict::ReadOnly(Refusal::ProtectedPath));
    }

    #[test]
    fn clean_external_disk_is_destructive_with_its_serial() {
        let d = disk(Some("  ABC123 "), true, vec![mount("/media/usb")]);
        let empty = HashSet::new();
        let v = evaluate(&d, &inputs(Some(&empty), &[]), Policy::default());
        assert_eq!(
            v,
            Verdict::Destructive(Confirmation {
                expected_serial: "ABC123".into(),
                size_bytes: 64_000_000_000,
            })
        );
    }

    #[test]
    fn typed_confirmation_is_exact_but_ignores_outer_whitespace() {
        let c = Confirmation {
            expected_serial: "AbC123".into(),
            size_bytes: 1,
        };
        assert!(confirm_typed("AbC123", &c));
        assert!(confirm_typed("  AbC123\n", &c));
        assert!(!confirm_typed("abc123", &c));
        assert!(!confirm_typed("AbC12", &c));
        assert!(!confirm_typed("", &c));
    }
}

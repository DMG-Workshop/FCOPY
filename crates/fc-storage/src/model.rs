//! Device model shared by every storage module. Plain data, no OS calls.

use std::fmt;
use std::path::PathBuf;

/// How the disk is attached. Drives the default policy (external only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Transport {
    Usb,
    Thunderbolt,
    Sata,
    Nvme,
    Scsi,
    Unknown,
}

/// The facts used to prove we are touching the same physical device.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DiskIdentity {
    /// `None` when the OS does not report one. Such disks are read-only.
    pub serial: Option<String>,
    pub model: String,
    pub size_bytes: u64,
}

/// Stable key built from transport, serial and size.
///
/// It is never derived from a device path or index, because those renumber
/// when a drive is re-plugged.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DiskId(String);

impl DiskId {
    pub fn derive(transport: Transport, identity: &DiskIdentity) -> Self {
        let serial = identity
            .serial
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("-");
        Self(format!("{transport:?}:{serial}:{}", identity.size_bytes))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for DiskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// A volume mounted from a disk, as the OS reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    pub point: PathBuf,
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disk {
    pub id: DiskId,
    /// OS path for display and for the worker only. Never used as a key.
    pub os_path: String,
    pub transport: Transport,
    pub removable: bool,
    pub identity: DiskIdentity,
    pub logical_block: u32,
    pub mounts: Vec<Mount>,
}

impl Disk {
    /// Builds the disk and derives its `id`, so the two can never disagree.
    pub fn new(
        os_path: impl Into<String>,
        transport: Transport,
        removable: bool,
        identity: DiskIdentity,
        logical_block: u32,
        mounts: Vec<Mount>,
    ) -> Self {
        let id = DiskId::derive(transport, &identity);
        Self {
            id,
            os_path: os_path.into(),
            transport,
            removable,
            identity,
            logical_block,
            mounts,
        }
    }
}

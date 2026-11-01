//! Traits every OS adapter implements. Adapters for Linux (read-only so far)
//! and the in-memory fake live beside this module. Destructive methods must
//! refuse anywhere outside the privileged worker.

use std::collections::HashSet;

use crate::health::Health;
use crate::model::{Disk, DiskId, DiskIdentity};
use crate::plan::{FsKind, TablePlan};
use crate::wipe::RawIo;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum OsError {
    #[error("permission denied: this operation needs the privileged worker")]
    Permission,
    #[error("device busy: a volume could not be unmounted or locked")]
    Busy,
    #[error("not available in this build: {0}")]
    Unsupported(&'static str),
    #[error("{0}")]
    Other(String),
}

/// A partition as the OS reports it after the table is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionRef {
    pub index: u32,
    pub os_path: String,
    pub offset: u64,
    pub size: u64,
}

pub trait Platform: Send + Sync {
    /// Read-only enumeration. Safe for the web dashboard.
    fn enumerate(&self) -> Result<Vec<Disk>, OsError>;

    /// The disks that host the running system. `None` means the set could not
    /// be determined, and callers must fail closed.
    fn system_disks(&self) -> Option<HashSet<DiskId>>;

    /// Read-only health. Many enclosures hide SMART; report that honestly.
    fn health(&self, disk: &Disk) -> Result<Health, OsError>;

    /// Unmounts and locks every volume on the disk. Dropping the handle releases the locks.
    fn open_exclusive(&self, disk: &Disk) -> Result<Box<dyn ExclusiveDisk>, OsError>;

    fn format_partition(&self, part: &PartitionRef, fs: FsKind, label: &str)
    -> Result<(), OsError>;
}

/// A disk held open for writing, with its volume locks.
pub trait ExclusiveDisk: RawIo {
    /// Identity re-read from the open handle, for the time-of-check check.
    fn identity(&self) -> Result<DiskIdentity, OsError>;
    fn commit_table(&mut self, table: &TablePlan) -> Result<(), OsError>;
    fn rescan(&mut self) -> Result<Vec<PartitionRef>, OsError>;
}

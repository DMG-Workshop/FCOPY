//! Error model for the copy engine.
//!
//! Two layers:
//!
//! * [`CopyError`] – the typed error returned from public APIs.
//! * [`ErrorClass`] – a *portable* classification of an `io::Error`, so that
//!   recovery policy ("retry locked files, skip permission errors, cancel when
//!   the disk is full") is written once and works on Windows, macOS and Linux,
//!   where the raw OS error codes differ.

use std::fmt;
use std::io;
use std::path::PathBuf;

use thiserror::Error;

use crate::checksum::Algorithm;

/// The step of the copy pipeline that was executing when something failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Operation {
    CheckSpace,
    CreateDir,
    OpenSource,
    OpenDestination,
    Preallocate,
    Read,
    Write,
    Sync,
    Verify,
    Finalize,
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Operation::CheckSpace => "checking free space",
            Operation::CreateDir => "creating directory",
            Operation::OpenSource => "opening source",
            Operation::OpenDestination => "opening destination",
            Operation::Preallocate => "preallocating destination",
            Operation::Read => "reading",
            Operation::Write => "writing",
            Operation::Sync => "flushing to disk",
            Operation::Verify => "verifying",
            Operation::Finalize => "finalizing (rename)",
        })
    }
}

/// Portable classification of a failure; this is what recovery policy keys on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorClass {
    /// File is in use / locked by another process (often clears on its own).
    Locked,
    /// Access denied, read-only filesystem, write-protected media.
    PermissionDenied,
    /// Device-level I/O failure: unreadable sector, CRC error, unplugged media.
    BadSector,
    /// No space left on device or quota exceeded. Every later file will fail
    /// too, so the default policy cancels the job.
    DiskFull,
    NotFound,
    AlreadyExists,
    /// Destination bytes differ from what was read from the source.
    Mismatch,
    Other,
}

impl fmt::Display for ErrorClass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            ErrorClass::Locked => "file locked / busy",
            ErrorClass::PermissionDenied => "permission denied",
            ErrorClass::BadSector => "device I/O error (bad sector?)",
            ErrorClass::DiskFull => "disk full",
            ErrorClass::NotFound => "not found",
            ErrorClass::AlreadyExists => "already exists",
            ErrorClass::Mismatch => "checksum mismatch",
            ErrorClass::Other => "other error",
        })
    }
}

/// Classify an `io::Error`. Raw OS codes are consulted first (they carry the
/// most precise meaning, e.g. Windows sharing violations or Unix `EIO`); the
/// portable [`io::ErrorKind`] is the fallback.
pub fn classify(err: &io::Error) -> ErrorClass {
    if let Some(code) = err.raw_os_error() {
        if let Some(class) = classify_os_code(code) {
            return class;
        }
    }
    use io::ErrorKind as K;
    match err.kind() {
        K::PermissionDenied | K::ReadOnlyFilesystem => ErrorClass::PermissionDenied,
        K::NotFound => ErrorClass::NotFound,
        K::AlreadyExists => ErrorClass::AlreadyExists,
        K::StorageFull | K::QuotaExceeded => ErrorClass::DiskFull,
        K::ResourceBusy | K::WouldBlock | K::TimedOut | K::Deadlock => ErrorClass::Locked,
        _ => ErrorClass::Other,
    }
}

#[cfg(unix)]
fn classify_os_code(code: i32) -> Option<ErrorClass> {
    Some(match code {
        libc::EIO => ErrorClass::BadSector,
        libc::ENOSPC | libc::EDQUOT => ErrorClass::DiskFull,
        libc::EBUSY | libc::ETXTBSY | libc::EAGAIN => ErrorClass::Locked,
        libc::EACCES | libc::EPERM | libc::EROFS => ErrorClass::PermissionDenied,
        libc::ENOENT => ErrorClass::NotFound,
        libc::EEXIST => ErrorClass::AlreadyExists,
        _ => return None,
    })
}

#[cfg(windows)]
fn classify_os_code(code: i32) -> Option<ErrorClass> {
    // Win32 system error codes (winerror.h).
    Some(match code {
        32 | 33 => ErrorClass::Locked,                      // SHARING / LOCK_VIOLATION
        5 | 19 => ErrorClass::PermissionDenied,             // ACCESS_DENIED / WRITE_PROTECT
        2 | 3 => ErrorClass::NotFound,                      // FILE / PATH_NOT_FOUND
        80 | 183 => ErrorClass::AlreadyExists,              // FILE_EXISTS / ALREADY_EXISTS
        39 | 112 | 1816 => ErrorClass::DiskFull,            // HANDLE_DISK_FULL / DISK_FULL / NOT_ENOUGH_QUOTA
        23 | 27 | 1117 | 1127 | 1393 => ErrorClass::BadSector, // CRC / SECTOR_NOT_FOUND / IO_DEVICE / DISK_OP_FAILED / DISK_CORRUPT
        _ => return None,
    })
}

#[cfg(not(any(unix, windows)))]
fn classify_os_code(_code: i32) -> Option<ErrorClass> {
    None
}

/// Errors returned by the public API.
///
/// Note that "the user chose Skip" is *not* an error: it is reported as
/// [`crate::CopyOutcome::Skipped`], which still carries the underlying
/// `CopyError` for the log.
#[derive(Debug, Error)]
pub enum CopyError {
    #[error("operation cancelled")]
    Cancelled,

    #[error("skipped by user")]
    SkippedByUser,

    /// Failure reported by a transfer backend (e.g. a remote agent).
    #[error("{0}")]
    Backend(String),

    #[error(
        "not enough free space for {}: need {needed} bytes (+{reserve} reserve), {available} available",
        dest.display()
    )]
    InsufficientSpace {
        dest: PathBuf,
        needed: u64,
        available: u64,
        reserve: u64,
    },

    #[error("{op} {}: {source}", path.display())]
    Io {
        op: Operation,
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("destination already exists: {}", .0.display())]
    DestinationExists(PathBuf),

    #[error("source is not a regular file: {}", .0.display())]
    NotRegularFile(PathBuf),

    #[error("source changed while it was being copied: {}", .0.display())]
    SourceChanged(PathBuf),

    #[error(
        "{algorithm} mismatch for {}: source {expected}, destination {actual}",
        path.display()
    )]
    VerifyMismatch {
        path: PathBuf,
        algorithm: Algorithm,
        expected: String,
        actual: String,
    },
}

impl CopyError {
    pub(crate) fn io(op: Operation, path: impl Into<PathBuf>, source: io::Error) -> Self {
        CopyError::Io { op, path: path.into(), source }
    }

    /// Portable classification of this error.
    pub fn class(&self) -> ErrorClass {
        match self {
            CopyError::Io { source, .. } => classify(source),
            CopyError::InsufficientSpace { .. } => ErrorClass::DiskFull,
            CopyError::DestinationExists(_) => ErrorClass::AlreadyExists,
            CopyError::VerifyMismatch { .. } => ErrorClass::Mismatch,
            CopyError::NotRegularFile(_) | CopyError::SourceChanged(_) => ErrorClass::Other,
            CopyError::Cancelled | CopyError::SkippedByUser | CopyError::Backend(_) => ErrorClass::Other,
        }
    }
}

/// Convenience alias.
pub type Result<T, E = CopyError> = std::result::Result<T, E>;

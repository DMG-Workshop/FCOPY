//! Destination free-space pre-check.
//!
//! Failing *before* the first byte is written is far better than failing at
//! 97 % of a 200 GB job. Three things make a naive check wrong, and this module
//! handles each:
//!
//! 1. **The destination may not exist yet.** We probe the nearest existing
//!    ancestor, which lives on the same filesystem the new path will be on.
//! 2. **Small files waste space.** A 1-byte file occupies a whole cluster/block,
//!    so a tree of 100k tiny files needs far more than the sum of its sizes.
//!    [`TreeStats::allocated`] rounds every file up to the block size.
//! 3. **Overwrites are not free.** The engine writes `name.fcpart` and renames
//!    it over the old file, so old and new copies coexist briefly. We therefore
//!    never credit the size of a file that is about to be replaced.
//!
//! "Available" means *available to this user* (`f_bavail` on Unix, the
//! caller-quota-aware figure on Windows), so reserved root blocks and per-user
//! quotas are respected.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::error::{CopyError, Operation, Result};

/// Headroom left free by default for filesystem metadata and other writers.
pub const DEFAULT_RESERVE: u64 = 16 * 1024 * 1024;
/// Block size assumed when rounding file sizes up (4 KiB is the common minimum).
pub const DEFAULT_BLOCK_SIZE: u64 = 4096;

/// Outcome of a space probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpaceReport {
    /// The existing directory that was actually queried.
    pub probe: PathBuf,
    /// Bytes the job needs.
    pub needed: u64,
    /// Bytes available to this user on the destination filesystem.
    pub available: u64,
    /// Extra headroom that must remain free.
    pub reserve: u64,
}

impl SpaceReport {
    pub fn is_sufficient(&self) -> bool {
        self.available >= self.needed.saturating_add(self.reserve)
    }

    /// How many more bytes would be required (0 if sufficient).
    pub fn shortfall(&self) -> u64 {
        self.needed.saturating_add(self.reserve).saturating_sub(self.available)
    }
}

/// Round `len` up to a multiple of `block` (`block == 0` returns `len`).
pub fn round_up(len: u64, block: u64) -> u64 {
    if block == 0 {
        return len;
    }
    len.div_ceil(block).saturating_mul(block)
}

/// Nearest ancestor of `path` (or `path` itself) that exists.
fn nearest_existing(path: &Path) -> PathBuf {
    let mut cur = path;
    loop {
        if cur.as_os_str().is_empty() {
            return PathBuf::from(".");
        }
        if fs::metadata(cur).is_ok() {
            return cur.to_path_buf();
        }
        match cur.parent() {
            Some(p) => cur = p,
            None => return PathBuf::from("."),
        }
    }
}

/// Bytes available to the current user on the filesystem that holds `path`
/// (which need not exist yet).
pub fn available_space(path: &Path) -> io::Result<u64> {
    fs4::available_space(nearest_existing(path))
}

/// Probe free space without judging it.
pub fn probe_space(dest: &Path, needed: u64, reserve: u64) -> Result<SpaceReport> {
    let probe = nearest_existing(dest);
    let available =
        fs4::available_space(&probe).map_err(|e| CopyError::io(Operation::CheckSpace, &probe, e))?;
    Ok(SpaceReport { probe, needed, available, reserve })
}

/// Verify that `dest` can hold `needed` more bytes plus `reserve`.
///
/// Returns the [`SpaceReport`] on success and
/// [`CopyError::InsufficientSpace`] otherwise.
pub fn check_space(dest: &Path, needed: u64, reserve: u64) -> Result<SpaceReport> {
    let report = probe_space(dest, needed, reserve)?;
    if report.is_sufficient() {
        Ok(report)
    } else {
        Err(CopyError::InsufficientSpace {
            dest: report.probe,
            needed,
            available: report.available,
            reserve,
        })
    }
}

/// Size of a source tree, for sizing a job before it starts.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TreeStats {
    pub files: u64,
    pub dirs: u64,
    /// Sum of logical file sizes.
    pub bytes: u64,
    /// Sum of file sizes each rounded up to the block size: what the copy will
    /// really occupy on the destination.
    pub allocated: u64,
    /// Symlinks and special files that will not be copied as data.
    pub skipped_special: u64,
    /// Directories that could not be read (permission etc.).
    pub unreadable: u64,
}

/// Walk `root` (a file or directory) without following symlinks and total it up.
pub fn measure_tree(root: &Path, block_size: u64) -> io::Result<TreeStats> {
    let mut stats = TreeStats::default();
    let top = fs::symlink_metadata(root)?;
    if top.is_file() {
        stats.files = 1;
        stats.bytes = top.len();
        stats.allocated = round_up(top.len(), block_size);
        return Ok(stats);
    }
    if !top.is_dir() {
        stats.skipped_special = 1;
        return Ok(stats);
    }

    let mut stack = vec![root.to_path_buf()];
    stats.dirs = 1;
    while let Some(dir) = stack.pop() {
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => {
                stats.unreadable += 1;
                continue;
            }
        };
        for entry in entries.flatten() {
            let Ok(meta) = entry.metadata() else {
                stats.unreadable += 1;
                continue;
            };
            // `DirEntry::metadata` does not follow symlinks.
            let ft = meta.file_type();
            if ft.is_dir() {
                stats.dirs += 1;
                stack.push(entry.path());
            } else if ft.is_file() {
                stats.files += 1;
                stats.bytes = stats.bytes.saturating_add(meta.len());
                stats.allocated = stats.allocated.saturating_add(round_up(meta.len(), block_size));
            } else {
                stats.skipped_special += 1;
            }
        }
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_up_works() {
        assert_eq!(round_up(0, 4096), 0);
        assert_eq!(round_up(1, 4096), 4096);
        assert_eq!(round_up(4096, 4096), 4096);
        assert_eq!(round_up(4097, 4096), 8192);
        assert_eq!(round_up(5, 0), 5);
        assert_eq!(round_up(u64::MAX, 4096), u64::MAX); // saturates, no panic
    }

    #[test]
    fn probes_nearest_existing_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let deep = dir.path().join("a/b/c/file.bin");
        let r = probe_space(&deep, 1, 0).unwrap();
        assert_eq!(r.probe, dir.path());
        assert!(r.available > 0);
    }

    #[test]
    fn sufficient_and_insufficient() {
        let dir = tempfile::tempdir().unwrap();
        assert!(check_space(dir.path(), 1024, 0).is_ok());
        match check_space(dir.path(), u64::MAX / 2, DEFAULT_RESERVE) {
            Err(CopyError::InsufficientSpace { needed, available, .. }) => {
                assert_eq!(needed, u64::MAX / 2);
                assert!(available < needed);
            }
            other => panic!("expected InsufficientSpace, got {other:?}"),
        }
    }

    #[test]
    fn reserve_counts_against_available() {
        let r = SpaceReport { probe: ".".into(), needed: 90, available: 100, reserve: 20 };
        assert!(!r.is_sufficient());
        assert_eq!(r.shortfall(), 10);
        let r = SpaceReport { reserve: 10, ..r };
        assert!(r.is_sufficient());
        assert_eq!(r.shortfall(), 0);
    }

    #[test]
    fn measure_tree_counts_and_rounds() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("a"), vec![0u8; 1]).unwrap();
        fs::write(dir.path().join("sub/b"), vec![0u8; 5000]).unwrap();
        let s = measure_tree(dir.path(), 4096).unwrap();
        assert_eq!(s.files, 2);
        assert_eq!(s.dirs, 2);
        assert_eq!(s.bytes, 5001);
        assert_eq!(s.allocated, 4096 + 8192);
    }

    #[cfg(unix)]
    #[test]
    fn measure_tree_does_not_follow_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("real"), vec![0u8; 100]).unwrap();
        std::os::unix::fs::symlink(dir.path(), dir.path().join("loop")).unwrap();
        let s = measure_tree(dir.path(), 1).unwrap();
        assert_eq!(s.files, 1);
        assert_eq!(s.skipped_special, 1);
    }
}

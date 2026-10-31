//! File conflicts are separate from transient I/O error recovery.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::{CopyError, JobControl};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictPolicy { Ask, Replace, Skip, KeepBoth, Compare }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictAction { Replace, Skip, KeepBoth, Compare, Cancel }

#[derive(Debug, Clone, Copy)]
pub struct ConflictChoice {
    pub action: ConflictAction,
    pub apply_to_all: bool,
}

#[derive(Debug, Clone)]
pub struct Conflict {
    pub source: PathBuf,
    pub destination: PathBuf,
    pub different: bool,
}

type Handler = dyn Fn(&Conflict, &JobControl) -> ConflictChoice + Send + Sync;

pub struct ConflictResolver {
    pub policy: ConflictPolicy,
    handler: Option<Arc<Handler>>,
    // A comparison that differs has a separate remembered answer.
    remembered: Mutex<[Option<ConflictAction>; 2]>,
}

impl std::fmt::Debug for ConflictResolver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConflictResolver").field("policy", &self.policy).finish_non_exhaustive()
    }
}

impl ConflictResolver {
    pub fn new(policy: ConflictPolicy) -> Self {
        Self { policy, handler: None, remembered: Mutex::new([None, None]) }
    }

    pub fn with_handler(mut self, handler: Arc<Handler>) -> Self {
        self.handler = Some(handler);
        self
    }

    pub fn choose(&self, source: &Path, destination: &Path, different: bool, control: &JobControl) -> Result<ConflictAction, CopyError> {
        control.checkpoint().map_err(|_| CopyError::Cancelled)?;
        if control.take_skip() { return Ok(ConflictAction::Skip); }
        let mut remembered = self.remembered.lock().unwrap_or_else(|e| e.into_inner());
        let index = usize::from(different);
        let action = remembered[index].or_else(|| {
            if different { return None; }
            match self.policy {
                ConflictPolicy::Ask => None,
                ConflictPolicy::Replace => Some(ConflictAction::Replace),
                ConflictPolicy::Skip => Some(ConflictAction::Skip),
                ConflictPolicy::KeepBoth => Some(ConflictAction::KeepBoth),
                ConflictPolicy::Compare => Some(ConflictAction::Compare),
            }
        });
        if let Some(action) = action { return Ok(action); }
        let handler = self.handler.as_ref().ok_or_else(|| CopyError::Backend(
            format!("{}: conflict needs an interactive decision; choose an explicit policy", destination.display())
        ))?;
        let choice = handler(&Conflict { source: source.into(), destination: destination.into(), different }, control);
        if control.is_cancelled() || choice.action == ConflictAction::Cancel {
            control.cancel();
            return Err(CopyError::Cancelled);
        }
        if control.take_skip() { return Ok(ConflictAction::Skip); }
        if different && choice.action == ConflictAction::Compare {
            return Err(CopyError::Backend("files already compared as different; choose replace, skip or keep both".into()));
        }
        if choice.apply_to_all && choice.action != ConflictAction::Cancel {
            remembered[index] = Some(choice.action);
        }
        Ok(choice.action)
    }
}

pub fn keep_both_path(path: &Path, number: u64) -> PathBuf {
    let mut name = path.file_stem().unwrap_or_default().to_os_string();
    name.push(format!(" (copy {number})"));
    if let Some(extension) = path.extension() {
        name.push(".");
        name.push(extension);
    }
    path.with_file_name(name)
}

pub fn exists(path: &Path) -> io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

pub fn open_comparison_file(path: &Path) -> io::Result<std::fs::File> {
    if !std::fs::metadata(path)?.is_file() {
        return Err(io::Error::other("only regular files can be compared"));
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // A FIFO swapped in after metadata must not block opening indefinitely.
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("only regular files can be compared"));
    }
    Ok(file)
}

pub fn files_equal(a: &Path, b: &Path, control: &JobControl) -> io::Result<bool> {
    let mut a = open_comparison_file(a)?;
    let mut b = open_comparison_file(b)?;
    let (a_before, b_before) = (a.metadata()?, b.metadata()?);
    if !a_before.is_file() || !b_before.is_file() {
        return Err(io::Error::other("only regular files can be compared"));
    }
    if a_before.len() != b_before.len() { return Ok(false); }
    let mut left = vec![0; 1024 * 1024];
    let mut right = vec![0; left.len()];
    let mut remaining = a_before.len();
    while remaining > 0 {
        if control.checkpoint().is_err() || control.skip_requested() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "comparison cancelled or skipped"));
        }
        let n = remaining.min(left.len() as u64) as usize;
        a.read_exact(&mut left[..n])?;
        b.read_exact(&mut right[..n])?;
        if left[..n] != right[..n] { return Ok(false); }
        remaining -= n as u64;
    }
    for (before, after) in [(a_before, a.metadata()?), (b_before, b.metadata()?)] {
        if before.len() != after.len() || before.modified()? != after.modified()? {
            return Err(io::Error::other("file changed while comparing"));
        }
    }
    Ok(true)
}

pub fn local_destination(
    source: &Path, destination: &Path, overwrite: bool,
    resolver: Option<&ConflictResolver>, control: &JobControl,
) -> Result<(PathBuf, bool), CopyError> {
    let io_error = |e| CopyError::io(crate::Operation::Finalize, destination, e);
    let overwrite = resolver.map_or(overwrite, |r| r.policy == ConflictPolicy::Replace);
    if !exists(destination).map_err(io_error)? { return Ok((destination.into(), overwrite)); }
    let mut overwrite = overwrite;
    if let Some(resolver) = resolver {
        let mut different = false;
        loop {
            match resolver.choose(source, destination, different, control)? {
                ConflictAction::Replace => { overwrite = true; break; }
                ConflictAction::Skip => return Err(CopyError::SkippedByUser),
                ConflictAction::Cancel => return Err(CopyError::Cancelled),
                ConflictAction::KeepBoth => {
                    for number in 1..u64::MAX {
                        control.checkpoint().map_err(|_| CopyError::Cancelled)?;
                        let candidate = keep_both_path(destination, number);
                        if !exists(&candidate).map_err(io_error)? { return Ok((candidate, false)); }
                    }
                    return Err(CopyError::Backend("keep-both suffixes exhausted".into()));
                }
                ConflictAction::Compare => {
                    match files_equal(source, destination, control) {
                        Ok(true) => return Err(CopyError::SkippedByUser),
                        Ok(false) => different = true,
                        Err(_) if control.is_cancelled() => return Err(CopyError::Cancelled),
                        Err(_) if control.take_skip() => return Err(CopyError::SkippedByUser),
                        Err(e) => return Err(io_error(e)),
                    }
                }
            }
        }
    }
    let same = matches!((std::fs::canonicalize(source), std::fs::canonicalize(destination)), (Ok(a), Ok(b)) if a == b);
    if !overwrite || same { return Err(CopyError::DestinationExists(destination.into())); }
    Ok((destination.into(), true))
}

/// Never replace a file which appeared after a non-overwriting copy began.
pub fn commit_file(temp: &Path, destination: &Path, overwrite: bool) -> io::Result<()> {
    if overwrite { return std::fs::rename(temp, destination); }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let temp: Vec<_> = crate::platform::longpath::to_extended(temp).as_os_str().encode_wide().chain(Some(0)).collect();
        let destination: Vec<_> = crate::platform::longpath::to_extended(destination).as_os_str().encode_wide().chain(Some(0)).collect();
        // SAFETY: NUL-terminated paths live through the call; no replacement flag is set.
        if unsafe { windows_sys::Win32::Storage::FileSystem::MoveFileExW(temp.as_ptr(), destination.as_ptr(), 0) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::os::unix::ffi::OsStrExt;
        let a = std::ffi::CString::new(temp.as_os_str().as_bytes())?;
        let b = std::ffi::CString::new(destination.as_os_str().as_bytes())?;
        // SAFETY: both C strings are valid through the call.
        #[cfg(target_os = "linux")]
        let result = unsafe { libc::renameat2(libc::AT_FDCWD, a.as_ptr(), libc::AT_FDCWD, b.as_ptr(), libc::RENAME_NOREPLACE) };
        #[cfg(target_os = "macos")]
        let result = unsafe { libc::renamex_np(a.as_ptr(), b.as_ptr(), libc::RENAME_EXCL) };
        if result == 0 { return Ok(()); }
        let error = io::Error::last_os_error();
        #[cfg(target_os = "linux")]
        if matches!(error.raw_os_error(), Some(libc::ENOSYS | libc::EINVAL | libc::EOPNOTSUPP)) {
            std::fs::hard_link(temp, destination)?;
            return std::fs::remove_file(temp);
        }
        Err(error)
    }
    #[cfg(not(any(windows, target_os = "linux", target_os = "macos")))]
    {
        std::fs::hard_link(temp, destination)?;
        std::fs::remove_file(temp)
    }
}

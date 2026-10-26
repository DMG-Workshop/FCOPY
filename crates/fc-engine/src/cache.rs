//! Best-effort page-cache control.
//!
//! These calls are *hints*: failure is ignored because the copy is still
//! correct without them. They matter for two reasons:
//!
//! * `advise_sequential` lets the kernel read ahead more aggressively.
//! * Read-back **verification only means something if it reads the media**, not
//!   the page cache that still holds the bytes we just wrote. `open_uncached`
//!   and `drop_cache` make the verify pass go to disk where the OS allows it.
//!
//! | OS      | Mechanism |
//! |---------|-----------|
//! | Linux   | `posix_fadvise(DONTNEED)` after `fsync` (only clean pages can be dropped) |
//! | macOS   | `fcntl(F_NOCACHE)` on the verify handle |
//! | Windows | `FILE_FLAG_NO_BUFFERING` on the verify handle (needs aligned buffers: ours are) |

use std::fs::File;
use std::io;
use std::path::Path;

/// Hint that `file` will be read front to back.
pub(crate) fn advise_sequential(file: &File) {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: plain syscall on a valid fd; the return value is only a hint result.
        unsafe {
            libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_SEQUENTIAL);
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = file;
}

/// Ask the kernel to evict `file`'s cached pages. Call *after* `sync_all`.
pub(crate) fn drop_cache(file: &File) {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: as above.
        unsafe {
            libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED);
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = file;
}

/// Open `path` for reading in a way that avoids (or at least discourages) the
/// OS cache, for the verification pass.
pub(crate) fn open_uncached(path: &Path) -> io::Result<File> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_NO_BUFFERING: u32 = 0x2000_0000;
        // Fall back to a normal open if the volume refuses unbuffered access.
        if let Ok(f) = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(FILE_FLAG_NO_BUFFERING)
            .open(path)
        {
            return Ok(f);
        }
    }

    let file = File::open(path)?;

    #[cfg(target_os = "macos")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: plain fcntl on a valid fd; failure is ignored (hint only).
        unsafe {
            libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1);
        }
    }

    drop_cache(&file);
    Ok(file)
}

/// Make the rename of a freshly written file durable by syncing its directory.
/// Unix only; elsewhere this is a no-op.
pub(crate) fn sync_parent_dir(path: &Path) {
    #[cfg(unix)]
    if let Some(parent) = path.parent() {
        let parent = if parent.as_os_str().is_empty() { Path::new(".") } else { parent };
        if let Ok(dir) = File::open(parent) {
            let _ = dir.sync_all();
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

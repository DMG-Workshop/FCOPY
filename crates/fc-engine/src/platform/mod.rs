//! OS abstraction layer.
//!
//! Rule of thumb for this module: **portable logic is written as pure
//! functions and tested on every OS; `#[cfg]` blocks are kept thin** and only
//! translate between those functions and a system call. That keeps the
//! untested-on-this-machine surface small.
//!
//! ```text
//! platform/
//! ├─ longpath.rs  Windows \\?\ extended-length paths (pure transform + cfg(windows) glue)
//! ├─ names.rs     names that are illegal on Windows/NTFS/exFAT, case-collision detection
//! └─ meta.rs      timestamps, permissions/attributes, xattrs, Windows DACLs
//! ```
//!
//! Conditional-compilation conventions used throughout:
//!
//! | Attribute | Meaning |
//! |-----------|---------|
//! | `#[cfg(unix)]` | Linux, macOS, BSDs: POSIX mode bits, xattrs |
//! | `#[cfg(windows)]` | Win32 attributes, security descriptors, `\\?\` |
//! | `#[cfg(target_os = "linux")]` | `posix_fadvise`, `fallocate` behaviour |
//! | `#[cfg(target_os = "macos")]` | `F_NOCACHE`, birth time, Finder metadata |
//! | `#[cfg(not(any(unix, windows)))]` | compile-only fallback (WASI etc.) |

pub mod longpath;
pub mod meta;
pub mod names;

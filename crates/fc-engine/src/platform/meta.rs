//! Metadata preservation: timestamps, permissions/attributes, xattrs, ACLs.
//!
//! **Policy: metadata never fails a copy.** Every function here is
//! best-effort. Errors become warnings in the `CopyReport`, because the
//! destination filesystem often *can't* store what the source has (no xattrs
//! on FAT32/exFAT, no POSIX modes on NTFS-from-Linux, no ACLs on SMB shares
//! without permission). The data is what matters; losses are reported.
//!
//! | What | Linux | macOS | Windows |
//! |------|-------|-------|---------|
//! | mtime / atime | `futimens` via `File::set_times` | same | `SetFileTime` via `File::set_times` |
//! | birth time | not settable | `FileTimesExt::set_created` | `FileTimesExt::set_created` |
//! | permissions | `rwx` bits (setuid/setgid/sticky dropped) | same | read-only flag |
//! | attributes | — | (flags like `uchg`: not copied) | hidden/system/archive/not-indexed |
//! | xattrs | `user.*`, `trusted.*`, `system.posix_acl_*` (= POSIX ACLs); `security.*` skipped | all, incl. `com.apple.*` (resource forks, Finder info, quarantine) | n/a (alternate data streams: not copied yet) |
//! | ACLs | via `system.posix_acl_*` xattrs | native ACLs are not xattrs: see note | DACL via `Get/SetNamedSecurityInfoW` (opt-in) |
//!
//! macOS note: native ACLs are *not* xattrs. The complete native answer on
//! macOS is `copyfile(3)` / `fcopyfile(src_fd, dst_fd, NULL, COPYFILE_METADATA)`,
//! which copies ACLs, flags, xattrs and times in one call. That is a good
//! macOS-only fast path to add behind this same interface.
//!
//! **Windows and macOS branches in this file are not compiled by this repo's
//! Linux CI.** Their Win32 signatures were checked against the `windows-sys`
//! 0.61 sources; build on those hosts before release.

use std::fs::{File, FileTimes, Metadata};
use std::io;
use std::path::Path;

/// Copy access/modification (and, where settable, creation) times.
pub(crate) fn apply_times(file: &File, src: &Metadata) -> io::Result<()> {
    let mut t = FileTimes::new();
    if let Ok(m) = src.modified() {
        t = t.set_modified(m);
    }
    if let Ok(a) = src.accessed() {
        t = t.set_accessed(a);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileTimesExt;
        if let Ok(c) = src.created() {
            t = t.set_created(c);
        }
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::macos::fs::FileTimesExt;
        if let Ok(c) = src.created() {
            t = t.set_created(c);
        }
    }
    file.set_times(t)
}

// ---------------------------------------------------------------------------
// Permissions / attributes
// ---------------------------------------------------------------------------

/// Unix: copy `rwx` bits. Setuid/setgid/sticky are deliberately dropped: a
/// copy should never gain privilege. (`cp -p` behaves the same unless run as root.)
#[cfg(unix)]
pub(crate) fn apply_permissions(file: &File, _path: &Path, src: &Metadata) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(std::fs::Permissions::from_mode(src.permissions().mode() & 0o777))
}

/// Windows: copy the user-visible attribute bits (read-only, hidden, system,
/// archive, not-content-indexed). Compression/encryption/sparse are properties
/// of how the data is stored and need dedicated `FSCTL`s, so they are not here.
#[cfg(windows)]
pub(crate) fn apply_permissions(_file: &File, path: &Path, src: &Metadata) -> io::Result<()> {
    use std::os::windows::fs::MetadataExt;
    use windows_sys::Win32::Storage::FileSystem::{
        SetFileAttributesW, FILE_ATTRIBUTE_ARCHIVE, FILE_ATTRIBUTE_HIDDEN, FILE_ATTRIBUTE_NOT_CONTENT_INDEXED,
        FILE_ATTRIBUTE_READONLY, FILE_ATTRIBUTE_SYSTEM,
    };
    const MASK: u32 = FILE_ATTRIBUTE_READONLY
        | FILE_ATTRIBUTE_HIDDEN
        | FILE_ATTRIBUTE_SYSTEM
        | FILE_ATTRIBUTE_ARCHIVE
        | FILE_ATTRIBUTE_NOT_CONTENT_INDEXED;
    let attrs = src.file_attributes() & MASK;
    let wide = win::wide(path);
    // SAFETY: `wide` is a NUL-terminated UTF-16 string that outlives the call.
    let ok = unsafe { SetFileAttributesW(wide.as_ptr(), attrs) };
    if ok == 0 { Err(io::Error::last_os_error()) } else { Ok(()) }
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn apply_permissions(file: &File, _path: &Path, src: &Metadata) -> io::Result<()> {
    file.set_permissions(src.permissions())
}

// ---------------------------------------------------------------------------
// Extended attributes
// ---------------------------------------------------------------------------

/// Should this xattr be left behind?
///
/// `security.*` holds SELinux/SMACK labels and file capabilities. The
/// destination's policy should assign its own label (as `cp` does without
/// `--preserve=context`), and setting capabilities needs `CAP_SETFCAP`.
#[cfg(unix)]
fn skip_xattr(name: &std::ffi::OsStr) -> bool {
    name.to_string_lossy().starts_with("security.")
}

#[cfg(unix)]
fn xattrs_unsupported(e: &io::Error) -> bool {
    matches!(e.raw_os_error(), Some(c) if c == libc::ENOTSUP || c == libc::EOPNOTSUPP)
}

/// Copy extended attributes from `src` to `dst`; returns warnings.
#[cfg(unix)]
pub(crate) fn copy_xattrs(src: &Path, dst: &Path) -> Vec<String> {
    let mut warnings = Vec::new();
    let names = match xattr::list(src) {
        Ok(n) => n,
        Err(e) if xattrs_unsupported(&e) => return warnings, // source FS has none
        Err(e) => return vec![format!("could not list extended attributes: {e}")],
    };
    for name in names {
        if skip_xattr(&name) {
            continue;
        }
        let value = match xattr::get(src, &name) {
            Ok(Some(v)) => v,
            Ok(None) => continue,
            Err(e) => {
                warnings.push(format!("could not read xattr {}: {e}", name.to_string_lossy()));
                continue;
            }
        };
        if let Err(e) = xattr::set(dst, &name, &value) {
            if xattrs_unsupported(&e) {
                warnings.push("destination filesystem does not support extended attributes; none copied".into());
                break;
            }
            warnings.push(format!("could not copy xattr {}: {e}", name.to_string_lossy()));
        }
    }
    warnings
}

/// Windows has no xattrs in the Unix sense. Alternate data streams
/// (`file.txt:Zone.Identifier` etc.) would be enumerated with
/// `FindFirstStreamW` and copied as extra streams. Not implemented yet.
#[cfg(not(unix))]
pub(crate) fn copy_xattrs(_src: &Path, _dst: &Path) -> Vec<String> {
    Vec::new()
}

// ---------------------------------------------------------------------------
// Windows DACL (opt-in; see CopyOptions::preserve_acls)
// ---------------------------------------------------------------------------

/// Copy the discretionary ACL (who may read/write), including whether it is
/// *protected* (doesn't inherit from the parent folder).
///
/// Owner and SACL (auditing) are not copied: setting another user as owner
/// needs `SeRestorePrivilege`, and the SACL needs `SeSecurityPrivilege`.
/// Inherited ACEs are re-derived from the destination folder automatically.
#[cfg(windows)]
pub fn copy_dacl(src: &Path, dst: &Path) -> io::Result<()> {
    use std::ptr::{null, null_mut};
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::Security::Authorization::{GetNamedSecurityInfoW, SetNamedSecurityInfoW, SE_FILE_OBJECT};
    use windows_sys::Win32::Security::{
        GetSecurityDescriptorControl, ACL, DACL_SECURITY_INFORMATION, PROTECTED_DACL_SECURITY_INFORMATION,
        PSECURITY_DESCRIPTOR, SE_DACL_PROTECTED, UNPROTECTED_DACL_SECURITY_INFORMATION,
    };

    let src_w = win::wide(src);
    let dst_w = win::wide(dst);
    let mut dacl: *mut ACL = null_mut();
    let mut sd: PSECURITY_DESCRIPTOR = null_mut();
    // SAFETY: out-pointers are valid; on success `sd` owns the buffer `dacl` points into
    // and must be released with LocalFree.
    let rc = unsafe {
        GetNamedSecurityInfoW(
            src_w.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION,
            null_mut(),
            null_mut(),
            &mut dacl,
            null_mut(),
            &mut sd,
        )
    };
    if rc != ERROR_SUCCESS {
        return Err(io::Error::from_raw_os_error(rc as i32));
    }
    let _free = win::LocalGuard(sd);

    let mut control: u16 = 0;
    let mut revision: u32 = 0;
    // SAFETY: `sd` is a valid security descriptor returned above.
    unsafe { GetSecurityDescriptorControl(sd, &mut control, &mut revision) };
    let protection = if control & SE_DACL_PROTECTED != 0 {
        PROTECTED_DACL_SECURITY_INFORMATION
    } else {
        UNPROTECTED_DACL_SECURITY_INFORMATION
    };

    // SAFETY: `dacl` stays valid while `sd` is alive (guarded by `_free`).
    let rc = unsafe {
        SetNamedSecurityInfoW(
            dst_w.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | protection,
            null_mut(),
            null_mut(),
            dacl as *const ACL,
            null(),
        )
    };
    if rc != ERROR_SUCCESS { Err(io::Error::from_raw_os_error(rc as i32)) } else { Ok(()) }
}

#[cfg(not(windows))]
pub fn copy_dacl(_src: &Path, _dst: &Path) -> io::Result<()> {
    Ok(()) // POSIX ACLs travel with the xattrs (system.posix_acl_access)
}

#[cfg(windows)]
mod win {
    use std::os::windows::ffi::OsStrExt;
    use std::path::Path;

    /// NUL-terminated UTF-16 for Win32 `W` functions, `\\?\`-extended when long.
    pub fn wide(path: &Path) -> Vec<u16> {
        let p = super::super::longpath::to_extended(path);
        p.as_os_str().encode_wide().chain(std::iter::once(0)).collect()
    }

    /// Frees a `LocalAlloc`'d buffer on drop.
    pub struct LocalGuard(pub *mut core::ffi::c_void);
    impl Drop for LocalGuard {
        fn drop(&mut self) {
            if !self.0.is_null() {
                // SAFETY: pointer was returned by a Win32 API documented to use LocalAlloc.
                unsafe { windows_sys::Win32::Foundation::LocalFree(self.0) };
            }
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::time::{Duration, SystemTime};

    #[test]
    fn times_are_copied() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"y").unwrap();
        let m = SystemTime::UNIX_EPOCH + Duration::from_secs(1_234_567_890);
        let at = SystemTime::UNIX_EPOCH + Duration::from_secs(1_300_000_000);
        File::options()
            .write(true)
            .open(&a)
            .unwrap()
            .set_times(FileTimes::new().set_modified(m).set_accessed(at))
            .unwrap();
        let src = std::fs::metadata(&a).unwrap();
        apply_times(&File::options().write(true).open(&b).unwrap(), &src).unwrap();
        let dst = std::fs::metadata(&b).unwrap();
        assert_eq!(dst.modified().unwrap(), m);
        assert_eq!(dst.accessed().unwrap(), at);
    }

    #[test]
    fn xattrs_are_copied_and_security_ones_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a"), dir.path().join("b"));
        std::fs::write(&a, b"x").unwrap();
        std::fs::write(&b, b"y").unwrap();
        if xattr::set(&a, "user.fc.test", b"hello").is_err() {
            eprintln!("filesystem without user xattrs; skipping");
            return;
        }
        let w = copy_xattrs(&a, &b);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(xattr::get(&b, "user.fc.test").unwrap().as_deref(), Some(&b"hello"[..]));
        assert!(skip_xattr(std::ffi::OsStr::new("security.selinux")));
        assert!(!skip_xattr(std::ffi::OsStr::new("system.posix_acl_access")));
    }
}

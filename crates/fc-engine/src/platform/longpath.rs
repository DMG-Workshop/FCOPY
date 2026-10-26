//! Windows long paths (> `MAX_PATH` = 260 UTF-16 units).
//!
//! # What Rust already does, and what it doesn't
//!
//! Since Rust 1.58, `std::fs` on Windows quietly converts long absolute paths
//! to the `\\?\` form before calling Win32, so `File::open`, `rename`,
//! `read_dir` and friends work beyond 260 characters **without** the system
//! "LongPathsEnabled" policy or an app manifest.
//!
//! You still need explicit handling when:
//!
//! * a path goes to an API that `std` doesn't wrap: `windows-sys` FFI (ACL
//!   copy, `CopyFileExW`, shell APIs), child processes, or other crates;
//! * a path is relative or contains `.`/`..`. `\\?\` paths are passed to the
//!   filesystem **verbatim**, with no normalisation, so they must be made
//!   absolute and cleaned first, which is what [`extend_str`] does;
//! * a UNC share is involved: `\\server\share\x` becomes `\\?\UNC\server\share\x`.
//!
//! Verbatim paths also skip Win32 name rules. A file called `"report. "`
//! (trailing dot and space) can be created through `\\?\` and then cannot be
//! opened or deleted from Explorer. Sanitize destination names with
//! [`super::names`] before they reach this layer.
//!
//! For display, strip the prefix again with [`strip_extended`].

use std::borrow::Cow;
use std::path::Path;
#[cfg(windows)]
use std::path::PathBuf;

/// Convert once the path gets near the limit. 248 is the limit for
/// directories (they need room for an 8.3 file name), so stay below it.
pub const EXTEND_THRESHOLD: usize = 240;

/// Return a form of `path` that Win32 accepts at any length.
///
/// * Windows: relative paths are made absolute; long ones get `\\?\`.
/// * Elsewhere: returned unchanged (borrowed, no allocation).
pub fn to_extended(path: &Path) -> Cow<'_, Path> {
    #[cfg(windows)]
    {
        let Some(s) = path.to_str() else {
            return Cow::Borrowed(path); // unpaired surrogates: leave to std's own handling
        };
        if s.len() < EXTEND_THRESHOLD {
            return Cow::Borrowed(path);
        }
        let abs = match std::path::absolute(path) {
            Ok(p) => p,
            Err(_) => return Cow::Borrowed(path),
        };
        match abs.to_str().and_then(extend_str) {
            Some(ext) => Cow::Owned(PathBuf::from(ext)),
            None => Cow::Owned(abs),
        }
    }
    #[cfg(not(windows))]
    {
        Cow::Borrowed(path)
    }
}

/// Pure transform from an **absolute** Windows path string to its `\\?\` form.
///
/// Returns `None` when the path is already extended or a device path
/// (`\\.\`), or when it isn't absolute (drive-relative `C:foo` and plain
/// relative paths must be resolved first).
///
/// ```
/// use fc_engine::platform::longpath::extend_str;
/// assert_eq!(extend_str(r"C:\a\.\b\..\c").as_deref(), Some(r"\\?\C:\a\c"));
/// assert_eq!(extend_str(r"\\nas\share\x").as_deref(), Some(r"\\?\UNC\nas\share\x"));
/// ```
pub fn extend_str(path: &str) -> Option<String> {
    if path.starts_with(r"\\?\") || path.starts_with(r"\\.\") {
        return None;
    }
    let p = path.replace('/', "\\");
    let (mut out, rest) = if let Some(unc) = p.strip_prefix(r"\\") {
        let mut parts = unc.splitn(3, '\\');
        let server = parts.next().filter(|s| !s.is_empty())?;
        let share = parts.next().filter(|s| !s.is_empty())?;
        (format!(r"\\?\UNC\{server}\{share}"), parts.next().unwrap_or("").to_string())
    } else {
        let b = p.as_bytes();
        if b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'\\' {
            // Bytes 0..2 are ASCII, so slicing at 2/3 is on char boundaries.
            (format!(r"\\?\{}", p[..2].to_ascii_uppercase()), p[3..].to_string())
        } else {
            return None;
        }
    };

    // Normalise: verbatim paths are NOT normalised by the OS.
    let mut parts: Vec<&str> = Vec::new();
    for c in rest.split('\\') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    if parts.is_empty() && !out.contains("UNC") {
        out.push('\\'); // drive root: "\\?\C:\"
    }
    for c in parts {
        out.push('\\');
        out.push_str(c);
    }
    Some(out)
}

/// `\\?\C:\x` → `C:\x`, `\\?\UNC\srv\sh` → `\\srv\sh`, others unchanged. For display only.
pub fn strip_extended(path: &str) -> Cow<'_, str> {
    if let Some(rest) = path.strip_prefix(r"\\?\UNC\") {
        Cow::Owned(format!(r"\\{rest}"))
    } else if let Some(rest) = path.strip_prefix(r"\\?\") {
        Cow::Borrowed(rest)
    } else {
        Cow::Borrowed(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_paths() {
        assert_eq!(extend_str(r"c:\Users\me\file.txt").as_deref(), Some(r"\\?\C:\Users\me\file.txt"));
        assert_eq!(extend_str("C:/mixed/slashes//x").as_deref(), Some(r"\\?\C:\mixed\slashes\x"));
        assert_eq!(extend_str(r"C:\a\b\..\..\..\c").as_deref(), Some(r"\\?\C:\c"), ".. never climbs above root");
        assert_eq!(extend_str(r"C:\").as_deref(), Some(r"\\?\C:\"));
    }

    #[test]
    fn unc_paths() {
        assert_eq!(extend_str(r"\\nas\media\film.mkv").as_deref(), Some(r"\\?\UNC\nas\media\film.mkv"));
        assert_eq!(extend_str(r"\\nas\media").as_deref(), Some(r"\\?\UNC\nas\media"));
        assert_eq!(extend_str(r"\\nas"), None, "server without share is not a path");
    }

    #[test]
    fn already_extended_or_not_absolute() {
        assert_eq!(extend_str(r"\\?\C:\x"), None);
        assert_eq!(extend_str(r"\\.\PhysicalDrive0"), None);
        assert_eq!(extend_str(r"C:relative"), None);
        assert_eq!(extend_str(r"relative\x"), None);
    }

    #[test]
    fn long_paths_keep_every_component() {
        let deep = format!(r"D:\{}", vec!["a-fairly-long-folder-name"; 20].join("\\"));
        let ext = extend_str(&deep).unwrap();
        assert!(ext.len() > 260);
        assert_eq!(ext.matches('\\').count(), deep.matches('\\').count() + 3);
    }

    #[test]
    fn strip_round_trip() {
        assert_eq!(strip_extended(r"\\?\C:\x"), r"C:\x");
        assert_eq!(strip_extended(r"\\?\UNC\nas\s\x"), r"\\nas\s\x");
        assert_eq!(strip_extended("/home/x"), "/home/x");
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_is_a_no_op() {
        let p = Path::new("relative/../path");
        assert!(matches!(to_extended(p), Cow::Borrowed(_)));
    }
}

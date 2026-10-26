//! File names that are fine on ext4/APFS but break on Windows, NTFS or exFAT.
//!
//! Copying from Linux to a Windows PC or a USB stick is where TeraCopy-class
//! tools hit these:
//!
//! * illegal characters `< > : " / \ | ? *` and control characters;
//! * reserved device names (`CON`, `NUL`, `COM1`, … even with an extension: `nul.txt`);
//! * trailing dots or spaces, which Win32 silently strips (two different
//!   Linux files can collide) and `\\?\` preserves (the file then can't be
//!   deleted from Explorer);
//! * case-only differences (`README` vs `readme`), which collide on
//!   case-insensitive volumes (NTFS by default, APFS by default, exFAT, FAT32).
//!
//! The planner uses these functions when its `windows_safe_names` option is
//! set, which is the default when running on Windows.

use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::OsStr;

const RESERVED: [&str; 4] = ["CON", "PRN", "AUX", "NUL"];

/// Is `name` a reserved DOS device name (with or without extension)?
pub fn is_reserved_windows_name(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or("").trim_end_matches(' ');
    let upper = stem.to_ascii_uppercase();
    if RESERVED.contains(&upper.as_str()) {
        return true;
    }
    // COM0-9 / LPT0-9, plus the superscript digits Windows also reserves.
    if (upper.starts_with("COM") || upper.starts_with("LPT")) && upper.chars().count() == 4 {
        let last = upper.chars().last().unwrap();
        return last.is_ascii_digit() || matches!(last, '¹' | '²' | '³');
    }
    false
}

/// Make one path component safe for Windows filesystems. Returns `Borrowed`
/// when nothing had to change.
///
/// * illegal or control characters → `_`
/// * trailing dots/spaces → `_`
/// * reserved names → `_` appended to the stem (`CON.txt` → `CON_.txt`)
pub fn sanitize_for_windows(name: &str) -> Cow<'_, str> {
    let illegal = |c: char| matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || (c as u32) < 32;
    let needs_char_fix = name.chars().any(illegal);
    let trailing = name.ends_with('.') || name.ends_with(' ');
    let reserved = is_reserved_windows_name(name);
    if !needs_char_fix && !trailing && !reserved {
        return Cow::Borrowed(name);
    }

    let mut s: String = name.chars().map(|c| if illegal(c) { '_' } else { c }).collect();
    if trailing {
        let kept = s.trim_end_matches(['.', ' ']).len();
        let removed = s.len() - kept;
        s.truncate(kept);
        s.extend(std::iter::repeat_n('_', removed));
    }
    if is_reserved_windows_name(&s) {
        match s.find('.') {
            Some(i) => s.insert(i, '_'),
            None => s.push('_'),
        }
    }
    Cow::Owned(s)
}

/// Sanitize an `OsStr` name. Non-UTF-8 names are rendered lossily first:
/// they can't be represented on Windows anyway.
pub fn sanitize_os_for_windows(name: &OsStr) -> Option<String> {
    let lossy = name.to_string_lossy();
    let fixed = sanitize_for_windows(&lossy);
    let changed = matches!(lossy, Cow::Owned(_)) || matches!(fixed, Cow::Owned(_));
    changed.then(|| fixed.into_owned())
}

/// Find names in one directory that collide on a case-insensitive volume.
/// Returns groups of colliding names (each group has ≥ 2 entries).
pub fn case_collisions<'a>(names: impl IntoIterator<Item = &'a str>) -> Vec<Vec<&'a str>> {
    let mut groups: HashMap<String, Vec<&'a str>> = HashMap::new();
    for n in names {
        groups.entry(n.to_lowercase()).or_default().push(n);
    }
    let mut out: Vec<_> = groups.into_values().filter(|g| g.len() > 1).collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_names() {
        for n in ["CON", "con", "Nul.txt", "COM1", "lpt9.tar.gz", "AUX ", "COM¹"] {
            assert!(is_reserved_windows_name(n), "{n}");
        }
        for n in ["CONSOLE", "COM10", "nullable.txt", "com", "my-con.txt"] {
            assert!(!is_reserved_windows_name(n), "{n}");
        }
    }

    #[test]
    fn sanitize() {
        assert_eq!(sanitize_for_windows("ok-name.txt"), Cow::Borrowed("ok-name.txt"));
        assert_eq!(sanitize_for_windows("a:b?c*.txt"), "a_b_c_.txt");
        assert_eq!(sanitize_for_windows("report. "), "report__");
        assert_eq!(sanitize_for_windows("CON.txt"), "CON_.txt");
        assert_eq!(sanitize_for_windows("nul"), "nul_");
        assert_eq!(sanitize_for_windows("tab\there"), "tab_here");
    }

    #[test]
    fn collisions() {
        let g = case_collisions(["README", "readme", "Readme", "other", "x", "X"]);
        assert_eq!(g, vec![vec!["README", "readme", "Readme"], vec!["x", "X"]]);
        assert!(case_collisions(["a", "b"]).is_empty());
    }
}

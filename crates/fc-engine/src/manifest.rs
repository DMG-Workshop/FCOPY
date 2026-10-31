//! Checksum lists in the `sha256sum` style: one `<hex>  <path>` line per file,
//! with paths relative to the list's own folder, so `fcopy check list` (or
//! `sha256sum -c` for SHA-256 lists) verifies a copy later or on another machine.

use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

use crate::checksum::Algorithm;

/// One copied file: where it landed, and its digest in lower-case hex.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    pub path: PathBuf,
    pub digest: String,
}

/// The file extension that names a list's algorithm, e.g. `sha256`.
pub fn extension(algorithm: Algorithm) -> &'static str {
    match algorithm {
        Algorithm::Xxh3 => "xxh3",
        Algorithm::Sha256 => "sha256",
        Algorithm::Md5 => "md5",
    }
}

/// The list's lines, with each path made relative to `root` where it lies
/// inside it.
pub fn lines(entries: &[ManifestEntry], root: &Path) -> Vec<String> {
    entries
        .iter()
        .map(|e| {
            let rel = e.path.strip_prefix(root).unwrap_or(&e.path);
            format!("{}  {}", e.digest, rel.display())
        })
        .collect()
}

/// Write `lines` to `path`, one per line.
pub fn write(path: &Path, lines: &[String]) -> io::Result<()> {
    let mut w = BufWriter::new(File::create(path)?);
    for l in lines {
        writeln!(w, "{l}")?;
    }
    w.flush()
}

/// A name in `dir` for a new list, `<stem>.<ext>`, that does not overwrite an
/// existing file: `<stem> (2).<ext>`, `<stem> (3).<ext>`, and so on.
pub fn free_name(dir: &Path, stem: &str, ext: &str) -> PathBuf {
    let stem = safe_stem(stem);
    let first = dir.join(format!("{stem}.{ext}"));
    if !first.exists() {
        return first;
    }
    (2..)
        .map(|n| dir.join(format!("{stem} ({n}).{ext}")))
        .find(|p| !p.exists())
        .expect("an unused name exists")
}

/// A job label made safe as a file name on every platform.
pub fn safe_stem(stem: &str) -> String {
    let cleaned: String = stem
        .chars()
        .map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { '_' } else { c })
        .collect();
    let trimmed = cleaned.trim().trim_end_matches('.');
    if trimmed.is_empty() { "checksums".into() } else { trimmed.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_relative_to_the_list() {
        let root = Path::new("/data/out");
        let entries = vec![
            ManifestEntry { path: root.join("a.bin"), digest: "aa".into() },
            ManifestEntry { path: root.join("sub").join("b.bin"), digest: "bb".into() },
        ];
        let l = lines(&entries, root);
        assert_eq!(l[0], "aa  a.bin");
        assert_eq!(l[1], format!("bb  {}", Path::new("sub").join("b.bin").display()));
    }

    #[test]
    fn names_never_overwrite_and_are_safe() {
        let dir = tempfile::tempdir().unwrap();
        let first = free_name(dir.path(), "photos: 2026/10", "xxh3");
        assert_eq!(first.file_name().unwrap(), "photos_ 2026_10.xxh3");
        std::fs::write(&first, b"x").unwrap();
        let second = free_name(dir.path(), "photos: 2026/10", "xxh3");
        assert_eq!(second.file_name().unwrap(), "photos_ 2026_10 (2).xxh3");
        assert_eq!(free_name(dir.path(), " ... ", "md5").file_name().unwrap(), "checksums.md5");
    }
}

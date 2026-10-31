//! What a small copy with default options must leave behind: the same bytes,
//! the source's timestamps (creation time too, on Windows), the read-only flag,
//! no temp file, and an existing destination untouched. Any faster path for
//! small files has to keep these passing.

use std::fs::{self, File, FileTimes};
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use fc_engine::*;

fn copier() -> Copier {
    Copier::new(CopyOptions::default(), Arc::new(AutoHandler::new(RetryPolicy::default())))
}

fn copy(src: &std::path::Path, dst: &std::path::Path) -> CopyReport {
    match copier().copy_file(src, dst, &Progress::new(), &JobControl::new()).unwrap() {
        CopyOutcome::Copied(r) => r,
        CopyOutcome::Skipped { error, .. } => panic!("skipped: {error}"),
    }
}

/// Within the coarsest timestamp resolution in common use (FAT keeps 2 s).
fn close(a: SystemTime, b: SystemTime) -> bool {
    let d = a.duration_since(b).unwrap_or_else(|e| e.duration());
    d < Duration::from_secs(2)
}

#[test]
fn a_small_copy_keeps_its_contents_and_timestamps() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("note.txt");
    let dst = dir.path().join("copy.txt");
    fs::write(&src, b"small but important").unwrap();
    let month_ago = SystemTime::now() - Duration::from_secs(30 * 24 * 3600);
    let times = FileTimes::new().set_modified(month_ago);
    #[cfg(windows)]
    let times = std::os::windows::fs::FileTimesExt::set_created(times, month_ago);
    File::options().write(true).open(&src).unwrap().set_times(times).unwrap();

    let report = copy(&src, &dst);

    assert_eq!(fs::read(&dst).unwrap(), b"small but important");
    assert_eq!(report.bytes, 19);
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    let (s, d) = (fs::metadata(&src).unwrap(), fs::metadata(&dst).unwrap());
    assert!(close(s.modified().unwrap(), d.modified().unwrap()), "modified time kept");
    #[cfg(windows)]
    assert!(close(s.created().unwrap(), d.created().unwrap()), "creation time kept, as the normal path does");
    let leftovers: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(TEMP_SUFFIX))
        .collect();
    assert!(leftovers.is_empty(), "no temp file is left behind");
}

#[test]
fn a_read_only_small_file_stays_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("locked.txt");
    let dst = dir.path().join("locked-copy.txt");
    fs::write(&src, b"do not edit").unwrap();
    let mut perms = fs::metadata(&src).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(&src, perms).unwrap();

    copy(&src, &dst);

    assert_eq!(fs::read(&dst).unwrap(), b"do not edit");
    assert!(fs::metadata(&dst).unwrap().permissions().readonly());

    // Let the temp dir clean up on Windows, where read-only files resist deletion.
    for p in [&src, &dst] {
        let mut perms = fs::metadata(p).unwrap().permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perms.set_readonly(false);
        fs::set_permissions(p, perms).unwrap();
    }
}

#[test]
fn an_existing_destination_is_still_refused() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("new.txt");
    let dst = dir.path().join("old.txt");
    fs::write(&src, b"new").unwrap();
    fs::write(&dst, b"old").unwrap();
    let err = copier().copy_file(&src, &dst, &Progress::new(), &JobControl::new()).unwrap_err();
    assert!(matches!(err, CopyError::DestinationExists(_)), "{err}");
    assert_eq!(fs::read(&dst).unwrap(), b"old");
}

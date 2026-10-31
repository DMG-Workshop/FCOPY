//! Job level: turn "copy these sources into that folder" into a [`Plan`], check
//! the *whole* plan fits before touching the destination, then run it.
//!
//! Files are processed one at a time; each file is already a two-stage
//! pipeline (read ‖ write). Running several files concurrently needs per-worker
//! progress slots and a storage-aware planner (one stream for HDD, several for
//! NVMe), which belongs in the scheduler layer above this crate.

use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::control::JobControl;
use crate::copy::{Copier, CopyOutcome, CopyReport, VerifyMode};
use crate::error::{CopyError, Result};
use crate::platform::names;
use crate::progress::Progress;
use crate::space::{self, round_up, TreeStats, DEFAULT_BLOCK_SIZE};

#[derive(Debug, Clone)]
pub struct CopyItem {
    pub src: PathBuf,
    pub dst: PathBuf,
    pub size: u64,
}

/// Planner switches.
#[derive(Debug, Clone)]
pub struct PlanOptions {
    /// Rewrite names that are illegal on Windows/NTFS/exFAT (`a:b`, `CON`,
    /// trailing dots). Default: on when running on Windows.
    pub windows_safe_names: bool,
    /// Warn about names that differ only in case (they collide on NTFS, APFS
    /// and exFAT). Default: on for Windows and macOS.
    pub warn_case_collisions: bool,
}

// Not derivable: the defaults differ per OS (cfg! is only `false` on Linux).
#[allow(clippy::derivable_impls)]
impl Default for PlanOptions {
    fn default() -> Self {
        Self {
            windows_safe_names: cfg!(windows),
            warn_case_collisions: cfg!(any(windows, target_os = "macos")),
        }
    }
}

/// Everything a job will do, computed up front.
#[derive(Debug, Default)]
pub struct Plan {
    pub items: Vec<CopyItem>,
    /// Directories to create (including empty ones, which have no file to create them).
    pub dirs: Vec<PathBuf>,
    pub stats: TreeStats,
    /// Things that were left out or renamed (symlinks, unreadable folders, illegal names).
    pub warnings: Vec<String>,
    /// Directory the free-space check probes.
    pub dest_root: PathBuf,
    /// Source folders walked, parents before children. A move removes the ones
    /// left empty, deepest first.
    pub source_dirs: Vec<PathBuf>,
}

/// Plan copying each of `sources` *into* `dest_dir` (`cp -r src dest_dir/`
/// semantics: `a/b` → `dest_dir/b`). Symlinks and special files are not
/// followed and are listed in [`Plan::warnings`].
pub fn plan_copy(sources: &[PathBuf], dest_dir: &Path) -> io::Result<Plan> {
    plan_copy_with(sources, dest_dir, &PlanOptions::default())
}

pub fn plan_copy_with(sources: &[PathBuf], dest_dir: &Path, opts: &PlanOptions) -> io::Result<Plan> {
    let mut plan = Plan { dest_root: dest_dir.to_path_buf(), ..Default::default() };
    let mut top_names = Vec::new();
    for src in sources {
        let name = src.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, format!("cannot copy '{}': no file name", src.display()))
        })?;
        let name = target_name(&mut plan, opts, src, name.to_os_string());
        top_names.push(name.clone());
        let target = dest_dir.join(&name);
        let meta = fs::symlink_metadata(src).map_err(|e| annotate(e, src))?;
        if meta.is_file() {
            add_file(&mut plan, src.clone(), target, meta.len());
        } else if meta.is_dir() {
            walk(&mut plan, opts, src, &target);
        } else {
            plan.warnings.push(format!("skipped (not a regular file or folder): {}", src.display()));
        }
    }
    if opts.warn_case_collisions {
        warn_collisions(&mut plan, dest_dir, &top_names);
    }
    Ok(plan)
}

/// Plan copying one file to an explicit destination *path* (`cp a.iso b.iso`).
pub fn plan_file_to(src: &Path, dst: &Path) -> io::Result<Plan> {
    let meta = fs::metadata(src).map_err(|e| annotate(e, src))?;
    if !meta.is_file() {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("{} is not a file", src.display())));
    }
    let parent = dst.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let mut plan = Plan { dest_root: parent.to_path_buf(), ..Default::default() };
    add_file(&mut plan, src.to_path_buf(), dst.to_path_buf(), meta.len());
    Ok(plan)
}

fn annotate(e: io::Error, p: &Path) -> io::Error {
    io::Error::new(e.kind(), format!("{}: {e}", p.display()))
}

fn target_name(plan: &mut Plan, opts: &PlanOptions, src: &Path, name: OsString) -> OsString {
    if !opts.windows_safe_names {
        return name;
    }
    match names::sanitize_os_for_windows(&name) {
        Some(fixed) => {
            plan.warnings.push(format!("renamed for Windows compatibility: {} → {fixed}", src.display()));
            fixed.into()
        }
        None => name,
    }
}

fn warn_collisions(plan: &mut Plan, dir: &Path, names_in_dir: &[OsString]) {
    let lossy: Vec<String> = names_in_dir.iter().map(|n| n.to_string_lossy().into_owned()).collect();
    for group in names::case_collisions(lossy.iter().map(String::as_str)) {
        plan.warnings.push(format!(
            "names differ only in case and will collide on case-insensitive volumes in {}: {}",
            dir.display(),
            group.join(", ")
        ));
    }
}

fn add_file(plan: &mut Plan, src: PathBuf, dst: PathBuf, size: u64) {
    plan.stats.files += 1;
    plan.stats.bytes = plan.stats.bytes.saturating_add(size);
    plan.stats.allocated = plan.stats.allocated.saturating_add(round_up(size, DEFAULT_BLOCK_SIZE));
    plan.items.push(CopyItem { src, dst, size });
}

fn walk(plan: &mut Plan, opts: &PlanOptions, src_root: &Path, dst_root: &Path) {
    let mut stack = vec![(src_root.to_path_buf(), dst_root.to_path_buf())];
    while let Some((src_dir, dst_dir)) = stack.pop() {
        plan.stats.dirs += 1;
        plan.dirs.push(dst_dir.clone());
        plan.source_dirs.push(src_dir.clone());
        let mut entries: Vec<_> = match fs::read_dir(&src_dir) {
            Ok(rd) => rd.flatten().collect(),
            Err(e) => {
                plan.stats.unreadable += 1;
                plan.warnings.push(format!("cannot read folder {}: {e}", src_dir.display()));
                continue;
            }
        };
        entries.sort_by_key(|e| e.file_name()); // deterministic order
        let mut subdirs = Vec::new();
        let mut targets = Vec::with_capacity(entries.len());
        for entry in entries {
            let path = entry.path();
            let name = target_name(plan, opts, &path, entry.file_name());
            let target = dst_dir.join(&name);
            targets.push(name);
            match entry.metadata() {
                Ok(m) if m.is_file() => add_file(plan, path, target, m.len()),
                Ok(m) if m.is_dir() => subdirs.push((path, target)),
                Ok(_) => {
                    plan.stats.skipped_special += 1;
                    plan.warnings.push(format!("skipped symlink/special file: {}", path.display()));
                }
                Err(e) => {
                    plan.stats.unreadable += 1;
                    plan.warnings.push(format!("cannot stat {}: {e}", path.display()));
                }
            }
        }
        if opts.warn_case_collisions {
            warn_collisions(plan, &dst_dir, &targets);
        }
        // Push in reverse so they pop in name order.
        stack.extend(subdirs.into_iter().rev());
    }
}

/// Per-file notification from [`Copier::run_plan_observed`]; feeds live logs.
#[derive(Debug)]
pub enum FileEvent<'a> {
    Copied(&'a CopyReport),
    Skipped(&'a CopyItem, &'a CopyError),
    Failed(&'a CopyItem, &'a CopyError),
}

/// Result of running a [`Plan`].
#[derive(Debug, Default)]
pub struct BatchReport {
    pub copied: Vec<CopyReport>,
    /// Files the handler (or the user's Skip button) chose to skip.
    pub skipped: Vec<(CopyItem, CopyError)>,
    /// Files that hit a conflict needing a decision (e.g. destination exists).
    pub failed: Vec<(CopyItem, CopyError)>,
    pub warnings: Vec<String>,
    /// True if the job stopped early (user cancel, or a handler chose Cancel).
    pub cancelled: bool,
    pub bytes_copied: u64,
    pub elapsed: Duration,
    /// On a move, sources deleted after their copy was committed.
    pub sources_removed: u64,
}

impl BatchReport {
    /// Everything was copied (and verified, if requested).
    pub fn is_clean(&self) -> bool {
        !self.cancelled && self.skipped.is_empty() && self.failed.is_empty()
    }

    pub fn throughput_bps(&self) -> f64 {
        self.bytes_copied as f64 / self.elapsed.as_secs_f64().max(1e-9)
    }
}

impl Copier {
    /// Run a plan into `dest_root`.
    ///
    /// Returns `Err(InsufficientSpace)` *before writing anything* if the whole
    /// plan won't fit. Per-file problems are reported in the [`BatchReport`],
    /// not as `Err`.
    pub fn run_plan(
        &self,
        plan: &Plan,
        dest_root: &Path,
        progress: &Progress,
        control: &JobControl,
    ) -> Result<BatchReport> {
        self.run_plan_observed(plan, dest_root, progress, control, &mut |_| {})
    }

    /// [`run_plan`](Self::run_plan), calling `on_file` after each file finishes.
    pub fn run_plan_observed(
        &self,
        plan: &Plan,
        dest_root: &Path,
        progress: &Progress,
        control: &JobControl,
        on_file: &mut dyn FnMut(FileEvent<'_>),
    ) -> Result<BatchReport> {
        let started = Instant::now();
        let opts = self.options();

        if opts.check_space {
            space::check_space(dest_root, plan.stats.allocated, opts.space_reserve)?;
        }
        progress.set_job_totals(plan.stats.files, plan.stats.bytes, opts.verify == VerifyMode::ReadBack);

        let mut report = BatchReport { warnings: plan.warnings.clone(), ..Default::default() };
        for dir in &plan.dirs {
            if let Err(e) = fs::create_dir_all(dir) {
                report.warnings.push(format!("cannot create folder {}: {e}", dir.display()));
            }
        }

        for item in &plan.items {
            if control.is_cancelled() {
                report.cancelled = true;
                break;
            }
            // Space was verified for the whole plan, so skip the per-file probe.
            match self.copy_file_impl(&item.src, &item.dst, progress, control, false, Some(item.size)) {
                Ok(CopyOutcome::Copied(mut r)) => {
                    if opts.move_source {
                        match remove_moved_source(&r) {
                            Ok(()) => report.sources_removed += 1,
                            Err(why) => r.warnings.push(why),
                        }
                    }
                    on_file(FileEvent::Copied(&r));
                    report.bytes_copied += r.bytes;
                    report.copied.push(r);
                }
                Ok(CopyOutcome::Skipped { error, .. }) => {
                    on_file(FileEvent::Skipped(item, &error));
                    report.skipped.push((item.clone(), error));
                }
                Err(CopyError::Cancelled) => {
                    // Make a handler-initiated cancel visible to everyone sharing `control`.
                    control.cancel();
                    report.cancelled = true;
                    break;
                }
                Err(e) => {
                    on_file(FileEvent::Failed(item, &e));
                    report.failed.push((item.clone(), e));
                }
            }
        }
        if opts.move_source && !report.cancelled {
            remove_empty_source_dirs(plan);
        }
        report.elapsed = started.elapsed();
        Ok(report)
    }
}

/// After a move, delete the source of one committed copy. Refuses, and keeps the
/// source, unless both files exist with the copied size and are not the same
/// file. Returns the reason as a warning for the file's log line.
pub fn remove_moved_source(report: &CopyReport) -> std::result::Result<(), String> {
    let dst_len = match fs::metadata(&report.dst) {
        Ok(m) => m.len(),
        Err(e) => return keep(format!("cannot read the copy: {e}")),
    };
    if dst_len != report.bytes {
        return keep("the copy has a different size".into());
    }
    if same_file(&report.src, &report.dst) {
        return keep("the copy is the same file".into());
    }
    delete_source(report)
}

/// After a move to another PC, delete the source of one file the other side
/// has confirmed. `dst` names a path over there, so only the source is checked
/// here; the remote side has already compared digests.
pub fn remove_sent_source(report: &CopyReport) -> std::result::Result<(), String> {
    delete_source(report)
}

fn keep(why: String) -> std::result::Result<(), String> {
    Err(format!("copied, but the source was kept: {why}"))
}

/// Delete `report.src` if it still has the size that was copied.
fn delete_source(report: &CopyReport) -> std::result::Result<(), String> {
    match fs::metadata(&report.src) {
        Ok(m) if m.len() == report.bytes => {}
        Ok(_) => return keep("it changed size during the copy".into()),
        Err(e) => return keep(format!("cannot read it: {e}")),
    }
    match fs::remove_file(&report.src) {
        Ok(()) => Ok(()),
        // Windows refuses to delete read-only files; the copy already carries the
        // flag. On Unix the file's own bits do not govern deletion, so no retry.
        Err(e) if cfg!(windows) && e.kind() == io::ErrorKind::PermissionDenied => {
            let cleared = fs::metadata(&report.src).map(|m| m.permissions()).and_then(|mut p| {
                #[allow(clippy::permissions_set_readonly_false)]
                p.set_readonly(false);
                fs::set_permissions(&report.src, p)
            });
            match cleared.and_then(|()| fs::remove_file(&report.src)) {
                Ok(()) => Ok(()),
                Err(_) => keep(format!("cannot delete it: {e}")),
            }
        }
        Err(e) => keep(format!("cannot delete it: {e}")),
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// After a move, remove the walked source folders that are now empty, deepest
/// first. Folders that still hold anything (skipped files, links) stay.
pub fn remove_empty_source_dirs(plan: &Plan) {
    for dir in plan.source_dirs.iter().rev() {
        let empty = fs::read_dir(dir).map(|mut d| d.next().is_none()).unwrap_or(false);
        if empty {
            // Losing a race with something writing into it just leaves the folder.
            let _ = fs::remove_dir(dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // The source tree needs names Windows itself cannot create (`a:b.txt`, `CON`,
    // and two names differing only in case).
    #[cfg(unix)]
    #[test]
    fn windows_safe_names_are_applied_and_reported() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir(&src).unwrap();
        fs::write(src.join("a:b.txt"), b"1").unwrap();
        fs::write(src.join("CON"), b"2").unwrap();
        fs::write(src.join("README"), b"3").unwrap();
        fs::write(src.join("readme"), b"4").unwrap();
        let case_sensitive = fs::read(src.join("README")).unwrap() == b"3";
        let opts = PlanOptions { windows_safe_names: true, warn_case_collisions: true };
        let plan = plan_copy_with(&[src], &dir.path().join("dst"), &opts).unwrap();
        let names: Vec<_> = plan.items.iter().map(|i| i.dst.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert!(names.contains(&"a_b.txt".to_string()), "{names:?}");
        assert!(names.contains(&"CON_".to_string()), "{names:?}");
        assert_eq!(plan.warnings.iter().filter(|w| w.starts_with("renamed")).count(), 2);
        assert_eq!(plan.warnings.iter().any(|w| w.contains("README, readme")), case_sensitive, "{:?}", plan.warnings);
    }

    #[test]
    fn plan_file_to_targets_an_explicit_path() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("a.iso");
        fs::write(&src, b"iso").unwrap();
        let plan = plan_file_to(&src, &dir.path().join("out/b.iso")).unwrap();
        assert_eq!(plan.items.len(), 1);
        assert!(plan.items[0].dst.ends_with("out/b.iso"));
        assert_eq!(plan.dest_root, dir.path().join("out"));
    }
}

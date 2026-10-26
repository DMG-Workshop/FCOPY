//! `fcopy`: command-line front end.
//!
//! ```text
//! fcopy copy [OPTIONS] <SOURCE>... <DEST>
//! fcopy hash [--algo A] <FILE>...
//! fcopy check [--algo A] <MANIFEST>
//! ```
//!
//! Examples:
//!
//! ```text
//! fcopy copy ~/Videos /mnt/backup                         # folder into folder
//! fcopy copy a.iso b.iso /media/usb --verify readback    # verify against the disk
//! fcopy copy big.iso /nas/iso/big-copy.iso --limit 20MB  # rename + 20 MB/s cap
//! fcopy copy photos/ /mnt/nas --on-error skip --manifest photos.sha256 --algo sha256
//! fcopy check photos.sha256 --algo sha256                # re-verify later
//! ```
//!
//! Exit codes: 0 everything copied · 1 some files skipped/failed or mismatched ·
//! 2 usage / planning error · 130 cancelled (Ctrl-C).

use std::collections::HashMap;
use std::fs::File;
use std::io::{self, BufRead, BufWriter, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use fc_engine::units::{human_bytes, human_duration, human_rate, parse_size};
use fc_engine::*;

#[derive(Parser)]
#[command(name = "fcopy", version, about = "Fast, verified file copying", propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Copy files and folders.
    #[command(override_usage = "fcopy copy [OPTIONS] <SOURCE>... <DEST>")]
    Copy(CopyArgs),
    /// Print checksums in `sha256sum`-compatible format.
    Hash {
        #[arg(long, value_enum, default_value_t = Algo::Sha256)]
        algo: Algo,
        #[arg(required = true)]
        files: Vec<PathBuf>,
    },
    /// Verify files against a checksum manifest (`<hex>  <path>` lines).
    Check {
        #[arg(long, value_enum, default_value_t = Algo::Sha256)]
        algo: Algo,
        manifest: PathBuf,
    },
}

#[derive(Args)]
struct CopyArgs {
    /// Sources followed by the destination. With one source file and a
    /// destination that is not an existing folder, the file is copied *to*
    /// that path; otherwise everything is copied *into* the destination folder.
    #[arg(required = true, num_args = 2.., value_name = "PATH")]
    paths: Vec<PathBuf>,

    /// Integrity check.
    #[arg(long, value_enum, default_value_t = Verify::Off)]
    verify: Verify,

    /// Checksum algorithm for --verify / --manifest.
    #[arg(long, value_enum, default_value_t = Algo::Xxh3)]
    algo: Algo,

    /// Speed cap, e.g. 10MB, 500k, 1.5G, 50MiB (decimal unless "i" is given).
    #[arg(long, value_parser = parse_size, value_name = "RATE")]
    limit: Option<u64>,

    /// What to do when a file can't be copied. Default: ask on a terminal, otherwise auto.
    #[arg(long, value_enum)]
    on_error: Option<OnError>,

    /// Retries for transient errors (locked files, I/O hiccups) in auto mode.
    #[arg(long, default_value_t = 5)]
    retries: u32,

    /// Replace files that already exist at the destination.
    #[arg(long)]
    overwrite: bool,

    /// Flush every file to disk before committing it.
    #[arg(long)]
    fsync: bool,

    /// I/O chunk size (64KiB..4MiB).
    #[arg(long, value_parser = parse_size, default_value = "1MiB")]
    chunk: u64,

    /// Buffers in flight between reader and writer (1 = no pipelining).
    #[arg(long, default_value_t = 4)]
    depth: usize,

    /// Don't copy timestamps, permissions or extended attributes.
    #[arg(long)]
    no_preserve: bool,

    /// Copy Windows ACLs (DACL) too.
    #[arg(long)]
    acls: bool,

    /// Rename files whose names are illegal on Windows/exFAT (default on Windows).
    #[arg(long)]
    windows_safe_names: bool,

    /// Write a checksum manifest of the copied files (implies --verify source).
    #[arg(long, value_name = "FILE")]
    manifest: Option<PathBuf>,

    /// No progress line.
    #[arg(short, long)]
    quiet: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum Verify {
    /// No hashing.
    Off,
    /// Hash the source while copying (for manifests).
    Source,
    /// Re-read the destination from disk and compare.
    Readback,
}

#[derive(Clone, Copy, ValueEnum)]
enum Algo {
    Xxh3,
    Sha256,
    Md5,
}

impl From<Algo> for Algorithm {
    fn from(a: Algo) -> Self {
        match a {
            Algo::Xxh3 => Algorithm::Xxh3,
            Algo::Sha256 => Algorithm::Sha256,
            Algo::Md5 => Algorithm::Md5,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum OnError {
    /// Prompt: Retry / Skip / Cancel.
    Ask,
    /// Retry transient errors, skip the rest, stop on a full disk.
    Auto,
    /// Skip any file that fails.
    Skip,
    /// Stop at the first error.
    Cancel,
}

fn main() -> ExitCode {
    match Cli::parse().command {
        Command::Copy(args) => copy(args),
        Command::Hash { algo, files } => hash(algo.into(), &files),
        Command::Check { algo, manifest } => check(algo.into(), &manifest),
    }
}

// ---------------------------------------------------------------------------
// copy
// ---------------------------------------------------------------------------

fn build_request(paths: &[PathBuf]) -> TransferRequest {
    let (dest, sources) = paths.split_last().expect("clap enforces >= 2 paths");
    let dest_str = dest.to_string_lossy();
    let dest_is_dir_syntax = dest_str.ends_with('/') || dest_str.ends_with(std::path::MAIN_SEPARATOR);
    if sources.len() == 1 && sources[0].is_file() && !dest.is_dir() && !dest_is_dir_syntax {
        TransferRequest::file_to(&sources[0], dest)
    } else {
        TransferRequest::into_dir(sources.to_vec(), dest)
    }
}

fn copy(args: CopyArgs) -> ExitCode {
    let interactive = io::stdin().is_terminal();
    let show_progress = !args.quiet && io::stderr().is_terminal();
    let on_error = args.on_error.unwrap_or(if interactive { OnError::Ask } else { OnError::Auto });
    let policy = match on_error {
        OnError::Ask => ErrorPolicy::Ask,
        OnError::Auto => ErrorPolicy::Auto(RetryPolicy { max_retries: args.retries, ..Default::default() }),
        OnError::Skip => ErrorPolicy::Skip,
        OnError::Cancel => ErrorPolicy::Cancel,
    };
    let verify = match (args.verify, args.manifest.is_some()) {
        (Verify::Readback, _) => VerifyMode::ReadBack,
        (Verify::Source, _) | (Verify::Off, true) => VerifyMode::SourceHash,
        (Verify::Off, false) => VerifyMode::Off,
    };
    let preserve = !args.no_preserve;
    let options = CopyOptions {
        chunk_size: args.chunk as usize,
        pipeline_depth: args.depth,
        verify,
        algorithm: args.algo.into(),
        overwrite: args.overwrite,
        fsync: args.fsync,
        preserve_mtime: preserve,
        preserve_permissions: preserve,
        preserve_xattrs: preserve,
        preserve_acls: args.acls,
        ..Default::default()
    };
    let plan = PlanOptions {
        windows_safe_names: args.windows_safe_names || PlanOptions::default().windows_safe_names,
        ..Default::default()
    };

    let (queue, events) = Queue::start(QueueConfig {
        options,
        policy,
        plan,
        speed_limit: args.limit,
        telemetry_interval: Duration::from_millis(200),
        ..Default::default()
    });

    // Ctrl-C once: cancel cleanly (partial file removed). Twice: exit now.
    let presses = Arc::new(AtomicUsize::new(0));
    {
        let (q, presses) = (queue.clone(), presses.clone());
        let _ = ctrlc::set_handler(move || {
            if presses.fetch_add(1, Ordering::SeqCst) == 0 {
                eprintln!("\n^C cancelling (press again to quit immediately)…");
                q.cancel_all();
            } else {
                std::process::exit(130);
            }
        });
    }

    let request = build_request(&args.paths);
    let manifest_root = match &request.target {
        Target::IntoDir(d) => d.clone(),
        Target::FileTo(p) => p.parent().map(Path::to_path_buf).unwrap_or_default(),
        Target::Backend(_) => PathBuf::new(), // remote: manifest paths stay relative
    };
    let job = queue.add(request);

    let mut ui = Terminal { status_shown: false, show_progress };
    let mut remembered: HashMap<ErrorClass, ErrorAction> = HashMap::new();
    let mut manifest_lines = Vec::new();
    let mut final_state = JobState::Pending;
    let mut summary = JobSummary::default();

    for ev in events.iter() {
        match ev {
            Event::Started { files, bytes, warnings, .. } => {
                for w in warnings {
                    ui.line(&format!("warning: {w}"));
                }
                let limit = queue.speed_limit().map(|l| format!(", limit {}", human_rate(l as f64))).unwrap_or_default();
                ui.line(&format!("{files} file(s), {}{limit}", human_bytes(bytes)));
            }
            Event::Progress { telemetry: t, .. } => ui.status(&t, queue.is_paused()),
            Event::FileCopied { dst, digest, warnings, .. } => {
                for w in warnings {
                    ui.line(&format!("warning: {}: {w}", dst.display()));
                }
                if let Some(d) = digest {
                    let rel = dst.strip_prefix(&manifest_root).unwrap_or(&dst);
                    manifest_lines.push(format!("{d}  {}", rel.display()));
                }
            }
            Event::FileSkipped { src, reason, .. } => ui.line(&format!("SKIPPED {}: {reason}", src.display())),
            Event::FileFailed { src, reason, .. } => ui.line(&format!("FAILED  {}: {reason}", src.display())),
            Event::Decision(d) => {
                let action = match remembered.get(&d.class) {
                    Some(a) => *a,
                    None => {
                        let (a, remember) = ui.prompt(&d);
                        if remember {
                            remembered.insert(d.class, a);
                        }
                        a
                    }
                };
                d.respond(action);
            }
            Event::Finished { job: j, state, summary: s } if j == job => {
                final_state = state;
                summary = s;
                break;
            }
            _ => {}
        }
    }
    ui.clear_status();
    queue.shutdown();

    if let Some(path) = &args.manifest {
        if let Err(e) = write_manifest(path, &manifest_lines) {
            eprintln!("error writing manifest {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
        eprintln!("manifest: {} ({} entries)", path.display(), manifest_lines.len());
    }

    eprintln!(
        "copied {} file(s), {} in {} ({}){}",
        summary.files_copied,
        human_bytes(summary.bytes_copied),
        human_duration(summary.elapsed),
        human_rate(summary.throughput_bps),
        if verify == VerifyMode::ReadBack { ", verified" } else { "" }
    );
    if summary.files_skipped + summary.files_failed > 0 {
        eprintln!("{} skipped, {} failed", summary.files_skipped, summary.files_failed);
    }
    match final_state {
        JobState::Completed => ExitCode::SUCCESS,
        JobState::Cancelled => {
            eprintln!("cancelled");
            ExitCode::from(130)
        }
        JobState::Failed(msg) => {
            eprintln!("error: {msg}");
            ExitCode::from(2)
        }
        _ => ExitCode::FAILURE,
    }
}

fn write_manifest(path: &Path, lines: &[String]) -> io::Result<()> {
    let mut w = BufWriter::new(File::create(path)?);
    for l in lines {
        writeln!(w, "{l}")?;
    }
    w.flush()
}

/// Owns stderr: one rewritable status line plus normal log lines above it.
struct Terminal {
    status_shown: bool,
    show_progress: bool,
}

impl Terminal {
    fn clear_status(&mut self) {
        if self.status_shown {
            eprint!("\r\x1b[2K");
            self.status_shown = false;
        }
    }

    fn line(&mut self, s: &str) {
        self.clear_status();
        eprintln!("{s}");
    }

    fn status(&mut self, t: &Snapshot, paused: bool) {
        if !self.show_progress {
            return;
        }
        let file = t
            .current_file
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let eta = if paused { "paused".to_string() } else { t.eta.map(human_duration).unwrap_or_else(|| "--".into()) };
        let bar_w = 24;
        let filled = ((t.fraction * bar_w as f64).round() as usize).min(bar_w);
        eprint!(
            "\r\x1b[2K[{}{}] {:>5.1}%  {:>11}  ETA {:<8} {}/{}  {:?}: {}",
            "#".repeat(filled),
            "-".repeat(bar_w - filled),
            t.fraction * 100.0,
            human_rate(t.rate_bps),
            eta,
            t.files_done,
            t.files_total,
            t.phase,
            file
        );
        let _ = io::stderr().flush();
        self.status_shown = true;
    }

    /// Returns (action, apply-to-all).
    fn prompt(&mut self, d: &DecisionRequest) -> (ErrorAction, bool) {
        self.clear_status();
        eprintln!("\n!! {} — {}", d.class, d.path.display());
        let at = d.offset.map(|o| format!(" at byte {o}")).unwrap_or_default();
        eprintln!("   while {}{at} (attempt {}): {}", d.operation, d.attempt, d.message);
        loop {
            eprint!("   [r]etry  [s]kip  [c]ancel   (S/C = same answer for all '{}' errors) > ", d.class);
            let _ = io::stderr().flush();
            let mut input = String::new();
            if io::stdin().lock().read_line(&mut input).unwrap_or(0) == 0 {
                return (ErrorAction::Cancel, false); // stdin closed
            }
            let c = input.trim().chars().next().unwrap_or(' ');
            let action = match c.to_ascii_lowercase() {
                'r' => ErrorAction::Retry,
                's' => ErrorAction::Skip,
                'c' => ErrorAction::Cancel,
                _ => continue,
            };
            // "Retry all" would loop forever on a permanent error, so it is never remembered.
            return (action, c.is_ascii_uppercase() && action != ErrorAction::Retry);
        }
    }
}

// ---------------------------------------------------------------------------
// hash / check
// ---------------------------------------------------------------------------

fn hash(algo: Algorithm, files: &[PathBuf]) -> ExitCode {
    let mut ok = true;
    for f in files {
        match hash_file(f, algo) {
            Ok(d) => println!("{d}  {}", f.display()),
            Err(e) => {
                eprintln!("{}: {e}", f.display());
                ok = false;
            }
        }
    }
    if ok { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

fn check(algo: Algorithm, manifest: &Path) -> ExitCode {
    let file = match File::open(manifest) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("{}: {e}", manifest.display());
            return ExitCode::from(2);
        }
    };
    let base = manifest.parent().unwrap_or(Path::new("."));
    let (mut good, mut bad) = (0, 0);
    for (n, line) in io::BufReader::new(file).lines().enumerate() {
        let Ok(line) = line else { continue };
        if line.trim().is_empty() || line.starts_with('#') {
            continue;
        }
        // "<hex>  <path>" (two spaces, or " *" for binary mode in GNU format)
        let Some((hex, path)) = line.split_once("  ").or_else(|| line.split_once(" *")) else {
            eprintln!("line {}: malformed", n + 1);
            bad += 1;
            continue;
        };
        let path = base.join(path);
        let Some(expected) = Digest::from_hex(algo, hex) else {
            eprintln!("line {}: not a {algo} digest (try --algo)", n + 1);
            bad += 1;
            continue;
        };
        match hash_file(&path, algo) {
            Ok(actual) if actual == expected => {
                good += 1;
                println!("{}: OK", path.display());
            }
            Ok(_) => {
                bad += 1;
                println!("{}: FAILED", path.display());
            }
            Err(e) => {
                bad += 1;
                println!("{}: FAILED open or read ({e})", path.display());
            }
        }
    }
    eprintln!("{good} OK, {bad} failed");
    if bad == 0 { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

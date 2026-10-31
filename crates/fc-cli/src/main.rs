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
//! fcopy copy photos/ alice@nas:/backup/photos            # upload to a PC running fcopy-agent
//! fcopy copy alice@nas:/backup/photos ~/restore          # download (resumes after a dropout)
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
    /// List PCs running fcopy-agent on this network.
    Peers {
        /// How long to listen, in seconds.
        #[arg(long, default_value_t = 3)]
        timeout: u64,
    },
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
    /// Either end may be a PC running fcopy-agent (`user@host:/share/path`): a
    /// remote destination uploads, remote sources download into a local folder.
    #[arg(required = true, num_args = 2.., value_name = "PATH")]
    paths: Vec<PathBuf>,

    /// Password for a remote PC. Visible in process listings: prefer FCOPY_PASSWORD or the prompt.
    #[arg(long, value_name = "PASSWORD")]
    password: Option<String>,

    /// Read the remote password from a file (first line).
    #[arg(long, value_name = "FILE", conflicts_with = "password")]
    password_file: Option<PathBuf>,

    /// Trust this certificate fingerprint for a remote destination the first time.
    #[arg(long, value_name = "SHA256:…")]
    accept_fingerprint: Option<String>,

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

    /// Existing files: ask, replace, skip, keep-both, or compare (ask if different).
    #[arg(long, value_enum, conflicts_with = "overwrite")]
    conflict: Option<ConflictMode>,

    /// Flush every file to disk before committing it.
    #[arg(long)]
    fsync: bool,

    /// Move: delete each source once its copy is synced and read back
    /// (implies --fsync and --verify readback). Files that are not copied stay.
    #[arg(long = "move")]
    move_files: bool,

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
        Command::Peers { timeout } => peers(timeout),
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

#[derive(Clone, Copy, clap::ValueEnum)]
enum ConflictMode { Ask, Replace, Skip, KeepBoth, Compare }

fn conflicts(args: &CopyArgs) -> Option<Arc<fc_engine::conflict::ConflictResolver>> {
    use fc_engine::conflict::{ConflictPolicy as P, ConflictResolver};
    if args.overwrite { return None; }
    let mode = args.conflict.or_else(|| io::stdin().is_terminal().then_some(ConflictMode::Ask))?;
    let policy = match mode {
        ConflictMode::Ask => P::Ask, ConflictMode::Replace => P::Replace, ConflictMode::Skip => P::Skip,
        ConflictMode::KeepBoth => P::KeepBoth, ConflictMode::Compare => P::Compare,
    };
    let resolver = ConflictResolver::new(policy);
    Some(Arc::new(if io::stdin().is_terminal() {
        resolver.with_handler(Arc::new(|details, _| prompt_conflict(details)))
    } else { resolver }))
}

fn prompt_conflict(details: &fc_engine::conflict::Conflict) -> fc_engine::conflict::ConflictChoice {
    use fc_engine::conflict::{ConflictAction as A, ConflictChoice};
    let cancel = ConflictChoice { action: A::Cancel, apply_to_all: false };
    if !io::stdin().is_terminal() {
        eprintln!("{}: conflict needs a terminal; use --conflict replace, skip or keep-both", details.destination.display());
        return cancel;
    }
    eprintln!("\n{}: {} -> {}", if details.different { "Contents differ" } else { "Destination exists" }, details.source.display(), details.destination.display());
    loop {
        eprint!("[r]eplace [s]kip [k]eep both {}[c]ancel (uppercase = apply to remaining conflicts in this job) > ",
            if details.different { "" } else { "[m]compare " });
        if let Err(e) = io::stderr().flush() { eprintln!("cannot display conflict prompt: {e}"); return cancel; }
        let mut input = String::new();
        match io::stdin().read_line(&mut input) {
            Ok(0) => { eprintln!("input closed; cancelling"); return cancel; }
            Err(e) => { eprintln!("cannot read conflict choice: {e}"); return cancel; }
            Ok(_) => {}
        }
        let letter = input.trim().chars().next().unwrap_or(' ');
        let action = match letter.to_ascii_lowercase() {
            'r' => A::Replace, 's' => A::Skip, 'k' => A::KeepBoth, 'c' => A::Cancel,
            'm' if !details.different => A::Compare,
            _ => { eprintln!("choose one of the listed actions"); continue; }
        };
        return ConflictChoice { action, apply_to_all: letter.is_ascii_uppercase() };
    }
}

fn copy(args: CopyArgs) -> ExitCode {
    match remote_sources(&args) {
        Ok(Some(_)) if args.move_files => {
            eprintln!("error: --move is not supported when copying from another PC; copy instead");
            return ExitCode::from(2);
        }
        Ok(Some(specs)) => return pull(&args, specs),
        Ok(None) => {}
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    }
    let interactive = io::stdin().is_terminal();
    let remote = match remote_destination(&args) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
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
        conflicts: conflicts(&args),
        fsync: args.fsync,
        preserve_mtime: preserve,
        preserve_permissions: preserve,
        preserve_xattrs: preserve,
        preserve_acls: args.acls,
        move_source: args.move_files,
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

    let request = match remote {
        Some(backend) => {
            let (dest, sources) = args.paths.split_last().expect("clap enforces >= 2 paths");
            let _ = dest;
            TransferRequest::to_backend(sources.to_vec(), backend)
        }
        None => build_request(&args.paths),
    };
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
            Event::Conflict { job: id } => {
                if let Some(conflict) = queue.job(id).and_then(|j| j.conflict) {
                    ui.clear_status();
                    let reply = prompt_conflict(&conflict.details);
                    if !queue.resolve_conflict(id, conflict.id, reply) {
                        ui.line("Conflict is no longer waiting for an answer.");
                    }
                }
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

// ---------------------------------------------------------------------------
// remote destinations and peers
// ---------------------------------------------------------------------------

/// The remote backend for `copy`, if the destination is `user@host:/share/dir`.
/// Returns `Ok(None)` for a local copy.
fn remote_destination(args: &CopyArgs) -> Result<Option<Arc<dyn TransferBackend>>, String> {
    use fc_remote::client::RemoteSpec;

    let dest = args.paths.last().expect("clap enforces >= 2 paths").to_string_lossy().to_string();
    // A local path is never a valid `host:/share` target, so a parse failure means local.
    let Ok(spec) = RemoteSpec::parse(&dest) else { return Ok(None) };
    Ok(Some(Arc::new(connect_backend(args, spec)?)))
}

/// A connection to the agent named by `spec`: pinned from known_hosts, or
/// trusted on first use after confirmation.
fn connect_backend(args: &CopyArgs, spec: fc_remote::client::RemoteSpec) -> Result<fc_remote::backend::RemoteBackend, String> {
    use fc_remote::backend::RemoteBackend;
    use fc_remote::client::Connector;
    use fc_remote::config::{KnownHosts, default_client_dir};

    let known_path = default_client_dir().join("known_hosts");
    let known = KnownHosts::load(&known_path);
    let host_port = spec.host_port();
    let pinned = known.get(&host_port).map(str::to_string);

    let password = read_password(args, &spec)?;
    let mut connector = Connector::new(spec).password(password).pinned(pinned.clone());
    if pinned.is_none() {
        // First contact. The callback runs after the certificate arrives and
        // before anything is sent. It trusts the fingerprint only if it matches
        // --accept-fingerprint, or the user says yes at the prompt, and it saves
        // trust only then.
        let known_path = known_path.clone();
        let accepted = args.accept_fingerprint.clone();
        let interactive = io::stdin().is_terminal();
        connector = connector.on_unknown_host(Arc::new(move |host, fp| {
            if accepted.as_deref() == Some(fp) {
                save_trust(&known_path, host, fp);
                return true;
            }
            eprintln!("the PC at {host} presents fingerprint {fp}");
            if !interactive {
                eprintln!("not trusted: pass --accept-fingerprint {fp} to trust it");
                return false;
            }
            eprint!("trust this PC? [y/N] ");
            let mut answer = String::new();
            if io::stdin().lock().read_line(&mut answer).is_err() || !answer.trim().eq_ignore_ascii_case("y") {
                return false;
            }
            save_trust(&known_path, host, fp);
            true
        }));
    }
    let backend = RemoteBackend::new(connector).map_err(|e| format!("cannot start the remote transfer engine: {e}"))?;
    Ok(backend)
}

/// Remote sources (`user@host:/share/path`) mean a download. Returns `Ok(None)`
/// when every source is local. A download needs a local destination and all
/// sources on one PC and account.
fn remote_sources(args: &CopyArgs) -> Result<Option<Vec<fc_remote::client::RemoteSpec>>, String> {
    use fc_remote::client::RemoteSpec;

    let (dest, sources) = args.paths.split_last().expect("clap enforces >= 2 paths");
    let parsed: Vec<Option<RemoteSpec>> = sources.iter().map(|s| RemoteSpec::parse(&s.to_string_lossy()).ok()).collect();
    if parsed.iter().all(Option::is_none) {
        return Ok(None);
    }
    if RemoteSpec::parse(&dest.to_string_lossy()).is_ok() {
        return Err("copying from one remote PC to another is not supported".into());
    }
    if parsed.iter().any(Option::is_none) {
        return Err("cannot mix remote and local sources in one copy".into());
    }
    let specs: Vec<RemoteSpec> = parsed.into_iter().flatten().collect();
    if specs.iter().any(|s| s.host_port() != specs[0].host_port() || s.user != specs[0].user) {
        return Err("all remote sources must be on the same PC and account".into());
    }
    Ok(Some(specs))
}

/// `fcopy copy user@host:/share/path… DEST`: download into a local folder.
fn pull(args: &CopyArgs, specs: Vec<fc_remote::client::RemoteSpec>) -> ExitCode {
    use fc_remote::backend::PullOptions;
    use fc_remote::protocol::HashAlgo;

    if args.manifest.is_some() {
        eprintln!("error: --manifest is not supported for downloads yet");
        return ExitCode::from(2);
    }
    let dest = args.paths.last().expect("clap enforces >= 2 paths").clone();
    if specs.len() > 1 {
        if let Err(e) = std::fs::create_dir_all(&dest) {
            eprintln!("error: cannot create {}: {e}", dest.display());
            return ExitCode::from(2);
        }
    }
    let backend = match connect_backend(args, specs[0].clone()) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };

    let control = Arc::new(JobControl::new());
    let presses = Arc::new(AtomicUsize::new(0));
    {
        let (control, presses) = (control.clone(), presses.clone());
        let _ = ctrlc::set_handler(move || {
            if presses.fetch_add(1, Ordering::SeqCst) == 0 {
                eprintln!("\n^C cancelling; partial files are kept for resume (press again to quit immediately)…");
                control.cancel();
            } else {
                std::process::exit(130);
            }
        });
    }

    let limiter = args.limit.map(|l| RateLimiter::new(Some(l)));
    let opts = PullOptions {
        algo: match args.algo {
            Algo::Sha256 => HashAlgo::Sha256,
            _ => HashAlgo::Xxh3, // MD5 isn't offered remotely
        },
        overwrite: args.overwrite,
        conflicts: conflicts(args),
        preserve: !args.no_preserve,
    };
    let line = PullLine::new(!args.quiet && io::stderr().is_terminal());

    let (mut files, mut bytes, mut skipped, mut failed) = (0u64, 0u64, 0usize, 0usize);
    let mut cancelled = false;
    let started = std::time::Instant::now();
    for spec in &specs {
        match backend.pull(&spec.path, &dest, &opts, &control, limiter.as_ref(), &line) {
            Ok(r) => {
                files += r.files;
                bytes += r.bytes;
                skipped += r.skipped.len();
                failed += r.failed.len();
                line.clear();
                for p in &r.skipped {
                    eprintln!("SKIPPED {p}");
                }
                for (p, why) in &r.failed {
                    eprintln!("FAILED  {p}: {why}");
                }
                cancelled |= r.cancelled;
            }
            Err(e) => {
                line.clear();
                eprintln!("error: {spec}: {e}");
                failed += 1;
            }
        }
        if cancelled {
            break;
        }
    }
    line.clear();
    let elapsed = started.elapsed();
    eprintln!(
        "downloaded {files} file(s), {} in {} ({})",
        human_bytes(bytes),
        human_duration(elapsed),
        human_rate(bytes as f64 / elapsed.as_secs_f64().max(0.001))
    );
    if skipped + failed > 0 {
        eprintln!("{skipped} skipped, {failed} failed");
    }
    if cancelled {
        eprintln!("cancelled");
        return ExitCode::from(130);
    }
    if skipped + failed > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

/// One rewritable status line for a download.
struct PullLine {
    show: bool,
    state: std::sync::Mutex<PullLineState>,
}

struct PullLineState {
    started: std::time::Instant,
    last_draw: std::time::Instant,
    total: u64,
    file_bytes: u64,
    file_size: u64,
    name: String,
    drawn: bool,
}

impl PullLine {
    fn new(show: bool) -> Self {
        let now = std::time::Instant::now();
        Self {
            show,
            state: std::sync::Mutex::new(PullLineState { started: now, last_draw: now, total: 0, file_bytes: 0, file_size: 0, name: String::new(), drawn: false }),
        }
    }

    fn clear(&self) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if s.drawn {
            eprint!("\r\x1b[2K");
            s.drawn = false;
        }
    }

    fn draw(&self, s: &mut PullLineState) {
        if !self.show {
            return;
        }
        let pct = if s.file_size == 0 { 100.0 } else { s.file_bytes as f64 * 100.0 / s.file_size as f64 };
        let rate = s.total as f64 / s.started.elapsed().as_secs_f64().max(0.001);
        let name: String = s.name.chars().rev().take(48).collect::<Vec<_>>().into_iter().rev().collect();
        eprint!("\r\x1b[2K{pct:>5.1}%  {:>11}  {} total  {name}", human_rate(rate), human_bytes(s.total));
        let _ = io::stderr().flush();
        s.drawn = true;
        s.last_draw = std::time::Instant::now();
    }
}

impl fc_remote::backend::PullProgress for PullLine {
    fn begin(&self, remote: &str, size: u64) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.name = remote.rsplit('/').next().unwrap_or(remote).to_string();
        s.file_size = size;
        s.file_bytes = 0;
        self.draw(&mut s);
    }

    fn advance(&self, bytes: u64) {
        let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        s.total += bytes;
        s.file_bytes += bytes;
        if s.last_draw.elapsed() >= Duration::from_millis(100) {
            self.draw(&mut s);
        }
    }
}

/// The password, from --password, --password-file, FCOPY_PASSWORD, or a prompt.
fn read_password(args: &CopyArgs, spec: &fc_remote::client::RemoteSpec) -> Result<String, String> {
    if let Some(p) = &args.password {
        eprintln!("warning: --password is visible to other users in process listings");
        return Ok(p.clone());
    }
    if let Some(path) = &args.password_file {
        let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        return Ok(text.lines().next().unwrap_or_default().to_string());
    }
    if let Ok(p) = std::env::var("FCOPY_PASSWORD") {
        return Ok(p);
    }
    if io::stdin().is_terminal() {
        return rpassword::prompt_password(format!("password for {spec}: ")).map_err(|e| e.to_string());
    }
    Err("no password: set FCOPY_PASSWORD, use --password-file, or run from a terminal".into())
}

fn peers(timeout: u64) -> ExitCode {
    use fc_remote::config::{KnownHosts, default_client_dir};
    use fc_remote::discovery::browse;

    eprintln!("listening for {timeout} s…");
    let found = match browse(Duration::from_secs(timeout)) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    if found.is_empty() {
        eprintln!("no PCs found. Check that their agents are running and on this network.");
        return ExitCode::FAILURE;
    }
    let known = KnownHosts::load(&default_client_dir().join("known_hosts"));
    for p in &found {
        let addr = p.addresses.first().map(|a| a.to_string()).unwrap_or_else(|| p.host.clone());
        let trust = match known.get(&format!("{addr}:{}", p.port)) {
            Some(fp) if fp == p.fingerprint => "trusted",
            Some(_) => "FINGERPRINT CHANGED",
            None => "new",
        };
        println!("{}\t{addr}:{}\t{}\t{trust}", p.name, p.port, p.fingerprint);
    }
    ExitCode::SUCCESS
}

fn save_trust(path: &Path, host: &str, fingerprint: &str) {
    let mut known = fc_remote::config::KnownHosts::load(path);
    known.set(host, fingerprint);
    let _ = known.save(path);
}

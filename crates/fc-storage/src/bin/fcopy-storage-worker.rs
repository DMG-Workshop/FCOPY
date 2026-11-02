//! `fcopy-storage-worker`: the privileged helper that writes to a disk. It runs once
//! per operation, started through pkexec. It reads one signed plan from stdin and
//! streams events to stdout. The protocol is documented in `fc_storage::worker`.
//!
//! The audit log path is fixed in this file, not taken from the command line, so a
//! caller cannot move the log.

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("the storage worker is available on Linux in this build");
    std::process::exit(2);
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    use std::fs::{self, File, OpenOptions};
    use std::io::{self, BufReader, Write};
    use std::path::Path;
    use std::process::ExitCode;
    use std::time::{SystemTime, UNIX_EPOCH};

    use fc_storage::linux::LinuxSysfs;
    use fc_storage::wipe::random_seed;
    use fc_storage::worker::{Served, production_device_allowed, serve};

    const AUDIT_LOG: &str = "/var/lib/fcopy/storage-audit.jsonl";

    /// Syncs the log on every flush, so a record is on stable storage before it is trusted.
    struct SyncOnFlush(File);

    impl Write for SyncOnFlush {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.write(buf)
        }
        fn flush(&mut self) -> io::Result<()> {
            self.0.flush()?;
            self.0.sync_all()
        }
    }

    let log_path = Path::new(AUDIT_LOG);
    if let Some(dir) = log_path.parent() {
        if let Err(e) = fs::create_dir_all(dir) {
            eprintln!("cannot create {}: {e}", dir.display());
            return ExitCode::from(3);
        }
    }
    let log_file = match OpenOptions::new().create(true).append(true).open(log_path) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("cannot open the audit log: {e}");
            return ExitCode::from(3);
        }
    };
    let mut log = SyncOnFlush(log_file);

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let seed = match random_seed() {
        Ok(seed) => seed,
        Err(e) => {
            eprintln!("no random source: {e}");
            return ExitCode::from(3);
        }
    };

    let platform = LinuxSysfs::system();
    let stdin = io::stdin();
    let mut input = BufReader::new(stdin.lock());
    let stdout = io::stdout();
    let mut events = stdout.lock();
    match serve(
        &platform,
        &mut input,
        &mut events,
        &mut log,
        now,
        &production_device_allowed,
        seed,
    ) {
        Ok(Served::Completed) => ExitCode::SUCCESS,
        Ok(Served::Failed) => ExitCode::from(1),
        Err(e) => {
            eprintln!("worker I/O error: {e}");
            ExitCode::from(2)
        }
    }
}

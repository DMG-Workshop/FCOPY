//! The worker, end to end, against a real loop device.
//!
//! A loop device has no serial number of its own, so the test presents it as a disk
//! with a serial, through a thin wrapper. Every write still goes through the real
//! Linux writer, the real exclusive claim, and the real safety rules. The worker's
//! own allowlist is checked too: production settings refuse a loop device.
//!
//! Opt-in, like the other loop tests:
//! `FCOPY_LOOP_TESTS=1 cargo test -p fc-storage --features storage-destructive --test worker_loop`

#![cfg(all(target_os = "linux", feature = "storage-destructive"))]

use std::collections::HashSet;
use std::fs;
use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use fc_storage::health::Health;
use fc_storage::linux::LinuxSysfs;
use fc_storage::model::{Disk, DiskId, DiskIdentity, Transport};
use fc_storage::orchestrate::{Outcome, Request, WipeScope};
use fc_storage::plan::{FsKind, TablePlan};
use fc_storage::platform::{ExclusiveDisk, OsError, PartitionRef, Platform};
use fc_storage::protocol::{PlanBody, WorkerEvent, decode_event, seal};
use fc_storage::safety::Policy;
use fc_storage::wipe::{RawIo, WipeMethod, random_seed};
use fc_storage::worker::{Served, production_device_allowed, serve};

const MIB: u64 = 1 << 20;
const SIZE: u64 = 16 * MIB;
const SERIAL: &str = "LOOP-TEST-1";

fn enabled() -> bool {
    if std::env::var_os("FCOPY_LOOP_TESTS").is_some() {
        return true;
    }
    eprintln!("skipped: set FCOPY_LOOP_TESTS=1 (needs root and losetup)");
    false
}

/// A loop device backed by a file. Detached on drop.
struct Loop {
    device: PathBuf,
    backing: PathBuf,
    _dir: tempfile::TempDir,
}

impl Loop {
    fn attach(fill: u8) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let backing = dir.path().join("disk.img");
        fs::write(&backing, vec![fill; SIZE as usize]).expect("write backing file");
        let out = Command::new("losetup")
            .args(["--find", "--show"])
            .arg(&backing)
            .output()
            .expect("losetup must be installed");
        assert!(out.status.success(), "losetup failed");
        let device = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        // Path::starts_with compares whole components, so compare the text instead.
        if !device.to_string_lossy().starts_with("/dev/loop") {
            // Detach the device this test just attached before failing, so it is not leaked.
            let _ = Command::new("losetup")
                .arg("--detach")
                .arg(&device)
                .status();
            panic!("refusing {}: not a loop device", device.display());
        }
        Self {
            device,
            backing,
            _dir: dir,
        }
    }
}

impl Drop for Loop {
    fn drop(&mut self) {
        let _ = Command::new("losetup")
            .arg("--detach")
            .arg(&self.device)
            .status();
    }
}

/// Presents one loop device as a whole disk with a serial. The writer underneath is
/// the real one.
struct LoopPlatform {
    inner: LinuxSysfs,
    disk: Disk,
}

impl Platform for LoopPlatform {
    fn enumerate(&self) -> Result<Vec<Disk>, OsError> {
        Ok(vec![self.disk.clone()])
    }

    fn system_disks(&self) -> Option<HashSet<DiskId>> {
        Some(HashSet::new())
    }

    fn health(&self, disk: &Disk) -> Result<Health, OsError> {
        self.inner.health(disk)
    }

    fn open_exclusive(&self, disk: &Disk) -> Result<Box<dyn ExclusiveDisk>, OsError> {
        let inner = self.inner.open_exclusive(disk)?;
        Ok(Box::new(WithIdentity {
            inner,
            identity: self.disk.identity.clone(),
        }))
    }

    fn format_partition(
        &self,
        part: &PartitionRef,
        fs: FsKind,
        label: &str,
    ) -> Result<(), OsError> {
        self.inner.format_partition(part, fs, label)
    }
}

/// Passes every operation to the real writer. Only the identity it reports is
/// supplied, since a loop device has no serial of its own.
struct WithIdentity {
    inner: Box<dyn ExclusiveDisk>,
    identity: DiskIdentity,
}

impl Read for WithIdentity {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl Write for WithIdentity {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl Seek for WithIdentity {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.inner.seek(pos)
    }
}

impl RawIo for WithIdentity {
    fn size(&self) -> u64 {
        RawIo::size(&*self.inner)
    }
    fn block_size(&self) -> u32 {
        RawIo::block_size(&*self.inner)
    }
    fn flush_to_media(&mut self) -> io::Result<()> {
        RawIo::flush_to_media(&mut *self.inner)
    }
}

impl ExclusiveDisk for WithIdentity {
    fn identity(&self) -> Result<DiskIdentity, OsError> {
        Ok(self.identity.clone())
    }
    fn commit_table(&mut self, table: &TablePlan) -> Result<(), OsError> {
        self.inner.commit_table(table)
    }
    fn rescan(&mut self) -> Result<Vec<PartitionRef>, OsError> {
        self.inner.rescan()
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The input the worker reads on stdin, for a plan that wipes `disk`.
fn worker_input(disk: &Disk, key: [u8; 32]) -> String {
    let request = Request {
        disk: disk.id.clone(),
        confirmed: disk.identity.clone(),
        typed_serial: SERIAL.into(),
        policy: Policy::default(),
        protected_paths: Vec::new(),
        scope: WipeScope::WholeDevice,
        method: WipeMethod::Clear,
        table: None,
    };
    let body = PlanBody {
        expires_at_unix: now() + 600,
        nonce: "00112233445566778899aabbccddeeff".into(),
        request,
    };
    let env = seal(&key, &body).expect("seal");
    format!(
        "{}\n{}\n{}\n",
        hex(&key),
        hex(&env.mac),
        String::from_utf8(env.body).unwrap()
    )
}

fn loop_disk(lp: &Loop) -> Disk {
    Disk::new(
        lp.device.display().to_string(),
        Transport::Usb,
        true,
        DiskIdentity {
            serial: Some(SERIAL.into()),
            model: "loop test device".into(),
            size_bytes: SIZE,
        },
        512,
        Vec::new(),
    )
}

fn platform_for(disk: &Disk) -> LoopPlatform {
    LoopPlatform {
        inner: LinuxSysfs::at(Path::new("/")),
        disk: disk.clone(),
    }
}

fn parse_events(out: &[u8]) -> Vec<WorkerEvent> {
    String::from_utf8_lossy(out)
        .lines()
        .map(|l| decode_event(l).expect("a valid event line"))
        .collect()
}

#[test]
fn the_worker_wipes_a_loop_device_through_the_real_writer() {
    if !enabled() {
        return;
    }
    let lp = Loop::attach(0xAA);
    let disk = loop_disk(&lp);
    let platform = platform_for(&disk);

    let mut events = Vec::new();
    let mut log = Vec::new();
    let served = serve(
        &platform,
        &mut Cursor::new(worker_input(&disk, [7u8; 32]).into_bytes()),
        &mut events,
        &mut log,
        now(),
        // The test allows loop devices. Production allows only real disks.
        &|path: &str| path.starts_with("/dev/loop") && !path[9..].contains('/'),
        random_seed().unwrap(),
    )
    .unwrap();

    assert_eq!(served, Served::Completed);
    let on_disk = fs::read(&lp.backing).unwrap();
    assert!(on_disk.iter().all(|&b| b == 0), "the loop device was wiped");
    let parsed = parse_events(&events);
    assert!(parsed.iter().any(|e| matches!(e, WorkerEvent::Progress(_))));
    assert_eq!(
        parsed.last(),
        Some(&WorkerEvent::Finished(Outcome::Completed))
    );
    let log = String::from_utf8(log).unwrap();
    assert!(log.contains("\"event\":\"begin\"") && log.contains("\"event\":\"end\""));
}

#[test]
fn production_settings_refuse_a_loop_device_and_write_nothing() {
    if !enabled() {
        return;
    }
    let lp = Loop::attach(0xAA);
    let disk = loop_disk(&lp);
    let platform = platform_for(&disk);

    let mut events = Vec::new();
    let mut log = Vec::new();
    let served = serve(
        &platform,
        &mut Cursor::new(worker_input(&disk, [7u8; 32]).into_bytes()),
        &mut events,
        &mut log,
        now(),
        &production_device_allowed,
        random_seed().unwrap(),
    )
    .unwrap();

    assert_eq!(served, Served::Failed);
    assert!(matches!(
        parse_events(&events).last(),
        Some(WorkerEvent::Finished(Outcome::Refused(_)))
    ));
    let on_disk = fs::read(&lp.backing).unwrap();
    assert!(on_disk.iter().all(|&b| b == 0xAA), "nothing was written");
}

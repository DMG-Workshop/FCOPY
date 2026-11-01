//! Integration tests against real Linux loop devices.
//!
//! A loop device is a kernel block device backed by an ordinary file, so these
//! tests exercise the real direct-I/O path without touching any physical disk.
//!
//! They are opt-in, because they need root and `losetup`. Run them with:
//!
//! ```text
//! FCOPY_LOOP_TESTS=1 cargo test -p fc-storage --features storage-destructive --test loop_device
//! ```
//!
//! Without `FCOPY_LOOP_TESTS` set, each test returns early and passes. The
//! harness only ever uses `/dev/loopN` nodes, and it refuses any other path.

#![cfg(all(target_os = "linux", feature = "storage-destructive"))]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

use fc_storage::linux_block::BlockDevice;
use fc_storage::plan::table_wipe_ranges;
use fc_storage::wipe::{self, Phase, WipeError, WipeMethod};

const MIB: u64 = 1 << 20;
const SIZE: u64 = 16 * MIB;
const SEED: [u8; 32] = [5u8; 32];

fn enabled() -> bool {
    if std::env::var_os("FCOPY_LOOP_TESTS").is_some() {
        return true;
    }
    eprintln!("skipped: set FCOPY_LOOP_TESTS=1 (needs root and losetup)");
    false
}

/// `/dev/loopN` and nothing else. The harness never accepts another device node.
fn is_loop_node(p: &Path) -> bool {
    let Some(name) = p.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    p.parent() == Some(Path::new("/dev"))
        && name
            .strip_prefix("loop")
            .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
}

/// A loop device backed by a file in a temp directory. Detached on drop.
struct Loop {
    device: PathBuf,
    backing: PathBuf,
    _dir: tempfile::TempDir,
}

impl Loop {
    fn with_fill(fill: u8) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let backing = dir.path().join("disk.img");
        fs::write(&backing, vec![fill; SIZE as usize]).expect("write backing file");
        let out = Command::new("losetup")
            .arg("--find")
            .arg("--show")
            .arg(&backing)
            .output()
            .expect("losetup must be installed");
        assert!(
            out.status.success(),
            "losetup failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let device = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
        assert!(
            is_loop_node(&device),
            "refusing {}: not a loop device",
            device.display()
        );
        Self {
            device,
            backing,
            _dir: dir,
        }
    }

    /// The backing file, which reflects every write made through the loop device.
    fn on_disk(&self) -> Vec<u8> {
        fs::read(&self.backing).expect("read backing file")
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

#[test]
fn opens_a_loop_device_with_its_size_and_block_size() {
    if !enabled() {
        return;
    }
    let lp = Loop::with_fill(0);
    let dev = BlockDevice::open(&lp.device).expect("open loop device");
    assert_eq!(wipe::RawIo::size(&dev), SIZE);
    assert_eq!(wipe::RawIo::block_size(&dev), 512);
}

#[test]
fn clear_overwrites_the_whole_loop_device_and_verifies() {
    if !enabled() {
        return;
    }
    let lp = Loop::with_fill(0xAA);
    let mut dev = BlockDevice::open(&lp.device).expect("open");
    let cancel = AtomicBool::new(false);
    let mut phases = Vec::new();
    wipe::run(&mut dev, WipeMethod::Clear, SEED, &cancel, |p| {
        phases.push(p.phase)
    })
    .expect("wipe and verify");
    drop(dev);

    assert!(lp.on_disk().iter().all(|&b| b == 0), "every byte is zero");
    assert!(phases.contains(&Phase::Verify), "the wipe was read back");
}

#[test]
fn random_wipe_is_reproducible_from_its_seed() {
    if !enabled() {
        return;
    }
    let cancel = AtomicBool::new(false);
    let a = Loop::with_fill(0);
    let b = Loop::with_fill(0);
    wipe::run(
        &mut BlockDevice::open(&a.device).unwrap(),
        WipeMethod::ClearRandom,
        SEED,
        &cancel,
        |_| {},
    )
    .unwrap();
    wipe::run(
        &mut BlockDevice::open(&b.device).unwrap(),
        WipeMethod::ClearRandom,
        SEED,
        &cancel,
        |_| {},
    )
    .unwrap();
    let (x, y) = (a.on_disk(), b.on_disk());
    assert_eq!(x, y, "same seed, same bytes");
    assert!(x.iter().any(|&v| v != 0), "not a zero fill");
}

#[test]
fn table_ranges_wipe_only_the_head_and_tail() {
    if !enabled() {
        return;
    }
    let lp = Loop::with_fill(0xAA);
    let mut dev = BlockDevice::open(&lp.device).expect("open");
    let cancel = AtomicBool::new(false);
    let ranges = table_wipe_ranges(SIZE);
    wipe::run_ranges(&mut dev, &ranges, WipeMethod::Clear, SEED, &cancel, |_| {})
        .expect("table wipe");
    drop(dev);

    let data = lp.on_disk();
    let mib = MIB as usize;
    assert!(data[..mib].iter().all(|&b| b == 0), "head zeroed");
    assert!(
        data[mib..data.len() - mib].iter().all(|&b| b == 0xAA),
        "data area untouched"
    );
    assert!(
        data[data.len() - mib..].iter().all(|&b| b == 0),
        "tail zeroed"
    );
}

#[test]
fn cancelling_mid_wipe_leaves_the_rest_of_the_device_as_it_was() {
    if !enabled() {
        return;
    }
    let lp = Loop::with_fill(0xAA);
    let mut dev = BlockDevice::open(&lp.device).expect("open");
    let cancel = AtomicBool::new(false);
    let chunk = 8 * MIB;
    let err = wipe::run(&mut dev, WipeMethod::Clear, SEED, &cancel, |p| {
        if p.phase == Phase::Write && p.done >= chunk {
            cancel.store(true, Ordering::Relaxed);
        }
    })
    .unwrap_err();
    drop(dev);

    assert!(
        matches!(err, WipeError::Cancelled { pass: 1 }),
        "got {err:?}"
    );
    let data = lp.on_disk();
    let split = chunk as usize;
    assert!(
        data[..split].iter().all(|&b| b == 0),
        "first chunk overwritten"
    );
    assert!(data[split..].iter().all(|&b| b == 0xAA), "rest untouched");
}

#[test]
fn a_second_open_of_a_busy_device_is_refused() {
    if !enabled() {
        return;
    }
    let lp = Loop::with_fill(0);
    let _held = BlockDevice::open(&lp.device).expect("first claim");
    // O_EXCL on a block device fails while another claim is held.
    let second = BlockDevice::open(&lp.device);
    assert!(second.is_err(), "exclusive claim must be refused");
}

//! Integration tests against real Linux loop devices.
//!
//! A loop device is a kernel block device backed by an ordinary file, so these
//! tests exercise the real direct-I/O path, partition table writes and
//! filesystem creation without touching any physical disk.
//!
//! They are opt-in, because they need root, `losetup`, `sgdisk`, `blkid`, and the
//! `mkfs` tools for each filesystem. Run them with:
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

use fc_storage::linux::LinuxSysfs;
use fc_storage::linux_block::{BlockDevice, LinuxExclusive};
use fc_storage::linux_tools::layout;
use fc_storage::plan::{FsKind, PartitionSpec, SizeSpec, TablePlan, TableStyle, table_wipe_ranges};
use fc_storage::platform::{ExclusiveDisk, PartitionRef, Platform};
use fc_storage::wipe::{self, Phase, WipeError, WipeMethod};

const MIB: u64 = 1 << 20;
const SIZE: u64 = 16 * MIB;
const SEED: [u8; 32] = [5u8; 32];

fn enabled() -> bool {
    if std::env::var_os("FCOPY_LOOP_TESTS").is_some() {
        return true;
    }
    eprintln!("skipped: set FCOPY_LOOP_TESTS=1 (needs root, losetup, sgdisk, blkid, mkfs tools)");
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

/// A loop device backed by a file in a temp directory. Partition scanning is on,
/// so partition nodes such as `/dev/loop0p1` appear. Detached on drop.
struct Loop {
    device: PathBuf,
    backing: PathBuf,
    _dir: tempfile::TempDir,
}

impl Loop {
    fn attach(size: u64, fill: u8) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let backing = dir.path().join("disk.img");
        fs::write(&backing, vec![fill; size as usize]).expect("write backing file");
        let out = Command::new("losetup")
            .args(["--find", "--show", "--partscan"])
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

    fn with_fill(fill: u8) -> Self {
        Self::attach(SIZE, fill)
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

fn sysfs() -> LinuxSysfs {
    LinuxSysfs::at(Path::new("/"))
}

/// Probes a device with `blkid -p`, which reads the device directly and bypasses the cache.
fn blkid(node: &str, field: &str) -> String {
    let out = Command::new("blkid")
        .args(["-p", "-o", "value", "-s", field, node])
        .output()
        .expect("blkid must be installed");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Detaches the offset loop devices it created, when it goes out of scope.
struct OffsetLoops(Vec<PathBuf>);

impl Drop for OffsetLoops {
    fn drop(&mut self) {
        for dev in &self.0 {
            let _ = Command::new("losetup").arg("--detach").arg(dev).status();
        }
    }
}

/// Attaches the byte range `[offset, offset + size)` of `backing` as its own loop device.
fn attach_range(backing: &Path, offset: u64, size: u64) -> PathBuf {
    let out = Command::new("losetup")
        .args(["--find", "--show", "--offset"])
        .arg(offset.to_string())
        .arg("--sizelimit")
        .arg(size.to_string())
        .arg(backing)
        .output()
        .expect("losetup must be installed");
    assert!(
        out.status.success(),
        "losetup --offset failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let dev = PathBuf::from(String::from_utf8_lossy(&out.stdout).trim());
    assert!(
        is_loop_node(&dev),
        "refusing {}: not a loop device",
        dev.display()
    );
    dev
}

/// Reads the table back with `sgdisk -p`, as (number, first LBA, last LBA).
fn read_table(device: &str) -> Vec<(u32, u64, u64)> {
    let out = Command::new("sgdisk")
        .arg("-p")
        .arg(device)
        .output()
        .expect("sgdisk must be installed");
    assert!(out.status.success(), "sgdisk -p failed");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 3 {
                return None;
            }
            Some((f[0].parse().ok()?, f[1].parse().ok()?, f[2].parse().ok()?))
        })
        .collect()
}

/// Wipes the loop device, writes the table, checks it with `sgdisk -p`, releases the
/// whole-disk claim, and formats each partition. This is the same order the pipeline uses.
///
/// If the kernel creates partition nodes, they are used. If it does not, each
/// partition's byte range is attached as its own loop device. The `mkfs` tools run on
/// a real block device either way. Returns the device of each partition, and a guard
/// that keeps any offset loop devices attached until it is dropped.
fn partition_and_format(lp: &Loop, table: &TablePlan) -> (Vec<String>, OffsetLoops) {
    let mut dev = BlockDevice::open(&lp.device).expect("open loop device");
    let cancel = AtomicBool::new(false);
    wipe::run(&mut dev, WipeMethod::Clear, SEED, &cancel, |_| {}).expect("clean the disk");

    // The layout must use the device's real size, which differs between tests.
    let size = wipe::RawIo::size(&dev);
    let expected = layout(table, size, 512).expect("layout");
    let mut ex = LinuxExclusive::new(dev, sysfs(), lp.device.display().to_string());
    ex.commit_table(table).expect("commit table");
    let rescan = ex.rescan();
    drop(ex);

    let want: Vec<(u32, u64, u64)> = expected.iter().map(|s| (s.index, s.start, s.end)).collect();
    assert_eq!(
        read_table(&lp.device.display().to_string()),
        want,
        "the table on disk matches the layout"
    );

    let mut guard = OffsetLoops(Vec::new());
    let parts: Vec<PartitionRef> = match rescan {
        Ok(parts) => parts,
        Err(e) if e.to_string().contains("did not create") => {
            eprintln!(
                "note: this kernel did not create partition nodes ({e}); formatting through offset loop devices"
            );
            expected
                .iter()
                .map(|s| {
                    let offset = s.start * 512;
                    let size = (s.end - s.start + 1) * 512;
                    let node = attach_range(&lp.backing, offset, size);
                    let os_path = node.display().to_string();
                    guard.0.push(node);
                    PartitionRef {
                        index: s.index,
                        os_path,
                        offset,
                        size,
                    }
                })
                .collect()
        }
        Err(e) => panic!("rescan failed unexpectedly: {e}"),
    };

    let host = sysfs();
    for (part, spec) in parts.iter().zip(&table.partitions) {
        host.format_partition(part, spec.fs, &spec.label)
            .expect("format partition");
    }
    (parts.into_iter().map(|p| p.os_path).collect(), guard)
}

fn gpt(partitions: Vec<PartitionSpec>) -> TablePlan {
    TablePlan {
        style: TableStyle::Gpt,
        partitions,
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

#[test]
fn a_gpt_table_is_written_and_each_partition_is_formatted() {
    if !enabled() {
        return;
    }
    let lp = Loop::attach(64 * MIB, 0xAA);
    let table = gpt(vec![
        PartitionSpec {
            size: SizeSpec::Bytes(40 * MIB),
            fs: FsKind::Fat32,
            label: "BOOT".into(),
        },
        PartitionSpec {
            size: SizeSpec::Remaining,
            fs: FsKind::ExFat,
            label: "DATA".into(),
        },
    ]);
    let spans = layout(&table, 64 * MIB, 512).expect("layout");
    assert!(spans[0].end < spans[1].start, "partitions do not overlap");
    let (nodes, _guard) = partition_and_format(&lp, &table);

    assert_eq!(nodes.len(), 2);
    assert_eq!(blkid(&nodes[0], "TYPE"), "vfat");
    assert_eq!(blkid(&nodes[0], "LABEL"), "BOOT");
    assert_eq!(blkid(&nodes[1], "TYPE"), "exfat");
    assert_eq!(blkid(&nodes[1], "LABEL"), "DATA");
}

#[test]
fn each_supported_filesystem_can_be_made_on_a_partition() {
    if !enabled() {
        return;
    }
    let cases = [
        (FsKind::ExFat, "DATA", "exfat"),
        (FsKind::Fat32, "BOOT", "vfat"),
        (FsKind::Ntfs, "WORK", "ntfs"),
        (FsKind::Ext4, "root", "ext4"),
    ];
    for (fs, label, expected_type) in cases {
        let lp = Loop::attach(64 * MIB, 0);
        let table = gpt(vec![PartitionSpec {
            size: SizeSpec::Remaining,
            fs,
            label: label.into(),
        }]);
        let (nodes, _guard) = partition_and_format(&lp, &table);
        assert_eq!(
            blkid(&nodes[0], "TYPE"),
            expected_type,
            "filesystem for {fs:?}"
        );
        assert_eq!(blkid(&nodes[0], "LABEL"), label, "label for {fs:?}");
    }
}

#[test]
fn rescan_before_a_table_is_written_is_refused() {
    if !enabled() {
        return;
    }
    let lp = Loop::with_fill(0);
    let dev = BlockDevice::open(&lp.device).expect("open");
    let mut ex = LinuxExclusive::new(dev, sysfs(), lp.device.display().to_string());
    assert!(ex.rescan().is_err(), "no table committed yet");
}

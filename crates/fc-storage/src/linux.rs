//! Read-only Linux adapter. It reads sysfs and procfs and never opens a block
//! device for writing. Its destructive methods refuse with `Unsupported`, so
//! nothing can write through this adapter until the worker exists.
//!
//! Parsing is done by pure functions over text, which are tested on every
//! platform. The file-reading wrapper is covered by fixture trees.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::devname::parent_disk;
use crate::health::{Health, HealthSource};
use crate::model::{Disk, DiskId, DiskIdentity, Mount, Transport};
use crate::plan::FsKind;
use crate::platform::{ExclusiveDisk, OsError, PartitionRef, Platform};

/// Mount points that hold the running system. A disk that hosts any of these is never a candidate.
const SYSTEM_POINTS: &[&str] = &[
    "/",
    "/boot",
    "/boot/efi",
    "/usr",
    "/var",
    "/home",
    "/etc",
    "/opt",
    "/root",
    "/srv",
];

/// Device-name families this adapter enumerates as whole disks.
const DISK_PREFIXES: &[&str] = &["sd", "hd", "vd", "xvd", "nvme", "mmcblk"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    pub point: PathBuf,
    pub source: String,
}

/// Maps a `/dev` source to the whole-disk name it belongs to. Returns `None` for
/// anything it cannot map safely, such as device-mapper or ZFS, so the caller fails closed.
pub fn source_disk_name(source: &str) -> Option<String> {
    let name = source.strip_prefix("/dev/")?;
    if !DISK_PREFIXES.iter().any(|p| name.starts_with(p)) {
        return None;
    }
    Some(parent_disk(name).unwrap_or(name).to_owned())
}

/// Decodes the octal escapes procfs uses for spaces and control characters, such as `\040`.
pub fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' {
            if let Some(oct) = bytes.get(i + 1..i + 4) {
                if oct.iter().all(|b| (b'0'..=b'7').contains(b)) {
                    let v = (oct[0] - b'0') as u32 * 64
                        + (oct[1] - b'0') as u32 * 8
                        + (oct[2] - b'0') as u32;
                    if v <= 255 {
                        out.push(v as u8);
                        i += 4;
                        continue;
                    }
                }
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Parses `/proc/self/mountinfo`. The fields before ` - ` hold the mount point;
/// the fields after it hold the filesystem type and the source device.
pub fn parse_mountinfo(text: &str) -> Vec<MountEntry> {
    text.lines()
        .filter_map(|line| {
            let (pre, post) = line.split_once(" - ")?;
            let point = unescape(pre.split_whitespace().nth(4)?);
            let source = post.split_whitespace().nth(1)?.to_owned();
            Some(MountEntry {
                point: PathBuf::from(point),
                source,
            })
        })
        .collect()
}

/// Parses `/proc/swaps`, skipping the header row, and returns each swap device.
pub fn parse_swaps(text: &str) -> Vec<String> {
    text.lines()
        .skip(1)
        .filter_map(|l| l.split_whitespace().next())
        .map(unescape)
        .collect()
}

/// The mount point that contains `path`: the longest matching mount point wins.
pub fn mount_containing(mounts: &[MountEntry], path: &Path) -> Option<PathBuf> {
    mounts
        .iter()
        .filter(|m| path.starts_with(&m.point))
        .max_by_key(|m| m.point.components().count())
        .map(|m| m.point.clone())
}

/// The sysfs link target says how a disk is attached.
pub fn transport_from_sysfs_path(path: &str) -> Transport {
    if path.contains("/usb") {
        Transport::Usb
    } else if path.contains("/nvme") {
        Transport::Nvme
    } else if path.contains("/thunderbolt") {
        Transport::Thunderbolt
    } else if path.contains("/ata") {
        Transport::Sata
    } else if path.contains("/scsi") || path.contains("/sas") {
        Transport::Scsi
    } else {
        Transport::Unknown
    }
}

/// Computes the disks that host the running system. Returns `None` (fail closed)
/// when the answer cannot be determined: no root mount, or a system mount or swap
/// device that does not map to an enumerated disk.
pub fn system_set(
    disks: &[Disk],
    mounts: &[MountEntry],
    swaps: &[String],
    exe_mount: Option<&Path>,
) -> Option<HashSet<DiskId>> {
    let by_name: HashMap<String, DiskId> = disks
        .iter()
        .filter_map(|d| {
            d.os_path
                .strip_prefix("/dev/")
                .map(|n| (n.to_owned(), d.id.clone()))
        })
        .collect();
    let resolve = |source: &str| -> Option<DiskId> {
        let name = source_disk_name(source)?;
        by_name.get(&name).cloned()
    };

    let mut set = HashSet::new();
    let mut root_seen = false;
    for m in mounts {
        let point = m.point.to_string_lossy().into_owned();
        let is_system =
            SYSTEM_POINTS.contains(&point.as_str()) || exe_mount == Some(m.point.as_path());
        if !is_system {
            continue;
        }
        if m.point == Path::new("/") {
            root_seen = true;
        }
        set.insert(resolve(&m.source)?);
    }
    if !root_seen {
        return None;
    }
    for s in swaps {
        set.insert(resolve(s)?);
    }
    Some(set)
}

/// Reads SMART data by running `<program> --json -a <device>` with an argument list.
/// It is a read-only query. A missing tool, hidden SMART data or unreadable output
/// all come back as unavailable, never as a zero reading.
pub fn read_smart(program: &str, device: &Path) -> Health {
    let unavailable = Health {
        source: HealthSource::Unavailable,
        passed: None,
        temperature_c: None,
        power_on_hours: None,
        reallocated_sectors: None,
        percent_used: None,
    };
    let Ok(out) = Command::new(program)
        .arg("--json")
        .arg("-a")
        .arg(device)
        .output()
    else {
        return unavailable;
    };
    // smartctl sets exit-status bits for warnings and still prints its JSON, so the
    // output is parsed whatever the status.
    crate::health::parse_smartctl(&String::from_utf8_lossy(&out.stdout)).unwrap_or(unavailable)
}

fn read_trim(path: PathBuf) -> Option<String> {
    fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn read_u64(path: PathBuf) -> Option<u64> {
    read_trim(path)?.parse().ok()
}

fn read_text(path: PathBuf) -> Result<String, OsError> {
    fs::read_to_string(&path)
        .map_err(|e| OsError::Other(format!("cannot read {}: {e}", path.display())))
}

/// The Linux adapter. `root` is `/` in production and a fixture tree in tests.
#[derive(Debug, Clone)]
pub struct LinuxSysfs {
    root: PathBuf,
    /// Location of the running binary, so its mount counts as a system mount.
    exe: Option<PathBuf>,
}

impl LinuxSysfs {
    pub fn system() -> Self {
        Self {
            root: PathBuf::from("/"),
            exe: std::env::current_exe().ok(),
        }
    }

    pub fn at(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
            exe: None,
        }
    }

    /// The directory that holds `sys/` and `proc/`. It is `/` in production.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn mounts(&self) -> Result<Vec<MountEntry>, OsError> {
        read_text(self.root.join("proc/self/mountinfo")).map(|t| parse_mountinfo(&t))
    }

    fn swaps(&self) -> Result<Vec<String>, OsError> {
        read_text(self.root.join("proc/swaps")).map(|t| parse_swaps(&t))
    }

    fn disks(&self) -> Result<Vec<Disk>, OsError> {
        let mounts = self.mounts()?;
        let block = self.root.join("sys/block");
        let entries = fs::read_dir(&block)
            .map_err(|e| OsError::Other(format!("cannot read {}: {e}", block.display())))?;
        let mut names: Vec<String> = entries
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();

        let mut disks = Vec::new();
        for name in names {
            if !DISK_PREFIXES.iter().any(|p| name.starts_with(p)) {
                continue;
            }
            let dir = block.join(&name);
            // Partitions also appear here on some kernels. Only whole disks count.
            if dir.join("partition").exists() {
                continue;
            }
            // A disk without a size cannot be identified, so it is skipped.
            let Some(sectors) = read_u64(dir.join("size")) else {
                continue;
            };
            let logical_block = read_u64(dir.join("queue/logical_block_size"))
                .and_then(|v| u32::try_from(v).ok())
                .unwrap_or(512);
            let removable = read_trim(dir.join("removable")).as_deref() == Some("1");
            let vendor = read_trim(dir.join("device/vendor")).unwrap_or_default();
            let model_part = read_trim(dir.join("device/model")).unwrap_or_default();
            let model = format!("{vendor} {model_part}").trim().to_owned();
            // Many buses do not expose a serial here. Such disks stay read-only.
            let serial = read_trim(dir.join("device/serial"));
            let transport = fs::read_link(&dir)
                .map(|t| transport_from_sysfs_path(&t.to_string_lossy()))
                .unwrap_or(Transport::Unknown);
            let mounts_here = mounts
                .iter()
                .filter(|m| source_disk_name(&m.source).as_deref() == Some(name.as_str()))
                .map(|m| Mount {
                    point: m.point.clone(),
                    label: None,
                })
                .collect();
            disks.push(Disk::new(
                format!("/dev/{name}"),
                transport,
                removable,
                DiskIdentity {
                    serial,
                    model,
                    size_bytes: sectors.saturating_mul(512),
                },
                logical_block,
                mounts_here,
            ));
        }
        Ok(disks)
    }
}

impl Platform for LinuxSysfs {
    fn enumerate(&self) -> Result<Vec<Disk>, OsError> {
        self.disks()
    }

    fn system_disks(&self) -> Option<HashSet<DiskId>> {
        let disks = self.disks().ok()?;
        let mounts = self.mounts().ok()?;
        let swaps = self.swaps().ok()?;
        let exe_mount = self
            .exe
            .as_deref()
            .and_then(|e| mount_containing(&mounts, e));
        system_set(&disks, &mounts, &swaps, exe_mount.as_deref())
    }

    fn health(&self, disk: &Disk) -> Result<Health, OsError> {
        Ok(read_smart("smartctl", Path::new(&disk.os_path)))
    }

    #[cfg(all(feature = "storage-destructive", target_os = "linux"))]
    fn open_exclusive(&self, disk: &Disk) -> Result<Box<dyn ExclusiveDisk>, OsError> {
        use crate::linux_block::{BlockDevice, LinuxExclusive, map_open_error};
        let dev = BlockDevice::open(Path::new(&disk.os_path)).map_err(map_open_error)?;
        Ok(Box::new(LinuxExclusive::new(
            dev,
            self.clone(),
            disk.os_path.clone(),
        )))
    }

    #[cfg(not(all(feature = "storage-destructive", target_os = "linux")))]
    fn open_exclusive(&self, _disk: &Disk) -> Result<Box<dyn ExclusiveDisk>, OsError> {
        Err(OsError::Unsupported(
            "writing to block devices needs the storage-destructive feature",
        ))
    }

    #[cfg(all(feature = "storage-destructive", target_os = "linux"))]
    fn format_partition(
        &self,
        part: &PartitionRef,
        fs: FsKind,
        label: &str,
    ) -> Result<(), OsError> {
        use crate::linux_block::run_tool;
        use crate::linux_tools::mkfs;
        run_tool(&mkfs(fs, label, &part.os_path))
    }

    #[cfg(not(all(feature = "storage-destructive", target_os = "linux")))]
    fn format_partition(
        &self,
        _part: &PartitionRef,
        _fs: FsKind,
        _label: &str,
    ) -> Result<(), OsError> {
        Err(OsError::Unsupported(
            "formatting needs the storage-destructive feature",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn disk(name: &str, transport: Transport, removable: bool) -> Disk {
        Disk::new(
            format!("/dev/{name}"),
            transport,
            removable,
            DiskIdentity {
                serial: None,
                model: name.into(),
                size_bytes: 1 << 30,
            },
            512,
            Vec::new(),
        )
    }

    #[test]
    fn source_names_map_to_whole_disks() {
        assert_eq!(source_disk_name("/dev/sdb1").as_deref(), Some("sdb"));
        assert_eq!(source_disk_name("/dev/sdb").as_deref(), Some("sdb"));
        assert_eq!(
            source_disk_name("/dev/nvme0n1p2").as_deref(),
            Some("nvme0n1")
        );
    }

    #[test]
    fn unmappable_sources_are_none_so_callers_fail_closed() {
        assert_eq!(source_disk_name("/dev/mapper/vg-root"), None);
        assert_eq!(source_disk_name("/dev/dm-0"), None);
        assert_eq!(source_disk_name("tmpfs"), None);
        assert_eq!(source_disk_name("overlay"), None);
    }

    #[test]
    fn a_missing_smart_tool_is_unavailable_not_zero() {
        let h = read_smart("fcopy-no-such-smart-tool", Path::new("/dev/null"));
        assert_eq!(h.source, HealthSource::Unavailable);
        assert_eq!(h.passed, None);
        assert_eq!(h.temperature_c, None);
    }

    #[cfg(unix)]
    #[test]
    fn a_smart_report_from_the_tool_is_used() {
        use std::os::unix::fs::PermissionsExt;
        // A stand-in for smartctl that prints a healthy report, to check the wiring.
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("smartctl-stub");
        std::fs::write(
            &tool,
            "#!/bin/sh\necho '{\"smart_status\":{\"passed\":true},\"temperature\":{\"current\":31}}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();

        let h = read_smart(tool.to_str().unwrap(), Path::new("/dev/null"));
        assert_eq!(h.source, HealthSource::Smartctl);
        assert_eq!(h.passed, Some(true));
        assert_eq!(h.temperature_c, Some(31));
    }

    #[test]
    fn octal_escapes_are_decoded() {
        assert_eq!(unescape("/media/usb\\040stick"), "/media/usb stick");
        assert_eq!(unescape("a\\134b"), "a\\b");
        // Not a valid escape: left as written.
        assert_eq!(unescape("a\\9bc"), "a\\9bc");
        assert_eq!(unescape("plain"), "plain");
    }

    #[test]
    fn mountinfo_lines_are_parsed_around_the_separator() {
        let text = "25 1 8:1 / / rw,relatime shared:1 - ext4 /dev/sda1 rw\n\
                    30 25 8:17 / /media/usb\\040stick rw,nosuid - vfat /dev/sdb1 rw\n\
                    garbage line\n";
        let m = parse_mountinfo(text);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].point, PathBuf::from("/"));
        assert_eq!(m[0].source, "/dev/sda1");
        assert_eq!(m[1].point, PathBuf::from("/media/usb stick"));
    }

    #[test]
    fn swaps_skip_the_header() {
        let text = "Filename\tType\tSize\tUsed\tPriority\n/dev/sda2 partition 1 0 -2\n";
        assert_eq!(parse_swaps(text), vec!["/dev/sda2".to_string()]);
    }

    #[test]
    fn transport_is_read_from_the_sysfs_path() {
        assert_eq!(
            transport_from_sysfs_path("../devices/pci/usb2/2-1/block/sdb"),
            Transport::Usb
        );
        assert_eq!(
            transport_from_sysfs_path("../devices/pci/nvme/nvme0/nvme0n1"),
            Transport::Nvme
        );
        assert_eq!(
            transport_from_sysfs_path("../devices/pci/ata1/host0/block/sda"),
            Transport::Sata
        );
        assert_eq!(
            transport_from_sysfs_path("../devices/virtual/block/loop0"),
            Transport::Unknown
        );
    }

    #[test]
    fn longest_mount_point_contains_the_path() {
        let mounts = vec![
            MountEntry {
                point: PathBuf::from("/"),
                source: "/dev/sda1".into(),
            },
            MountEntry {
                point: PathBuf::from("/opt/app"),
                source: "/dev/sdc1".into(),
            },
        ];
        assert_eq!(
            mount_containing(&mounts, Path::new("/opt/app/bin/fcopy")),
            Some(PathBuf::from("/opt/app"))
        );
        assert_eq!(
            mount_containing(&mounts, Path::new("/usr/bin/fcopy")),
            Some(PathBuf::from("/"))
        );
    }

    #[test]
    fn system_set_contains_the_root_disk_and_swap() {
        let disks = vec![
            disk("sda", Transport::Sata, false),
            disk("sdb", Transport::Usb, true),
        ];
        let mounts = vec![MountEntry {
            point: PathBuf::from("/"),
            source: "/dev/sda1".into(),
        }];
        let set = system_set(&disks, &mounts, &[], None).unwrap();
        assert!(set.contains(&disks[0].id));
        assert!(!set.contains(&disks[1].id));

        let swapped = system_set(&disks, &mounts, &["/dev/sdb2".into()], None).unwrap();
        assert!(swapped.contains(&disks[1].id));
    }

    #[test]
    fn system_set_is_unknown_without_a_root_mount() {
        let disks = vec![disk("sda", Transport::Sata, false)];
        let mounts = vec![MountEntry {
            point: PathBuf::from("/media/x"),
            source: "/dev/sda1".into(),
        }];
        assert_eq!(system_set(&disks, &mounts, &[], None), None);
    }

    #[test]
    fn system_set_is_unknown_when_a_system_mount_cannot_be_mapped() {
        let disks = vec![disk("sda", Transport::Sata, false)];
        let mounts = vec![MountEntry {
            point: PathBuf::from("/"),
            source: "/dev/mapper/vg-root".into(),
        }];
        assert_eq!(system_set(&disks, &mounts, &[], None), None);
    }

    #[test]
    fn the_running_binary_counts_as_a_system_mount() {
        // The root disk is sda. The running binary lives on the USB disk sdc.
        let disks = vec![
            disk("sda", Transport::Sata, false),
            disk("sdc", Transport::Usb, true),
        ];
        let mounts = vec![
            MountEntry {
                point: PathBuf::from("/"),
                source: "/dev/sda1".into(),
            },
            MountEntry {
                point: PathBuf::from("/opt/app"),
                source: "/dev/sdc1".into(),
            },
        ];
        let without = system_set(&disks, &mounts, &[], None).unwrap();
        assert!(!without.contains(&disks[1].id));
        let with = system_set(&disks, &mounts, &[], Some(Path::new("/opt/app"))).unwrap();
        assert!(
            with.contains(&disks[1].id),
            "the disk that holds the running binary is a system disk"
        );
    }

    #[cfg(unix)]
    mod fixture {
        use super::*;
        use crate::safety::{Policy, Refusal, SafetyInputs, Verdict, evaluate};

        /// Builds a sysfs/procfs tree with a removable USB disk and an internal SATA disk.
        fn tree() -> tempfile::TempDir {
            let t = tempfile::tempdir().unwrap();
            let r = t.path();
            let usb = r.join("sys/devices/pci/usb2/2-1/block/sdb");
            let sata = r.join("sys/devices/pci/ata1/host0/block/sda");
            let loopdev = r.join("sys/devices/virtual/block/loop0");
            for d in [&usb, &sata, &loopdev] {
                fs::create_dir_all(d.join("queue")).unwrap();
            }
            fs::write(usb.join("size"), "2097152\n").unwrap();
            fs::write(usb.join("removable"), "1\n").unwrap();
            fs::write(usb.join("queue/logical_block_size"), "512\n").unwrap();
            fs::create_dir_all(usb.join("device")).unwrap();
            fs::write(usb.join("device/vendor"), "SanDisk \n").unwrap();
            fs::write(usb.join("device/model"), "Cruzer Blade\n").unwrap();

            fs::write(sata.join("size"), "2097152\n").unwrap();
            fs::write(sata.join("removable"), "0\n").unwrap();
            fs::create_dir_all(sata.join("device")).unwrap();
            fs::write(sata.join("device/serial"), "SATA-SN-1\n").unwrap();
            fs::write(sata.join("queue/logical_block_size"), "512\n").unwrap();

            fs::write(loopdev.join("size"), "0\n").unwrap();

            fs::create_dir_all(r.join("sys/block")).unwrap();
            link(&usb, &r.join("sys/block/sdb"));
            link(&sata, &r.join("sys/block/sda"));
            link(&loopdev, &r.join("sys/block/loop0"));

            fs::create_dir_all(r.join("proc/self")).unwrap();
            fs::write(
                r.join("proc/self/mountinfo"),
                "25 1 8:1 / / rw,relatime - ext4 /dev/sda1 rw\n\
                 30 25 8:17 / /media/usb\\040stick rw,nosuid - vfat /dev/sdb1 rw\n",
            )
            .unwrap();
            fs::create_dir_all(r.join("proc")).unwrap();
            fs::write(
                r.join("proc/swaps"),
                "Filename\tType\tSize\tUsed\tPriority\n",
            )
            .unwrap();
            t
        }

        fn link(target: &Path, at: &Path) {
            let rel = pathdiff(target, at.parent().unwrap());
            std::os::unix::fs::symlink(rel, at).unwrap();
        }

        /// The relative path from `from` to `to`, for symlinks that resemble real sysfs.
        fn pathdiff(to: &Path, from: &Path) -> PathBuf {
            let root = from
                .ancestors()
                .find(|a| to.starts_with(a))
                .unwrap()
                .to_path_buf();
            let up = from.strip_prefix(&root).unwrap().components().count();
            let mut rel = PathBuf::new();
            for _ in 0..up {
                rel.push("..");
            }
            rel.join(to.strip_prefix(&root).unwrap())
        }

        #[test]
        fn enumerates_whole_disks_with_attachment_and_mounts() {
            let t = tree();
            let adapter = LinuxSysfs::at(t.path());
            let disks = adapter.enumerate().unwrap();
            assert_eq!(disks.len(), 2, "loop devices are not disks: {disks:?}");

            let usb = disks.iter().find(|d| d.os_path == "/dev/sdb").unwrap();
            assert!(usb.removable);
            assert_eq!(usb.transport, Transport::Usb);
            assert_eq!(usb.identity.model, "SanDisk Cruzer Blade");
            assert_eq!(usb.identity.size_bytes, 1 << 30);
            assert_eq!(usb.identity.serial, None);
            assert_eq!(usb.mounts.len(), 1);
            assert_eq!(usb.mounts[0].point, PathBuf::from("/media/usb stick"));

            let sata = disks.iter().find(|d| d.os_path == "/dev/sda").unwrap();
            assert!(!sata.removable);
            assert_eq!(sata.transport, Transport::Sata);
        }

        #[test]
        fn system_set_is_the_internal_root_disk() {
            let t = tree();
            let adapter = LinuxSysfs::at(t.path());
            let disks = adapter.enumerate().unwrap();
            let set = adapter.system_disks().unwrap();
            let sata = disks.iter().find(|d| d.os_path == "/dev/sda").unwrap();
            let usb = disks.iter().find(|d| d.os_path == "/dev/sdb").unwrap();
            assert!(set.contains(&sata.id));
            assert!(!set.contains(&usb.id));
        }

        #[test]
        fn the_fixture_disks_are_refused_for_destruction() {
            let t = tree();
            let adapter = LinuxSysfs::at(t.path());
            let disks = adapter.enumerate().unwrap();
            let set = adapter.system_disks().unwrap();
            let policy = Policy::default();

            // The internal disk is the system disk.
            let sata = disks.iter().find(|d| d.os_path == "/dev/sda").unwrap();
            let v = evaluate(
                sata,
                &SafetyInputs {
                    system_disks: Some(&set),
                    protected_paths: &[],
                },
                policy,
            );
            assert_eq!(v, Verdict::ReadOnly(Refusal::SystemDisk));

            // The USB disk has no serial in this fixture, so it is read-only.
            let usb = disks.iter().find(|d| d.os_path == "/dev/sdb").unwrap();
            let v = evaluate(
                usb,
                &SafetyInputs {
                    system_disks: Some(&set),
                    protected_paths: &[],
                },
                policy,
            );
            assert_eq!(v, Verdict::ReadOnly(Refusal::NoSerial));
        }

        // Only in the default build. With the feature on, these methods act on the
        // device path in the fixture, which is the host's real /dev/sdb. A test must
        // never reach that path.
        #[cfg(not(feature = "storage-destructive"))]
        #[test]
        fn destructive_methods_refuse_in_this_adapter() {
            let t = tree();
            let adapter = LinuxSysfs::at(t.path());
            let disk = adapter.enumerate().unwrap().remove(0);
            assert!(matches!(
                adapter.open_exclusive(&disk),
                Err(OsError::Unsupported(_))
            ));
            let part = PartitionRef {
                index: 1,
                os_path: "/dev/sdb1".into(),
                offset: 0,
                size: 0,
            };
            assert!(matches!(
                adapter.format_partition(&part, FsKind::ExFat, "X"),
                Err(OsError::Unsupported(_))
            ));
        }

        #[test]
        fn a_missing_sysfs_tree_is_an_error_not_an_empty_list() {
            let t = tempfile::tempdir().unwrap();
            let adapter = LinuxSysfs::at(t.path());
            assert!(adapter.enumerate().is_err());
            assert_eq!(
                adapter.system_disks(),
                None,
                "unknown system set must fail closed"
            );
        }
    }
}

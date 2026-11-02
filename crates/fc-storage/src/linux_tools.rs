//! Pure builders for the external tools the Linux writer runs: `sgdisk` for the
//! partition table, and the `mkfs` family for filesystems. Nothing here runs a
//! program. The writer passes these argument lists straight to `Command`, never
//! through a shell, and the tests check the lists exactly.

use crate::plan::{FsKind, PlanError, SizeSpec, TablePlan, TableStyle, validate};

/// Bytes per sector. The Linux writer supports 512-byte logical blocks only.
pub const SECTOR: u64 = 512;
/// The first partition starts at 1 MiB, which keeps partitions aligned on flash and on
/// Advanced Format disks. The alignment unit is also 1 MiB.
pub const ALIGN_SECTORS: u64 = 2048;
/// The first usable LBA after the protective MBR and the primary GPT.
pub const FIRST_USABLE: u64 = 2048;
/// Sectors reserved at the end for the backup entries (32) and the backup header (1),
/// plus one more so that the last usable LBA is `total - GPT_TAIL`.
pub const GPT_TAIL: u64 = 34;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LayoutError {
    #[error(transparent)]
    Plan(#[from] PlanError),
    #[error("only GPT tables are written by the Linux writer")]
    MbrNotSupported,
    #[error("the Linux writer needs 512-byte logical blocks")]
    BlockSize,
    #[error("partition {0} does not fit on the device")]
    DoesNotFit(u32),
}

/// An inclusive range of LBAs for one partition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Span {
    pub index: u32,
    pub start: u64,
    /// Inclusive.
    pub end: u64,
}

fn round_up(x: u64, unit: u64) -> u64 {
    x.div_ceil(unit) * unit
}

fn align_down(x: u64, unit: u64) -> u64 {
    x / unit * unit
}

/// Lays the partitions out on a GPT disk. Every partition starts and ends on a
/// 1 MiB boundary, except the last one, which ends at the last aligned usable LBA.
pub fn layout(table: &TablePlan, device_bytes: u64, block: u32) -> Result<Vec<Span>, LayoutError> {
    if table.style != TableStyle::Gpt {
        return Err(LayoutError::MbrNotSupported);
    }
    if block != 512 {
        return Err(LayoutError::BlockSize);
    }
    validate(table, device_bytes)?;

    let total = device_bytes / SECTOR;
    let last_usable = total
        .checked_sub(GPT_TAIL)
        .ok_or(LayoutError::DoesNotFit(0))?;

    let mut spans = Vec::with_capacity(table.partitions.len());
    let mut next = FIRST_USABLE;
    for (i, spec) in table.partitions.iter().enumerate() {
        let index = i as u32 + 1;
        let end = match spec.size {
            SizeSpec::Bytes(bytes) => {
                let sectors = round_up(bytes.div_ceil(SECTOR), ALIGN_SECTORS);
                next.checked_add(sectors)
                    .and_then(|e| e.checked_sub(1))
                    .ok_or(LayoutError::DoesNotFit(index))?
            }
            SizeSpec::Remaining => align_down(last_usable + 1, ALIGN_SECTORS)
                .checked_sub(1)
                .ok_or(LayoutError::DoesNotFit(index))?,
        };
        if end > last_usable || end < next {
            return Err(LayoutError::DoesNotFit(index));
        }
        spans.push(Span {
            index,
            start: next,
            end,
        });
        next = round_up(end + 1, ALIGN_SECTORS);
    }
    Ok(spans)
}

/// The `sgdisk` arguments that create an empty GPT and the given partitions.
/// The device path is appended by the caller.
///
/// Partition names are not set. A name would have to pass through `--change-name`,
/// and a colon in a user label would split that argument. The filesystem label
/// carries the user-visible name.
pub fn sgdisk_args(spans: &[Span]) -> Vec<String> {
    let mut args = vec!["--clear".to_owned()];
    for span in spans {
        args.push(format!("--new={}:{}:{}", span.index, span.start, span.end));
        // Microsoft basic data. It is correct for FAT, exFAT and NTFS. For ext4 the
        // type is cosmetic: the kernel mounts by filesystem signature.
        args.push(format!("--typecode={}:0700", span.index));
    }
    args
}

/// A program and its arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tool {
    pub program: &'static str,
    pub args: Vec<String>,
}

/// The formatter for a filesystem, with the label and the target node.
/// The label has already passed `validate_label`, and the node is a path under /dev.
pub fn mkfs(fs: FsKind, label: &str, node: &str) -> Tool {
    let (program, args): (&'static str, Vec<&str>) = match fs {
        FsKind::ExFat => ("mkfs.exfat", vec!["-n", label, node]),
        FsKind::Fat32 => ("mkfs.fat", vec!["-F", "32", "-n", label, node]),
        FsKind::Ntfs => ("mkfs.ntfs", vec!["-Q", "-F", "-L", label, node]),
        FsKind::Ext4 => ("mkfs.ext4", vec!["-q", "-F", "-L", label, node]),
    };
    Tool {
        program,
        args: args.into_iter().map(str::to_owned).collect(),
    }
}

/// The device node for partition `index` of `disk`. Names that end in a digit
/// (`nvme0n1`, `loop0`, `mmcblk0`) take a `p` before the number. Others do not
/// (`sdb` becomes `sdb1`).
pub fn partition_path(disk: &str, index: u32) -> String {
    if disk.ends_with(|c: char| c.is_ascii_digit()) {
        format!("{disk}p{index}")
    } else {
        format!("{disk}{index}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plan::{FsKind, PartitionSpec, SizeSpec, TableStyle};

    const MIB: u64 = 1 << 20;

    fn gpt(parts: Vec<PartitionSpec>) -> TablePlan {
        TablePlan {
            style: TableStyle::Gpt,
            partitions: parts,
        }
    }

    fn part(size: SizeSpec, fs: FsKind, label: &str) -> PartitionSpec {
        PartitionSpec {
            size,
            fs,
            label: label.into(),
        }
    }

    #[test]
    fn two_partitions_are_aligned_and_the_last_one_fills_the_disk() {
        let table = gpt(vec![
            part(SizeSpec::Bytes(40 * MIB), FsKind::Fat32, "BOOT"),
            part(SizeSpec::Remaining, FsKind::ExFat, "DATA"),
        ]);
        let spans = layout(&table, 64 * MIB, 512).unwrap();
        // 40 MiB is 81920 sectors, and 1 MiB is 2048 sectors.
        assert_eq!(
            spans[0],
            Span {
                index: 1,
                start: 2048,
                end: 83967
            }
        );
        assert_eq!(spans[1].index, 2);
        assert_eq!(spans[1].start, 83968, "next start is 1 MiB aligned");
        // Total is 131072 sectors. The last usable LBA is 131038, and the end is
        // rounded down to a 1 MiB boundary.
        assert_eq!(spans[1].end, 129023);
        assert_eq!(spans[1].end + 1, 129024, "ends on a 1 MiB boundary");
    }

    #[test]
    fn a_fixed_size_rounds_up_to_the_alignment_unit() {
        let table = gpt(vec![part(SizeSpec::Bytes(1), FsKind::Ext4, "X")]);
        let spans = layout(&table, 64 * MIB, 512).unwrap();
        assert_eq!(spans[0].start, 2048);
        assert_eq!(spans[0].end - spans[0].start + 1, 2048, "one 1 MiB unit");
    }

    #[test]
    fn a_partition_that_does_not_fit_is_refused() {
        let table = gpt(vec![part(
            SizeSpec::Bytes(100 * MIB),
            FsKind::Fat32,
            "BOOT",
        )]);
        assert!(matches!(
            layout(&table, 64 * MIB, 512),
            Err(LayoutError::Plan(PlanError::ExceedsDevice))
        ));
    }

    #[test]
    fn remaining_space_on_a_tiny_device_is_refused_not_wrapped() {
        // 40 KiB of space: the usable area is smaller than the first aligned unit.
        let table = gpt(vec![part(SizeSpec::Remaining, FsKind::ExFat, "X")]);
        assert_eq!(
            layout(&table, 40 * 1024, 512),
            Err(LayoutError::DoesNotFit(1))
        );
    }

    #[test]
    fn mbr_and_non_512_blocks_are_refused() {
        let mut table = gpt(vec![part(SizeSpec::Remaining, FsKind::ExFat, "X")]);
        table.style = TableStyle::Mbr;
        assert_eq!(
            layout(&table, 64 * MIB, 512),
            Err(LayoutError::MbrNotSupported)
        );
        let gpt_table = gpt(vec![part(SizeSpec::Remaining, FsKind::ExFat, "X")]);
        assert_eq!(
            layout(&gpt_table, 64 * MIB, 4096),
            Err(LayoutError::BlockSize)
        );
    }

    #[test]
    fn sgdisk_arguments_describe_each_partition() {
        let spans = vec![
            Span {
                index: 1,
                start: 2048,
                end: 83967,
            },
            Span {
                index: 2,
                start: 83968,
                end: 129023,
            },
        ];
        assert_eq!(
            sgdisk_args(&spans),
            vec![
                "--clear",
                "--new=1:2048:83967",
                "--typecode=1:0700",
                "--new=2:83968:129023",
                "--typecode=2:0700",
            ]
        );
    }

    #[test]
    fn mkfs_arguments_are_fixed_per_filesystem() {
        assert_eq!(
            mkfs(FsKind::ExFat, "DATA", "/dev/loop0p2"),
            Tool {
                program: "mkfs.exfat",
                args: vec!["-n".into(), "DATA".into(), "/dev/loop0p2".into()]
            }
        );
        assert_eq!(
            mkfs(FsKind::Fat32, "BOOT", "/dev/sdb1"),
            Tool {
                program: "mkfs.fat",
                args: vec![
                    "-F".into(),
                    "32".into(),
                    "-n".into(),
                    "BOOT".into(),
                    "/dev/sdb1".into()
                ]
            }
        );
        assert_eq!(mkfs(FsKind::Ntfs, "WORK", "/dev/sdb2").program, "mkfs.ntfs");
        assert_eq!(
            mkfs(FsKind::Ext4, "root", "/dev/sdb3"),
            Tool {
                program: "mkfs.ext4",
                args: vec![
                    "-q".into(),
                    "-F".into(),
                    "-L".into(),
                    "root".into(),
                    "/dev/sdb3".into()
                ]
            }
        );
    }

    #[test]
    fn a_label_is_one_argument_even_when_it_looks_like_a_flag() {
        // The label is always the value of -n or -L, so a leading dash cannot become a flag.
        let tool = mkfs(FsKind::ExFat, "-rf", "/dev/sdb1");
        let n = tool.args.iter().position(|a| a == "-n").unwrap();
        assert_eq!(tool.args[n + 1], "-rf");
    }

    #[test]
    fn partition_names_follow_the_kernel_convention() {
        assert_eq!(partition_path("/dev/sdb", 1), "/dev/sdb1");
        assert_eq!(partition_path("/dev/nvme0n1", 2), "/dev/nvme0n1p2");
        assert_eq!(partition_path("/dev/loop0", 1), "/dev/loop0p1");
        assert_eq!(partition_path("/dev/mmcblk0", 3), "/dev/mmcblk0p3");
    }
}

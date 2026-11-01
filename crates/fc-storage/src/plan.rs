//! Partition-table and filesystem-label validation. Pure: everything that can
//! be checked before a write happens is checked here.

use std::ops::Range;

/// The filesystems the product offers. Per-OS availability is a platform concern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsKind {
    ExFat,
    Fat32,
    Ntfs,
    Ext4,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableStyle {
    /// Default. GPT allows up to 128 entries.
    Gpt,
    /// Legacy devices only. At most four primary entries and a 2 TiB limit.
    Mbr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeSpec {
    /// Uses all remaining space. Only the last partition may use it.
    Remaining,
    Bytes(u64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionSpec {
    pub size: SizeSpec,
    pub fs: FsKind,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TablePlan {
    pub style: TableStyle,
    pub partitions: Vec<PartitionSpec>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PlanError {
    #[error("at least one partition is required")]
    Empty,
    #[error("MBR allows at most 4 primary partitions")]
    TooManyMbrPartitions,
    #[error("GPT allows at most 128 partitions")]
    TooManyGptPartitions,
    #[error("only the last partition may use the remaining space")]
    RemainingNotLast,
    #[error("a partition cannot be zero bytes")]
    ZeroSize,
    #[error("partitions exceed the device size")]
    ExceedsDevice,
    #[error("label is too long for {0:?}")]
    LabelTooLong(FsKind),
    #[error("label contains characters not allowed for {0:?}")]
    LabelForbidden(FsKind),
}

/// Checks a label against the rules of the target filesystem.
/// FAT labels are uppercased by `mkfs.fat`; the UI should say so.
pub fn validate_label(fs: FsKind, label: &str) -> Result<(), PlanError> {
    let (max, forbidden): (usize, &str) = match fs {
        FsKind::Fat32 => (11, "*?/\\|.,;:+=[]<>\""),
        FsKind::ExFat => (15, "*?/\\|:<>\""),
        FsKind::Ntfs => (32, "*?/\\|:<>\""),
        FsKind::Ext4 => (16, "/"),
    };
    // ext4 counts bytes; the others count UTF-16 code units.
    let len = if fs == FsKind::Ext4 {
        label.len()
    } else {
        label.encode_utf16().count()
    };
    if len > max {
        return Err(PlanError::LabelTooLong(fs));
    }
    if label.chars().any(|c| forbidden.contains(c)) {
        return Err(PlanError::LabelForbidden(fs));
    }
    Ok(())
}

/// Validates the whole plan against the device size. Nothing is written
/// unless this returns `Ok`.
pub fn validate(plan: &TablePlan, device_bytes: u64) -> Result<(), PlanError> {
    if plan.partitions.is_empty() {
        return Err(PlanError::Empty);
    }
    match plan.style {
        TableStyle::Mbr if plan.partitions.len() > 4 => {
            return Err(PlanError::TooManyMbrPartitions);
        }
        TableStyle::Gpt if plan.partitions.len() > 128 => {
            return Err(PlanError::TooManyGptPartitions);
        }
        _ => {}
    }
    let last = plan.partitions.len() - 1;
    let mut fixed: u64 = 0;
    for (i, p) in plan.partitions.iter().enumerate() {
        match p.size {
            SizeSpec::Remaining if i != last => return Err(PlanError::RemainingNotLast),
            SizeSpec::Bytes(0) => return Err(PlanError::ZeroSize),
            SizeSpec::Bytes(b) => {
                fixed = fixed.checked_add(b).ok_or(PlanError::ExceedsDevice)?;
            }
            SizeSpec::Remaining => {}
        }
        validate_label(p.fs, &p.label)?;
    }
    if fixed > device_bytes {
        return Err(PlanError::ExceedsDevice);
    }
    Ok(())
}

/// Byte ranges to zero when removing a partition table: the protective MBR
/// and primary GPT at the start, and the backup GPT at the end. Each range
/// is clipped to the device, and overlapping ranges collapse into one.
// The function returns one range on small devices and two otherwise, so the
// single-element vec is intentional.
#[allow(clippy::single_range_in_vec_init)]
pub fn table_wipe_ranges(device_bytes: u64) -> Vec<Range<u64>> {
    const MIB: u64 = 1 << 20;
    let head = 0..MIB.min(device_bytes);
    let tail = device_bytes.saturating_sub(MIB)..device_bytes;
    if head.end >= tail.start {
        vec![0..device_bytes]
    } else {
        vec![head, tail]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn part(size: SizeSpec, fs: FsKind, label: &str) -> PartitionSpec {
        PartitionSpec {
            size,
            fs,
            label: label.into(),
        }
    }

    fn plan(style: TableStyle, parts: Vec<PartitionSpec>) -> TablePlan {
        TablePlan {
            style,
            partitions: parts,
        }
    }

    #[test]
    fn label_length_limits_per_filesystem() {
        assert!(validate_label(FsKind::Fat32, "ABCDEFGHIJK").is_ok());
        assert_eq!(
            validate_label(FsKind::Fat32, "ABCDEFGHIJKL"),
            Err(PlanError::LabelTooLong(FsKind::Fat32))
        );
        assert!(validate_label(FsKind::ExFat, &"a".repeat(15)).is_ok());
        assert!(validate_label(FsKind::ExFat, &"a".repeat(16)).is_err());
        assert!(validate_label(FsKind::Ntfs, &"a".repeat(32)).is_ok());
        assert!(validate_label(FsKind::Ntfs, &"a".repeat(33)).is_err());
        assert!(validate_label(FsKind::Ext4, &"a".repeat(16)).is_ok());
        assert!(validate_label(FsKind::Ext4, &"a".repeat(17)).is_err());
    }

    #[test]
    fn label_length_counts_utf16_units_for_windows_family() {
        // Two characters outside the BMP take four UTF-16 units.
        let emoji = "\u{1F600}\u{1F600}\u{1F600}\u{1F600}";
        assert!(validate_label(FsKind::ExFat, emoji).is_ok());
        assert!(validate_label(FsKind::ExFat, &format!("{emoji}{emoji}")).is_err());
    }

    #[test]
    fn forbidden_characters_are_rejected() {
        assert_eq!(
            validate_label(FsKind::ExFat, "a:b"),
            Err(PlanError::LabelForbidden(FsKind::ExFat))
        );
        assert!(validate_label(FsKind::Ext4, "a:b").is_ok());
        assert_eq!(
            validate_label(FsKind::Ext4, "a/b"),
            Err(PlanError::LabelForbidden(FsKind::Ext4))
        );
    }

    #[test]
    fn empty_plan_is_refused() {
        assert_eq!(
            validate(&plan(TableStyle::Gpt, vec![]), 1 << 30),
            Err(PlanError::Empty)
        );
    }

    #[test]
    fn mbr_limits_to_four_entries() {
        let five: Vec<_> = (0..5)
            .map(|_| part(SizeSpec::Bytes(1 << 20), FsKind::ExFat, "X"))
            .collect();
        assert_eq!(
            validate(&plan(TableStyle::Mbr, five), 1 << 40),
            Err(PlanError::TooManyMbrPartitions)
        );
    }

    #[test]
    fn gpt_limits_to_128_entries() {
        let many: Vec<_> = (0..129)
            .map(|_| part(SizeSpec::Bytes(1 << 20), FsKind::ExFat, "X"))
            .collect();
        assert_eq!(
            validate(&plan(TableStyle::Gpt, many), 1 << 40),
            Err(PlanError::TooManyGptPartitions)
        );
    }

    #[test]
    fn remaining_space_must_be_last() {
        let parts = vec![
            part(SizeSpec::Remaining, FsKind::ExFat, "A"),
            part(SizeSpec::Bytes(1 << 20), FsKind::ExFat, "B"),
        ];
        assert_eq!(
            validate(&plan(TableStyle::Gpt, parts), 1 << 30),
            Err(PlanError::RemainingNotLast)
        );
    }

    #[test]
    fn zero_sized_partition_is_refused() {
        let parts = vec![part(SizeSpec::Bytes(0), FsKind::ExFat, "A")];
        assert_eq!(
            validate(&plan(TableStyle::Gpt, parts), 1 << 30),
            Err(PlanError::ZeroSize)
        );
    }

    #[test]
    fn fixed_sizes_must_fit_the_device() {
        let parts = vec![
            part(SizeSpec::Bytes(600), FsKind::ExFat, "A"),
            part(SizeSpec::Bytes(500), FsKind::ExFat, "B"),
        ];
        assert_eq!(
            validate(&plan(TableStyle::Gpt, parts.clone()), 1000),
            Err(PlanError::ExceedsDevice)
        );
        assert!(validate(&plan(TableStyle::Gpt, parts), 1100).is_ok());
    }

    #[test]
    fn size_overflow_is_reported_as_exceeding_the_device() {
        let parts = vec![
            part(SizeSpec::Bytes(u64::MAX), FsKind::ExFat, "A"),
            part(SizeSpec::Bytes(1), FsKind::ExFat, "B"),
        ];
        assert_eq!(
            validate(&plan(TableStyle::Gpt, parts), u64::MAX),
            Err(PlanError::ExceedsDevice)
        );
    }

    #[test]
    fn valid_plan_with_remaining_tail_passes() {
        let parts = vec![
            part(SizeSpec::Bytes(1 << 30), FsKind::Fat32, "BOOT"),
            part(SizeSpec::Remaining, FsKind::ExFat, "DATA"),
        ];
        assert!(validate(&plan(TableStyle::Gpt, parts), 8 << 30).is_ok());
    }

    #[test]
    fn table_wipe_covers_head_and_tail_of_a_normal_device() {
        const MIB: u64 = 1 << 20;
        let size = 64 * MIB;
        assert_eq!(table_wipe_ranges(size), vec![0..MIB, size - MIB..size]);
    }

    #[test]
    fn table_wipe_collapses_for_small_devices() {
        const MIB: u64 = 1 << 20;
        assert_eq!(table_wipe_ranges(MIB), vec![0..MIB]);
        assert_eq!(table_wipe_ranges(MIB + 512), vec![0..MIB + 512]);
        assert_eq!(table_wipe_ranges(0), vec![0..0]);
    }
}

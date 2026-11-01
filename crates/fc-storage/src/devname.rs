//! Device-name parsing. Pure string logic, used on Linux where sysfs and
//! mountinfo name devices as `sdb1`, `nvme0n1p2`, `mmcblk0p1`, and so on.

/// Returns the parent disk of a partition name.
///
/// Only call this for names where sysfs has a `partition` file. The function
/// cannot tell a whole disk from a partition by name alone.
///
/// ```text
/// "sdb1"       -> Some("sdb")
/// "sdb"        -> None
/// "nvme0n1p2"  -> Some("nvme0n1")
/// "nvme0n1"    -> None
/// "mmcblk0p1"  -> Some("mmcblk0")
/// ```
pub fn parent_disk(part: &str) -> Option<&str> {
    if part.starts_with("nvme") || part.starts_with("mmcblk") {
        // These names end in "p<digits>" only for partitions. The disk part
        // must itself end in a digit, which excludes "nvme0n1" and similar.
        let idx = part.rfind('p')?;
        let (disk, rest) = part.split_at(idx);
        let digits = &rest[1..];
        let ok = !digits.is_empty()
            && digits.bytes().all(|b| b.is_ascii_digit())
            && disk.ends_with(|c: char| c.is_ascii_digit());
        return ok.then_some(disk);
    }
    let disk = part.trim_end_matches(|c: char| c.is_ascii_digit());
    (!disk.is_empty() && disk != part).then_some(disk)
}

#[cfg(test)]
mod tests {
    use super::parent_disk;

    #[test]
    fn scsi_style_partitions() {
        assert_eq!(parent_disk("sdb1"), Some("sdb"));
        assert_eq!(parent_disk("sdc12"), Some("sdc"));
    }

    #[test]
    fn whole_disks_have_no_parent() {
        assert_eq!(parent_disk("sdb"), None);
        assert_eq!(parent_disk("nvme0n1"), None);
        assert_eq!(parent_disk("mmcblk0"), None);
    }

    #[test]
    fn nvme_and_mmc_partitions() {
        assert_eq!(parent_disk("nvme0n1p2"), Some("nvme0n1"));
        assert_eq!(parent_disk("mmcblk0p1"), Some("mmcblk0"));
    }

    #[test]
    fn malformed_names_are_rejected() {
        assert_eq!(parent_disk(""), None);
        assert_eq!(parent_disk("1"), None);
        assert_eq!(parent_disk("nvme0np"), None);
        assert_eq!(parent_disk("nvme0n1pX"), None);
    }
}

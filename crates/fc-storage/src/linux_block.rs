//! Linux block-device writer. It opens a device for direct, synchronous I/O
//! with an exclusive claim, which fails with `EBUSY` if the device is mounted
//! or otherwise in use. It also writes the partition table and runs the
//! formatters.
//!
//! This is the only place in the crate that opens a device for writing, and it
//! exists only with the `storage-destructive` feature. It does not check what
//! device it was given. The caller is responsible for that, through the
//! safety rules in [`crate::safety`].

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::linux::LinuxSysfs;
use crate::linux_tools::{Span, Tool, layout, partition_path, sgdisk_args};
use crate::model::DiskIdentity;
use crate::plan::TablePlan;
use crate::platform::{ExclusiveDisk, OsError, PartitionRef, Platform};
use crate::wipe::RawIo;

/// `_IOR(0x12, 114, size_t)`: device size in bytes. `libc` does not export it.
const BLKGETSIZE64: libc::c_ulong =
    (2 << 30) | ((std::mem::size_of::<usize>() as libc::c_ulong) << 16) | (0x12 << 8) | 114;
/// `_IO(0x12, 104)`: logical block size in bytes.
const BLKSSZGET: libc::c_ulong = (0x12 << 8) | 104;
/// `_IO(0x12, 95)`: ask the kernel to re-read the partition table.
const BLKRRPART: libc::c_ulong = (0x12 << 8) | 95;

pub struct BlockDevice {
    file: File,
    path: PathBuf,
    size: u64,
    block: u32,
}

impl BlockDevice {
    /// Opens `path` for direct, synchronous writing with an exclusive claim.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_DIRECT | libc::O_SYNC | libc::O_EXCL)
            .open(path)?;
        let size = device_size(&file)?;
        let block = logical_block_size(&file)?;
        if block == 0 || size % u64::from(block) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "device size is not a multiple of its logical block size",
            ));
        }
        Ok(Self {
            file,
            path: path.to_path_buf(),
            size,
            block,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Asks the kernel to re-read the partition table, so that partition nodes appear.
    pub fn reread_partitions(&self) -> io::Result<()> {
        // SAFETY: BLKRRPART takes no argument and does not write through the pointer argument.
        let rc = unsafe { libc::ioctl(self.file.as_raw_fd(), BLKRRPART) };
        if rc == -1 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

fn device_size(file: &File) -> io::Result<u64> {
    let mut bytes: u64 = 0;
    // SAFETY: BLKGETSIZE64 writes one u64 through the pointer, which is valid for the call.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), BLKGETSIZE64, &mut bytes as *mut u64) };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(bytes)
}

fn logical_block_size(file: &File) -> io::Result<u32> {
    let mut size: libc::c_int = 0;
    // SAFETY: BLKSSZGET writes one int through the pointer, which is valid for the call.
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), BLKSSZGET, &mut size as *mut libc::c_int) };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    u32::try_from(size)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "negative block size"))
}

impl Read for BlockDevice {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl Write for BlockDevice {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

impl Seek for BlockDevice {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.file.seek(pos)
    }
}

impl RawIo for BlockDevice {
    fn size(&self) -> u64 {
        self.size
    }

    fn block_size(&self) -> u32 {
        self.block
    }

    fn flush_to_media(&mut self) -> io::Result<()> {
        self.file.sync_all()
    }
}

/// Maps an open failure to the error the pipeline understands.
pub fn map_open_error(e: io::Error) -> OsError {
    match e.raw_os_error() {
        Some(libc::EBUSY) => OsError::Busy,
        Some(libc::EPERM) | Some(libc::EACCES) => OsError::Permission,
        _ => OsError::Other(format!("cannot open device: {e}")),
    }
}

/// Runs an external tool with an argument list. There is no shell.
pub fn run_tool(tool: &Tool) -> Result<(), OsError> {
    let out = Command::new(tool.program)
        .args(&tool.args)
        .output()
        .map_err(|e| {
            if e.kind() == io::ErrorKind::NotFound {
                OsError::Other(format!("{} is not installed", tool.program))
            } else {
                OsError::Other(format!("cannot run {}: {e}", tool.program))
            }
        })?;
    if out.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        Err(OsError::Other(format!(
            "{} failed: {}",
            tool.program,
            stderr.trim()
        )))
    }
}

/// A whole disk held open for writing. The fields are the device, the sysfs view
/// used for identity and partition checks, and the layout committed by `commit_table`.
pub struct LinuxExclusive {
    dev: BlockDevice,
    sysfs: LinuxSysfs,
    os_path: String,
    spans: Vec<Span>,
}

impl LinuxExclusive {
    pub fn new(dev: BlockDevice, sysfs: LinuxSysfs, os_path: String) -> Self {
        Self {
            dev,
            sysfs,
            os_path,
            spans: Vec::new(),
        }
    }
}

impl RawIo for LinuxExclusive {
    fn size(&self) -> u64 {
        RawIo::size(&self.dev)
    }

    fn block_size(&self) -> u32 {
        RawIo::block_size(&self.dev)
    }

    fn flush_to_media(&mut self) -> io::Result<()> {
        RawIo::flush_to_media(&mut self.dev)
    }
}

impl Read for LinuxExclusive {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.dev.read(buf)
    }
}

impl Write for LinuxExclusive {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.dev.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.dev.flush()
    }
}

impl Seek for LinuxExclusive {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        self.dev.seek(pos)
    }
}

impl ExclusiveDisk for LinuxExclusive {
    fn identity(&self) -> Result<DiskIdentity, OsError> {
        self.sysfs
            .enumerate()?
            .into_iter()
            .find(|d| d.os_path == self.os_path)
            .map(|d| d.identity)
            .ok_or_else(|| OsError::Other("the device is no longer present".into()))
    }

    fn commit_table(&mut self, table: &TablePlan) -> Result<(), OsError> {
        let spans = layout(table, RawIo::size(&self.dev), RawIo::block_size(&self.dev))
            .map_err(|e| OsError::Other(e.to_string()))?;
        let mut args = sgdisk_args(&spans);
        args.push(self.os_path.clone());
        run_tool(&Tool {
            program: "sgdisk",
            args,
        })?;
        self.spans = spans;
        Ok(())
    }

    fn rescan(&mut self) -> Result<Vec<PartitionRef>, OsError> {
        if self.spans.is_empty() {
            return Err(OsError::Other("no partition table has been written".into()));
        }
        self.dev
            .reread_partitions()
            .map_err(|e| OsError::Other(format!("partition re-read failed: {e}")))?;

        let mut parts = Vec::with_capacity(self.spans.len());
        for span in &self.spans {
            let node = partition_path(&self.os_path, span.index);
            let name = Path::new(&node)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_owned();
            // The kernel must have created the node before anything formats it.
            if !self
                .sysfs
                .root()
                .join("sys/class/block")
                .join(&name)
                .exists()
            {
                return Err(OsError::Other(format!("the kernel did not create {node}")));
            }
            parts.push(PartitionRef {
                index: span.index,
                os_path: node,
                offset: span.start * 512,
                size: (span.end - span.start + 1) * 512,
            });
        }
        Ok(parts)
    }
}

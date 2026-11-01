//! Linux block-device writer. It opens a device for direct, synchronous I/O
//! with an exclusive claim, which fails with `EBUSY` if the device is mounted
//! or otherwise in use.
//!
//! This is the only place in the crate that opens a device for writing, and it
//! exists only with the `storage-destructive` feature. It does not check what
//! device it was given. The caller is responsible for that, through the
//! safety rules in [`crate::safety`].

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::wipe::RawIo;

/// `_IOR(0x12, 114, size_t)`: device size in bytes. `libc` does not export it.
const BLKGETSIZE64: libc::c_ulong =
    (2 << 30) | ((std::mem::size_of::<usize>() as libc::c_ulong) << 16) | (0x12 << 8) | 114;
/// `_IO(0x12, 104)`: logical block size in bytes.
const BLKSSZGET: libc::c_ulong = (0x12 << 8) | 104;

pub struct BlockDevice {
    file: File,
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
        Ok(Self { file, size, block })
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

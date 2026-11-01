//! In-memory platform for tests. It runs the whole pipeline without hardware and
//! lets a test control the system-disk set, simulate a disk being swapped between
//! enumeration and open, and make formatting fail.

use std::collections::HashSet;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::health::{Health, HealthSource};
use crate::model::{Disk, DiskId, DiskIdentity};
use crate::plan::{FsKind, SizeSpec, TablePlan};
use crate::platform::{ExclusiveDisk, OsError, PartitionRef, Platform};
use crate::wipe::RawIo;

type Bytes = Arc<Mutex<Vec<u8>>>;

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

struct Slot {
    disk: Disk,
    bytes: Bytes,
    /// When set, the handle reports this identity instead of the enumerated one.
    identity_on_open: Option<DiskIdentity>,
    committed: Option<TablePlan>,
    partitions: Vec<PartitionRef>,
}

#[derive(Default)]
struct State {
    slots: Vec<Slot>,
    system: Option<HashSet<DiskId>>,
    formatted: Vec<(String, FsKind, String)>,
    fail_format: bool,
}

/// Cloneable handle to one shared in-memory machine.
#[derive(Clone, Default)]
pub struct FakePlatform {
    state: Arc<Mutex<State>>,
}

impl FakePlatform {
    /// Adds a disk whose contents are `size_bytes` bytes, all set to `fill`.
    pub fn add_disk(&self, disk: Disk, fill: u8) -> DiskId {
        let id = disk.id.clone();
        let len = disk.identity.size_bytes as usize;
        locked(&self.state).slots.push(Slot {
            disk,
            bytes: Arc::new(Mutex::new(vec![fill; len])),
            identity_on_open: None,
            committed: None,
            partitions: Vec::new(),
        });
        id
    }

    pub fn set_system_disks(&self, set: Option<HashSet<DiskId>>) {
        locked(&self.state).system = set;
    }

    /// Simulates the device changing identity between enumeration and open.
    pub fn change_identity_on_open(&self, id: &DiskId, identity: DiskIdentity) {
        let mut st = locked(&self.state);
        if let Some(slot) = st.slots.iter_mut().find(|s| &s.disk.id == id) {
            slot.identity_on_open = Some(identity);
        }
    }

    pub fn fail_formats(&self, on: bool) {
        locked(&self.state).fail_format = on;
    }

    pub fn contents(&self, id: &DiskId) -> Vec<u8> {
        let st = locked(&self.state);
        st.slots
            .iter()
            .find(|s| &s.disk.id == id)
            .map(|s| locked(&s.bytes).clone())
            .unwrap_or_default()
    }

    pub fn committed_table(&self, id: &DiskId) -> Option<TablePlan> {
        let st = locked(&self.state);
        st.slots
            .iter()
            .find(|s| &s.disk.id == id)
            .and_then(|s| s.committed.clone())
    }

    pub fn formatted(&self) -> Vec<(String, FsKind, String)> {
        locked(&self.state).formatted.clone()
    }
}

impl Platform for FakePlatform {
    fn enumerate(&self) -> Result<Vec<Disk>, OsError> {
        Ok(locked(&self.state)
            .slots
            .iter()
            .map(|s| s.disk.clone())
            .collect())
    }

    fn system_disks(&self) -> Option<HashSet<DiskId>> {
        locked(&self.state).system.clone()
    }

    fn health(&self, _disk: &Disk) -> Result<Health, OsError> {
        Ok(Health {
            source: HealthSource::Unavailable,
            passed: None,
            temperature_c: None,
            power_on_hours: None,
            reallocated_sectors: None,
            percent_used: None,
        })
    }

    fn open_exclusive(&self, disk: &Disk) -> Result<Box<dyn ExclusiveDisk>, OsError> {
        let st = locked(&self.state);
        let slot = st
            .slots
            .iter()
            .find(|s| s.disk.id == disk.id)
            .ok_or_else(|| OsError::Other("no such disk".into()))?;
        Ok(Box::new(FakeHandle {
            platform: self.state.clone(),
            id: disk.id.clone(),
            bytes: slot.bytes.clone(),
            identity: slot
                .identity_on_open
                .clone()
                .unwrap_or_else(|| slot.disk.identity.clone()),
            block: slot.disk.logical_block,
            pos: 0,
        }))
    }

    fn format_partition(
        &self,
        part: &PartitionRef,
        fs: FsKind,
        label: &str,
    ) -> Result<(), OsError> {
        let mut st = locked(&self.state);
        if st.fail_format {
            return Err(OsError::Other("format failed".into()));
        }
        st.formatted
            .push((part.os_path.clone(), fs, label.to_owned()));
        Ok(())
    }
}

/// An open handle onto a fake disk. Reads and writes go straight to the shared bytes.
struct FakeHandle {
    platform: Arc<Mutex<State>>,
    id: DiskId,
    bytes: Bytes,
    identity: DiskIdentity,
    block: u32,
    pos: u64,
}

impl Read for FakeHandle {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let data = locked(&self.bytes);
        let start = self.pos as usize;
        if start >= data.len() {
            return Ok(0);
        }
        let n = buf.len().min(data.len() - start);
        buf[..n].copy_from_slice(&data[start..start + n]);
        self.pos += n as u64;
        Ok(n)
    }
}

impl Write for FakeHandle {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut data = locked(&self.bytes);
        let start = self.pos as usize;
        let end = start
            .checked_add(buf.len())
            .filter(|&e| e <= data.len())
            .ok_or_else(|| io::Error::new(io::ErrorKind::WriteZero, "write past end of device"))?;
        data[start..end].copy_from_slice(buf);
        self.pos = end as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Seek for FakeHandle {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let len = locked(&self.bytes).len() as i128;
        let target = match pos {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::End(d) => len + d as i128,
            SeekFrom::Current(d) => self.pos as i128 + d as i128,
        };
        let target = u64::try_from(target)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "seek before start"))?;
        self.pos = target;
        Ok(target)
    }
}

impl RawIo for FakeHandle {
    fn size(&self) -> u64 {
        locked(&self.bytes).len() as u64
    }

    fn block_size(&self) -> u32 {
        self.block
    }

    fn flush_to_media(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl ExclusiveDisk for FakeHandle {
    fn identity(&self) -> Result<DiskIdentity, OsError> {
        Ok(self.identity.clone())
    }

    fn commit_table(&mut self, table: &TablePlan) -> Result<(), OsError> {
        let device = RawIo::size(self);
        let mut offset = 1u64 << 20;
        let mut parts = Vec::new();
        for (i, spec) in table.partitions.iter().enumerate() {
            let size = match spec.size {
                SizeSpec::Bytes(b) => b,
                SizeSpec::Remaining => device.saturating_sub(offset),
            };
            parts.push(PartitionRef {
                index: i as u32 + 1,
                os_path: format!("{}-part{}", self.id, i + 1),
                offset,
                size,
            });
            offset = offset.saturating_add(size);
        }
        let mut st = locked(&self.platform);
        let slot = st
            .slots
            .iter_mut()
            .find(|s| s.disk.id == self.id)
            .ok_or_else(|| OsError::Other("no such disk".into()))?;
        slot.committed = Some(table.clone());
        slot.partitions = parts;
        Ok(())
    }

    fn rescan(&mut self) -> Result<Vec<PartitionRef>, OsError> {
        let st = locked(&self.platform);
        st.slots
            .iter()
            .find(|s| s.disk.id == self.id)
            .map(|s| s.partitions.clone())
            .ok_or_else(|| OsError::Other("no such disk".into()))
    }
}

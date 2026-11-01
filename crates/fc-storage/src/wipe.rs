//! Overwrite engine over a raw device. Generic over [`RawIo`], so the same
//! logic runs against a real block device (added in the OS layer) or an
//! in-memory buffer in tests.
//!
//! Direct-I/O rules: buffers are aligned to 4096 bytes, chunks are whole
//! multiples of the logical block, and every range must start and end on a
//! logical-block boundary. Anything else is refused before the first write.

use std::alloc::{Layout, alloc, dealloc};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};

use rand_chacha::ChaCha20Rng;
use rand_core::{RngCore, SeedableRng};
use serde::{Deserialize, Serialize};

/// Multiple of every supported logical block size (512 and 4096).
pub const CHUNK: usize = 8 << 20;
/// Buffer alignment for direct I/O.
pub const ALIGN: usize = 4096;

/// Byte-addressable device opened for direct I/O. Real handles implement this
/// with `O_DIRECT`, `FILE_FLAG_NO_BUFFERING`, or `F_NOCACHE`.
pub trait RawIo: Read + Write + Seek + Send {
    fn size(&self) -> u64;
    fn block_size(&self) -> u32;
    /// fsync, `FlushFileBuffers`, or `DKIOCSYNCHRONIZECACHE`.
    fn flush_to_media(&mut self) -> io::Result<()>;
}

/// One overwrite pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Pass {
    Fixed(u8),
    /// Cryptographically seeded stream. Verification regenerates it from the seed.
    Random,
}

/// Wipe methods the product offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WipeMethod {
    /// NIST SP 800-88 Rev.1 *Clear*: one overwrite with zeros. The default.
    Clear,
    /// One random overwrite, then verification.
    ClearRandom,
    /// Legacy DoD 5220.22-M sequence: 0x00, 0xFF, random, then verification.
    /// Provides no added assurance on modern media. Kept for compatibility.
    Dod3Pass,
}

impl WipeMethod {
    pub fn passes(self) -> &'static [Pass] {
        match self {
            WipeMethod::Clear => &[Pass::Fixed(0x00)],
            WipeMethod::ClearRandom => &[Pass::Random],
            WipeMethod::Dod3Pass => &[Pass::Fixed(0x00), Pass::Fixed(0xFF), Pass::Random],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Write,
    Verify,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub pass: u8,
    pub passes: u8,
    pub done: u64,
    pub total: u64,
    pub phase: Phase,
}

#[derive(Debug, thiserror::Error)]
pub enum WipeError {
    /// The disk is partly overwritten. The caller must record it as incomplete.
    #[error("cancelled during pass {pass}; the disk is partly overwritten")]
    Cancelled { pass: u8 },
    #[error("verification mismatch at byte offset {offset}")]
    Verify { offset: u64 },
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// Fresh 32-byte seed from the operating system's random source.
pub fn random_seed() -> Result<[u8; 32], getrandom::Error> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed)?;
    Ok(seed)
}

/// Heap buffer with guaranteed alignment for direct I/O.
pub struct AlignedBuf {
    ptr: *mut u8,
    layout: Layout,
}

// SAFETY: AlignedBuf exclusively owns its allocation and has no shared aliases.
unsafe impl Send for AlignedBuf {}

impl AlignedBuf {
    pub fn new(len: usize, align: usize) -> io::Result<Self> {
        let layout = Layout::from_size_align(len, align)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        // SAFETY: `layout` has non-zero size, which callers guarantee.
        let ptr = unsafe { alloc(layout) };
        if ptr.is_null() {
            return Err(io::ErrorKind::OutOfMemory.into());
        }
        Ok(Self { ptr, layout })
    }

    pub fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: `ptr` is valid for `layout.size()` bytes and uniquely borrowed here.
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.layout.size()) }
    }

    pub fn bytes(&self) -> &[u8] {
        // SAFETY: `ptr` is valid for `layout.size()` bytes; this is a shared borrow.
        unsafe { std::slice::from_raw_parts(self.ptr, self.layout.size()) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` with this exact layout.
        unsafe { dealloc(self.ptr, self.layout) }
    }
}

/// Deterministic per-pass byte stream. Each pass gets its own ChaCha stream
/// index, so the random passes of one run do not repeat each other.
struct PassStream {
    pass: Pass,
    rng: ChaCha20Rng,
}

impl PassStream {
    fn new(pass: Pass, seed: [u8; 32], index: u64) -> Self {
        let mut rng = ChaCha20Rng::from_seed(seed);
        rng.set_stream(index);
        Self { pass, rng }
    }

    fn fill(&mut self, out: &mut [u8]) {
        match self.pass {
            Pass::Fixed(b) => out.fill(b),
            Pass::Random => self.rng.fill_bytes(out),
        }
    }
}

fn check_cancel(cancel: &AtomicBool, pass: u8) -> Result<(), WipeError> {
    if cancel.load(Ordering::Relaxed) {
        Err(WipeError::Cancelled { pass })
    } else {
        Ok(())
    }
}

/// Runs the method over the whole device.
// A single range is the whole-device case; the slice API is what the lint dislikes.
#[allow(clippy::single_range_in_vec_init)]
pub fn run<D: RawIo + ?Sized>(
    dev: &mut D,
    method: WipeMethod,
    seed: [u8; 32],
    cancel: &AtomicBool,
    report: impl FnMut(Progress),
) -> Result<(), WipeError> {
    let total = dev.size();
    run_ranges(dev, &[0..total], method, seed, cancel, report)
}

/// Runs the method over the given byte ranges, in order. Each pass writes and
/// flushes, then the final pass is read back and compared. The seed is supplied
/// by the caller, so tests can reproduce a run exactly.
pub fn run_ranges<D: RawIo + ?Sized>(
    dev: &mut D,
    ranges: &[Range<u64>],
    method: WipeMethod,
    seed: [u8; 32],
    cancel: &AtomicBool,
    mut report: impl FnMut(Progress),
) -> Result<(), WipeError> {
    let block = dev.block_size() as u64;
    let size = dev.size();
    let aligned = block != 0 && (CHUNK as u64) % block == 0 && size % block == 0;
    let ranges_ok = ranges
        .iter()
        .all(|r| r.start <= r.end && r.end <= size && r.start % block == 0 && r.end % block == 0);
    if !aligned || !ranges_ok {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "device or range is incompatible with direct I/O",
        )
        .into());
    }

    let total: u64 = ranges.iter().map(|r| r.end - r.start).sum();
    let passes = method.passes();
    let count = passes.len() as u8;
    let mut buf = AlignedBuf::new(CHUNK, ALIGN.max(block as usize))?;

    for (idx, &pass) in passes.iter().enumerate() {
        let n = idx as u8 + 1;
        let mut stream = PassStream::new(pass, seed, idx as u64);
        let mut done = 0u64;
        for range in ranges {
            dev.seek(SeekFrom::Start(range.start))?;
            let mut off = range.start;
            while off < range.end {
                check_cancel(cancel, n)?;
                let len = (CHUNK as u64).min(range.end - off) as usize;
                stream.fill(&mut buf.bytes_mut()[..len]);
                dev.write_all(&buf.bytes()[..len])?;
                off += len as u64;
                done += len as u64;
                report(Progress {
                    pass: n,
                    passes: count,
                    done,
                    total,
                    phase: Phase::Write,
                });
            }
        }
        // A pass that did not reach the medium must not be followed by another.
        dev.flush_to_media()?;
    }

    // Only the final pass remains on the medium, so only it is verified.
    let last = passes.len() - 1;
    verify_ranges(
        dev,
        ranges,
        total,
        passes[last],
        seed,
        last as u64,
        count,
        &mut buf,
        cancel,
        &mut report,
    )
}

#[allow(clippy::too_many_arguments)]
fn verify_ranges<D: RawIo + ?Sized>(
    dev: &mut D,
    ranges: &[Range<u64>],
    total: u64,
    pass: Pass,
    seed: [u8; 32],
    index: u64,
    count: u8,
    buf: &mut AlignedBuf,
    cancel: &AtomicBool,
    report: &mut impl FnMut(Progress),
) -> Result<(), WipeError> {
    let mut expected = PassStream::new(pass, seed, index);
    let mut want = vec![0u8; CHUNK];
    let mut done = 0u64;
    for range in ranges {
        dev.seek(SeekFrom::Start(range.start))?;
        let mut off = range.start;
        while off < range.end {
            check_cancel(cancel, count)?;
            let len = (CHUNK as u64).min(range.end - off) as usize;
            dev.read_exact(&mut buf.bytes_mut()[..len])?;
            expected.fill(&mut want[..len]);
            if let Some(i) = buf.bytes()[..len]
                .iter()
                .zip(&want[..len])
                .position(|(a, b)| a != b)
            {
                return Err(WipeError::Verify {
                    offset: off + i as u64,
                });
            }
            off += len as u64;
            done += len as u64;
            report(Progress {
                pass: count,
                passes: count,
                done,
                total,
                phase: Phase::Verify,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::sync::atomic::AtomicBool;

    use super::*;

    const BLOCK: u32 = 512;
    /// Two full chunks plus a tail, so chunk boundaries and the short last chunk are exercised.
    const DEV_LEN: usize = 2 * CHUNK + 4096;
    const SEED: [u8; 32] = [7u8; 32];

    #[derive(Debug)]
    struct MemDevice {
        cursor: Cursor<Vec<u8>>,
        block: u32,
        flushes: usize,
        /// Test hook: flip one bit when this offset is read, to simulate a bad medium.
        flip_on_read: Option<u64>,
    }

    impl MemDevice {
        fn new(len: usize) -> Self {
            Self {
                cursor: Cursor::new(vec![0xAA; len]),
                block: BLOCK,
                flushes: 0,
                flip_on_read: None,
            }
        }

        fn contents(&self) -> &[u8] {
            self.cursor.get_ref()
        }
    }

    impl Read for MemDevice {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let start = self.cursor.position();
            let n = self.cursor.read(buf)?;
            if let Some(at) = self.flip_on_read {
                if (start..start + n as u64).contains(&at) {
                    buf[(at - start) as usize] ^= 0x01;
                }
            }
            Ok(n)
        }
    }

    impl Write for MemDevice {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.cursor.write(buf)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl Seek for MemDevice {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.cursor.seek(pos)
        }
    }

    impl RawIo for MemDevice {
        fn size(&self) -> u64 {
            self.cursor.get_ref().len() as u64
        }

        fn block_size(&self) -> u32 {
            self.block
        }

        fn flush_to_media(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    #[test]
    fn clear_overwrites_everything_with_zeros() {
        let mut dev = MemDevice::new(DEV_LEN);
        let cancel = AtomicBool::new(false);
        run(&mut dev, WipeMethod::Clear, SEED, &cancel, |_| {}).unwrap();
        assert!(dev.contents().iter().all(|&b| b == 0));
        assert_eq!(dev.flushes, 1);
    }

    #[test]
    fn ranges_leave_the_rest_of_the_device_untouched() {
        const MIB: u64 = 1 << 20;
        let mut dev = MemDevice::new(DEV_LEN);
        let size = dev.size();
        let cancel = AtomicBool::new(false);
        let ranges = [0..MIB, size - MIB..size];
        run_ranges(&mut dev, &ranges, WipeMethod::Clear, SEED, &cancel, |_| {}).unwrap();
        let mid = MIB as usize..(size - MIB) as usize;
        assert!(dev.contents()[..MIB as usize].iter().all(|&b| b == 0));
        assert!(dev.contents()[mid.clone()].iter().all(|&b| b == 0xAA));
        assert!(
            dev.contents()[(size - MIB) as usize..]
                .iter()
                .all(|&b| b == 0)
        );
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn unaligned_range_is_refused_before_any_write() {
        let mut dev = MemDevice::new(DEV_LEN);
        let cancel = AtomicBool::new(false);
        let err = run_ranges(
            &mut dev,
            &[1..512],
            WipeMethod::Clear,
            SEED,
            &cancel,
            |_| {},
        )
        .unwrap_err();
        assert!(matches!(err, WipeError::Io(e) if e.kind() == io::ErrorKind::InvalidInput));
        assert!(dev.contents().iter().all(|&b| b == 0xAA));
    }

    #[test]
    fn progress_reaches_the_full_size() {
        let mut dev = MemDevice::new(DEV_LEN);
        let cancel = AtomicBool::new(false);
        let mut last = None;
        run(&mut dev, WipeMethod::Clear, SEED, &cancel, |p| {
            last = Some(p)
        })
        .unwrap();
        let last = last.unwrap();
        assert_eq!(last.done, DEV_LEN as u64);
        assert_eq!(last.total, DEV_LEN as u64);
        assert_eq!(last.phase, Phase::Verify);
    }

    #[test]
    fn dod_leaves_the_regenerated_random_stream_on_the_medium() {
        let mut dev = MemDevice::new(DEV_LEN);
        let cancel = AtomicBool::new(false);
        run(&mut dev, WipeMethod::Dod3Pass, SEED, &cancel, |_| {}).unwrap();

        // Final pass is index 2 in the DoD sequence.
        let mut want = vec![0u8; DEV_LEN];
        PassStream::new(Pass::Random, SEED, 2).fill(&mut want);
        assert_eq!(dev.contents(), want.as_slice());
        assert_eq!(dev.flushes, 3);
    }

    #[test]
    fn random_pass_is_not_a_constant_fill() {
        let mut dev = MemDevice::new(DEV_LEN);
        let cancel = AtomicBool::new(false);
        run(&mut dev, WipeMethod::ClearRandom, SEED, &cancel, |_| {}).unwrap();
        assert!(dev.contents().iter().any(|&b| b != 0));
        assert!(dev.contents().iter().any(|&b| b != dev.contents()[0]));
    }

    #[test]
    fn verification_reports_the_exact_offset_of_a_bad_byte() {
        let mut dev = MemDevice::new(DEV_LEN);
        dev.flip_on_read = Some(CHUNK as u64 + 7);
        let cancel = AtomicBool::new(false);
        let err = run(&mut dev, WipeMethod::Clear, SEED, &cancel, |_| {}).unwrap_err();
        match err {
            WipeError::Verify { offset } => assert_eq!(offset, CHUNK as u64 + 7),
            other => panic!("expected verify error, got {other:?}"),
        }
    }

    #[test]
    fn cancel_before_start_writes_nothing_and_reports_pass_one() {
        let mut dev = MemDevice::new(DEV_LEN);
        let cancel = AtomicBool::new(true);
        let err = run(&mut dev, WipeMethod::Clear, SEED, &cancel, |_| {}).unwrap_err();
        assert!(matches!(err, WipeError::Cancelled { pass: 1 }));
        assert!(dev.contents().iter().all(|&b| b == 0xAA));
    }

    #[test]
    fn cancel_mid_run_stops_at_the_next_chunk() {
        let mut dev = MemDevice::new(DEV_LEN);
        let cancel = AtomicBool::new(false);
        let err = run(&mut dev, WipeMethod::Clear, SEED, &cancel, |p| {
            if p.phase == Phase::Write && p.done >= CHUNK as u64 {
                cancel.store(true, Ordering::Relaxed);
            }
        })
        .unwrap_err();
        assert!(matches!(err, WipeError::Cancelled { pass: 1 }));
        // The first chunk was overwritten; the rest was not.
        assert!(dev.contents()[..CHUNK].iter().all(|&b| b == 0));
        assert!(dev.contents()[CHUNK..].iter().all(|&b| b == 0xAA));
    }

    #[test]
    fn misaligned_size_is_refused_before_any_write() {
        let mut dev = MemDevice::new(1000);
        let cancel = AtomicBool::new(false);
        let err = run(&mut dev, WipeMethod::Clear, SEED, &cancel, |_| {}).unwrap_err();
        match err {
            WipeError::Io(e) => assert_eq!(e.kind(), io::ErrorKind::InvalidInput),
            other => panic!("expected invalid-input error, got {other:?}"),
        }
        assert!(dev.contents().iter().all(|&b| b == 0xAA));
    }

    #[test]
    fn same_seed_gives_the_same_bytes() {
        let cancel = AtomicBool::new(false);
        let mut a = MemDevice::new(DEV_LEN);
        let mut b = MemDevice::new(DEV_LEN);
        run(&mut a, WipeMethod::ClearRandom, SEED, &cancel, |_| {}).unwrap();
        run(&mut b, WipeMethod::ClearRandom, SEED, &cancel, |_| {}).unwrap();
        assert_eq!(a.contents(), b.contents());
    }

    #[test]
    fn works_through_a_trait_object() {
        let mut dev = MemDevice::new(DEV_LEN);
        let cancel = AtomicBool::new(false);
        let obj: &mut dyn RawIo = &mut dev;
        run(obj, WipeMethod::Clear, SEED, &cancel, |_| {}).unwrap();
        assert!(dev.contents().iter().all(|&b| b == 0));
    }
}

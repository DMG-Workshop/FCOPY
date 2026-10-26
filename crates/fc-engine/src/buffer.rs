//! Pre-allocated, page-aligned I/O buffers and a pool that recycles them.
//!
//! Why this exists:
//!
//! * **No per-chunk allocation.** The copy loop never calls the allocator; it
//!   just moves the same few buffers around a channel.
//! * **No per-file allocation.** Buffers are returned to the [`BufferPool`] when
//!   a file finishes, so a job with 100k small files allocates only during
//!   warm-up.
//! * **Alignment.** Buffers are 4 KiB aligned and sized in multiples of 4 KiB,
//!   which is what `O_DIRECT` / `FILE_FLAG_NO_BUFFERING` require. The engine
//!   uses that for uncached read-back verification on Windows today and it keeps
//!   the door open for direct-I/O copying later.

use std::alloc::{self, Layout};
use std::ops::{Deref, DerefMut};
use std::ptr::NonNull;
use std::slice;
use std::sync::{Mutex, PoisonError};

/// Alignment of every buffer (a page / the largest common sector size).
pub const BUFFER_ALIGN: usize = 4096;
/// Smallest allowed chunk size.
pub const MIN_CHUNK_SIZE: usize = 64 * 1024;
/// Largest allowed chunk size.
pub const MAX_CHUNK_SIZE: usize = 4 * 1024 * 1024;
/// Default chunk size: large enough to amortise syscalls, small enough to stay
/// friendly to L2 cache and read-ahead.
pub const DEFAULT_CHUNK_SIZE: usize = 1024 * 1024;

/// Clamp to `[MIN_CHUNK_SIZE, MAX_CHUNK_SIZE]` and round down to a multiple of
/// [`BUFFER_ALIGN`].
pub fn normalize_chunk_size(requested: usize) -> usize {
    let clamped = requested.clamp(MIN_CHUNK_SIZE, MAX_CHUNK_SIZE);
    clamped - clamped % BUFFER_ALIGN
}

/// A fixed-size, zero-initialised, 4 KiB-aligned heap buffer.
pub struct AlignedBuf {
    ptr: NonNull<u8>,
    len: usize,
}

// SAFETY: `AlignedBuf` uniquely owns its allocation (like `Box<[u8]>`); there is
// no interior aliasing, so moving it between threads or sharing `&AlignedBuf`
// is sound.
unsafe impl Send for AlignedBuf {}
unsafe impl Sync for AlignedBuf {}

impl AlignedBuf {
    /// Allocate `len` zeroed bytes. `len` must be non-zero.
    pub fn new(len: usize) -> Self {
        assert!(len > 0, "AlignedBuf length must be non-zero");
        let layout = Layout::from_size_align(len, BUFFER_ALIGN).expect("invalid buffer layout");
        // SAFETY: layout has non-zero size.
        let raw = unsafe { alloc::alloc_zeroed(layout) };
        let ptr = match NonNull::new(raw) {
            Some(p) => p,
            None => alloc::handle_alloc_error(layout),
        };
        Self { ptr, len }
    }
}

impl Deref for AlignedBuf {
    type Target = [u8];
    #[inline]
    fn deref(&self) -> &[u8] {
        // SAFETY: ptr is valid for `len` initialised (zeroed) bytes and uniquely owned.
        unsafe { slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
}

impl DerefMut for AlignedBuf {
    #[inline]
    fn deref_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, and `&mut self` guarantees exclusive access.
        unsafe { slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        let layout = Layout::from_size_align(self.len, BUFFER_ALIGN).expect("invalid buffer layout");
        // SAFETY: allocated in `new` with the identical layout.
        unsafe { alloc::dealloc(self.ptr.as_ptr(), layout) }
    }
}

impl std::fmt::Debug for AlignedBuf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "AlignedBuf({} bytes)", self.len)
    }
}

/// Thread-safe pool of equally sized buffers.
#[derive(Debug)]
pub struct BufferPool {
    chunk_size: usize,
    max_cached: usize,
    free: Mutex<Vec<AlignedBuf>>,
}

impl BufferPool {
    /// Create a pool and pre-allocate `prealloc` buffers.
    ///
    /// `max_cached` bounds how many idle buffers are kept (extra buffers handed
    /// back are freed), which bounds steady-state memory.
    pub fn new(chunk_size: usize, prealloc: usize, max_cached: usize) -> Self {
        let chunk_size = normalize_chunk_size(chunk_size);
        let max_cached = max_cached.max(prealloc);
        let free = (0..prealloc).map(|_| AlignedBuf::new(chunk_size)).collect();
        Self { chunk_size, max_cached, free: Mutex::new(free) }
    }

    pub fn chunk_size(&self) -> usize {
        self.chunk_size
    }

    /// Take `n` buffers, allocating only if the pool is short.
    pub fn take(&self, n: usize) -> Vec<AlignedBuf> {
        let mut out = Vec::with_capacity(n);
        {
            let mut free = self.free.lock().unwrap_or_else(PoisonError::into_inner);
            while out.len() < n {
                match free.pop() {
                    Some(b) => out.push(b),
                    None => break,
                }
            }
        }
        while out.len() < n {
            out.push(AlignedBuf::new(self.chunk_size));
        }
        out
    }

    /// Return buffers for reuse (extras beyond `max_cached` are dropped).
    pub fn give(&self, bufs: impl IntoIterator<Item = AlignedBuf>) {
        let mut free = self.free.lock().unwrap_or_else(PoisonError::into_inner);
        for b in bufs {
            if b.len() == self.chunk_size && free.len() < self.max_cached {
                free.push(b);
            }
        }
    }

    /// Idle buffers currently cached (diagnostics / tests).
    pub fn idle(&self) -> usize {
        self.free.lock().unwrap_or_else(PoisonError::into_inner).len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_size_is_clamped_and_aligned() {
        assert_eq!(normalize_chunk_size(1), MIN_CHUNK_SIZE);
        assert_eq!(normalize_chunk_size(usize::MAX), MAX_CHUNK_SIZE);
        assert_eq!(normalize_chunk_size(100_000) % BUFFER_ALIGN, 0);
        assert_eq!(normalize_chunk_size(DEFAULT_CHUNK_SIZE), DEFAULT_CHUNK_SIZE);
    }

    #[test]
    fn buffers_are_aligned_and_zeroed() {
        let b = AlignedBuf::new(MIN_CHUNK_SIZE);
        assert_eq!(b.as_ptr() as usize % BUFFER_ALIGN, 0);
        assert!(b.iter().all(|&x| x == 0));
    }

    #[test]
    fn pool_recycles_without_growing() {
        let pool = BufferPool::new(MIN_CHUNK_SIZE, 2, 4);
        assert_eq!(pool.idle(), 2);
        let bufs = pool.take(3); // one fresh allocation
        assert_eq!(pool.idle(), 0);
        pool.give(bufs);
        assert_eq!(pool.idle(), 3);
        let bufs = pool.take(3);
        assert_eq!(pool.idle(), 0);
        pool.give(bufs);
        assert_eq!(pool.idle(), 3);
    }

    #[test]
    fn pool_caps_idle_buffers() {
        let pool = BufferPool::new(MIN_CHUNK_SIZE, 0, 2);
        let bufs = pool.take(5);
        pool.give(bufs);
        assert_eq!(pool.idle(), 2);
    }
}

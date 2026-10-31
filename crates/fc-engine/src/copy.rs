//! The copy engine: pipelined buffered copy with on-the-fly hashing,
//! uncached read-back verification, atomic commit and error recovery.
//!
//! # Life of one file
//!
//! ```text
//!  stat source ─▶ conflict check ─▶ create dirs ─▶ free-space check
//!        │
//!        ▼                     (any step can fail ─▶ Retry / Skip / Cancel)
//!  open src, create "name.fcpart", preallocate
//!        │
//!        ▼
//!  ┌─────────┐  full buffers   ┌─────────┐
//!  │ reader  │ ──────────────▶ │ writer  │   reader: read + hash (source digest)
//!  │ thread  │ ◀────────────── │ (caller)│   writer: write + progress
//!  └─────────┘  empty buffers  └─────────┘   a fixed set of buffers circulates; no allocation
//!        │
//!        ▼
//!  source-unchanged check ─▶ set mtime ─▶ fsync ─▶ read-back verify (cache bypassed)
//!        │
//!        ▼
//!  set permissions ─▶ atomic rename "name.fcpart" → "name"
//! ```
//!
//! The destination name only ever appears once its content is complete and
//! (if requested) verified. A crash, cancel or skip leaves no half-written file
//! under the real name, and the `.fcpart` temp file is always cleaned up.
//!
//! Small files (one chunk) skip the thread hand-off and run the same
//! read→write steps inline.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, sync_channel};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use fs4::FileExt as _;

use crate::buffer::{normalize_chunk_size, AlignedBuf, BufferPool, DEFAULT_CHUNK_SIZE};
use crate::cache;
use crate::checksum::{Algorithm, Digest, Hasher};
use crate::control::JobControl;
use crate::error::{classify, CopyError, ErrorClass, Operation, Result};
use crate::progress::{Phase, Progress};
use crate::recovery::{Abort, ErrorHandler, Recovery, Step};
use crate::platform::{longpath, meta};
use crate::space::{self, DEFAULT_BLOCK_SIZE, DEFAULT_RESERVE};
use crate::throttle::RateLimiter;

/// Files smaller than this are not worth a `fallocate` syscall.
const PREALLOCATE_MIN: u64 = 1024 * 1024;
/// Suffix of the in-progress file.
pub const TEMP_SUFFIX: &str = ".fcpart";

/// How much integrity checking to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VerifyMode {
    /// No hashing. Fastest.
    #[default]
    Off,
    /// Hash the data as it is read from the source (essentially free) and
    /// report it in [`CopyReport::source_digest`] – useful for writing a
    /// checksum manifest. This proves nothing about what reached the
    /// destination.
    SourceHash,
    /// `SourceHash`, then re-read the destination *from disk* (page cache
    /// bypassed where the OS allows) and compare. Forces an `fsync`. The
    /// destination name is only created if the digests match.
    ReadBack,
}

#[derive(Debug, Clone)]
pub struct CopyOptions {
    /// I/O chunk size, clamped to 64 KiB – 4 MiB and 4 KiB-aligned. Default 1 MiB.
    pub chunk_size: usize,
    /// Buffers circulating between reader and writer (1 = no pipelining).
    /// Default 4.
    pub pipeline_depth: usize,
    pub verify: VerifyMode,
    pub algorithm: Algorithm,
    /// Replace an existing destination file.
    pub overwrite: bool,
    /// Optional conflict policy; queues attach a per-job interactive handler.
    pub conflicts: Option<Arc<crate::conflict::ConflictResolver>>,
    /// `fsync` the file (and its directory) before committing.
    pub fsync: bool,
    /// Copy access and modification times (plus creation time on Windows/macOS).
    pub preserve_mtime: bool,
    /// Unix: copy the `rwx` bits. Windows: copy the read-only flag.
    pub preserve_permissions: bool,
    /// Pre-check free space before copying each file. Batch runs check the
    /// whole plan once instead.
    pub check_space: bool,
    /// Bytes that must remain free after the copy.
    pub space_reserve: u64,
    pub create_parent_dirs: bool,
    /// Reserve the destination's space up front (`fallocate` etc.) so a full
    /// disk fails immediately rather than half-way, and fragmentation is lower.
    pub preallocate: bool,
    /// Copy extended attributes (Linux/macOS; includes POSIX ACLs stored as
    /// `system.posix_acl_*`). Unsupported destinations produce one warning.
    pub preserve_xattrs: bool,
    /// Windows: copy the file's DACL (access rules). Off by default, like
    /// Explorer: copies normally inherit the destination folder's permissions.
    /// On Unix, POSIX ACLs are carried by `preserve_xattrs`.
    pub preserve_acls: bool,
    /// Shared bandwidth cap. Share one `Arc` between jobs to cap them together.
    pub rate_limit: Option<Arc<RateLimiter>>,
}

impl Default for CopyOptions {
    fn default() -> Self {
        Self {
            chunk_size: DEFAULT_CHUNK_SIZE,
            pipeline_depth: 4,
            verify: VerifyMode::Off,
            algorithm: Algorithm::default(),
            overwrite: false,
            conflicts: None,
            fsync: false,
            preserve_mtime: true,
            preserve_permissions: true,
            check_space: true,
            space_reserve: DEFAULT_RESERVE,
            create_parent_dirs: true,
            preallocate: true,
            preserve_xattrs: true,
            preserve_acls: false,
            rate_limit: None,
        }
    }
}

/// What happened to one successfully copied file.
#[derive(Debug)]
pub struct CopyReport {
    pub src: PathBuf,
    pub dst: PathBuf,
    pub bytes: u64,
    /// Digest of the bytes read from the source (when `verify != Off`).
    pub source_digest: Option<Digest>,
    /// True if the destination was re-read and matched.
    pub verified: bool,
    /// Number of retries the recovery layer performed for this file.
    pub retries: u32,
    pub elapsed: Duration,
    /// Non-fatal problems, e.g. "could not preserve permissions".
    pub warnings: Vec<String>,
}

impl CopyReport {
    /// Average throughput of this file in bytes/second (copy + verify wall time).
    pub fn throughput_bps(&self) -> f64 {
        self.bytes as f64 / self.elapsed.as_secs_f64().max(1e-9)
    }
}

#[derive(Debug)]
pub enum CopyOutcome {
    Copied(CopyReport),
    /// The handler chose Skip. The partial output has been removed.
    Skipped { src: PathBuf, dst: PathBuf, error: CopyError },
}

/// The engine. Cheap to share between worker threads (`Arc<Copier>`); all
/// workers draw from the same buffer pool.
pub struct Copier {
    options: CopyOptions,
    pool: BufferPool,
    handler: Arc<dyn ErrorHandler>,
}

impl Copier {
    pub fn new(mut options: CopyOptions, handler: Arc<dyn ErrorHandler>) -> Self {
        options.chunk_size = normalize_chunk_size(options.chunk_size);
        options.pipeline_depth = options.pipeline_depth.clamp(1, 16);
        let depth = options.pipeline_depth;
        let pool = BufferPool::new(options.chunk_size, depth, depth * 4);
        Self { options, pool, handler }
    }

    pub fn options(&self) -> &CopyOptions {
        &self.options
    }

    pub fn handler(&self) -> &dyn ErrorHandler {
        &*self.handler
    }

    /// Copy one file.
    ///
    /// * `Ok(Copied)`  – `dst` now exists with the full (and verified) content.
    /// * `Ok(Skipped)` – the handler chose Skip; `dst` is untouched.
    /// * `Err(Cancelled)` – cancelled via `control` or by the handler.
    /// * `Err(other)`  – conflicts that need a decision *before* copying
    ///   (`DestinationExists`, `NotRegularFile`).
    pub fn copy_file(
        &self,
        src: &Path,
        dst: &Path,
        progress: &Progress,
        control: &JobControl,
    ) -> Result<CopyOutcome> {
        self.copy_file_impl(src, dst, progress, control, self.options.check_space, None)
    }

    pub(crate) fn copy_file_impl(
        &self,
        src: &Path,
        dst: &Path,
        progress: &Progress,
        control: &JobControl,
        check_space: bool,
        size_hint: Option<u64>,
    ) -> Result<CopyOutcome> {
        let started = Instant::now();
        // A skip pressed between files must not hit the next one.
        control.clear_skip();
        // Windows: make long paths (> MAX_PATH) safe for every API we call. No-op elsewhere.
        let (src, dst) = (&*longpath::to_extended(src), &*longpath::to_extended(dst));
        let rec = Recovery::new(&*self.handler, control);
        let mut size = size_hint.unwrap_or(0);

        match self.run_file(&rec, src, dst, progress, check_space, &mut size) {
            Ok(mut report) => {
                report.elapsed = started.elapsed();
                report.retries = rec.retries();
                progress.finish_file();
                Ok(CopyOutcome::Copied(report))
            }
            Err(Abort::Skip(error)) => {
                progress.skip_file(size);
                Ok(CopyOutcome::Skipped { src: src.to_path_buf(), dst: dst.to_path_buf(), error })
            }
            Err(Abort::Cancel) => Err(CopyError::Cancelled),
            Err(Abort::Fatal(e)) => {
                progress.skip_file(size);
                Err(e)
            }
        }
    }

    fn run_file(
        &self,
        rec: &Recovery<'_>,
        src: &Path,
        dst: &Path,
        progress: &Progress,
        check_space: bool,
        size_out: &mut u64,
    ) -> Step<CopyReport> {
        let opts = &self.options;
        check(rec.control)?;
        progress.begin_file(src, *size_out, opts.verify != VerifyMode::Off);

        let meta = rec.run(src, Operation::OpenSource, None, |_| fs::metadata(src))?;
        if !meta.is_file() {
            return Err(Abort::Fatal(CopyError::NotRegularFile(src.to_path_buf())));
        }
        let size = meta.len();
        *size_out = size;
        progress.set_file_size(size);

        let (destination, overwrite) = crate::conflict::local_destination(
            src, dst, opts.overwrite, opts.conflicts.as_deref(), rec.control,
        ).map_err(|e| match e {
            CopyError::SkippedByUser => Abort::Skip(e),
            CopyError::Cancelled => Abort::Cancel,
            _ => Abort::Fatal(e),
        })?;
        let dst = destination.as_path();

        if opts.create_parent_dirs {
            if let Some(parent) = dst.parent().filter(|p| !p.as_os_str().is_empty()) {
                rec.run(parent, Operation::CreateDir, None, |_| fs::create_dir_all(parent))?;
            }
        }

        // Disk-full is the classic "free some space, then Retry" case, so a
        // failed pre-check goes to the handler rather than aborting outright.
        if check_space {
            let needed = space::round_up(size, DEFAULT_BLOCK_SIZE);
            let mut attempt = 0;
            // Anything but "not enough space" (success, or a filesystem that cannot
            // report free space, e.g. some network mounts) proceeds; the writes decide.
            while let Err(e @ CopyError::InsufficientSpace { .. }) =
                space::check_space(dst, needed, opts.space_reserve)
            {
                attempt += 1;
                rec.ask_restart(dst, Operation::CheckSpace, e, attempt)?;
            }
        }

        let mut attempt = 0;
        loop {
            match self.attempt(rec, src, dst, &meta, size, progress, overwrite) {
                Ok(report) => return Ok(report),
                Err(AttemptError::Abort(a)) => return Err(a),
                Err(AttemptError::Restart(op, err)) => {
                    // Verification mismatch, unstable source or failed fsync:
                    // the only safe retry is a full re-copy.
                    attempt += 1;
                    progress.rewind_file();
                    rec.ask_restart(dst, op, err, attempt)?;
                }
            }
        }
    }

    /// One complete copy attempt of one file.
    #[allow(clippy::too_many_arguments)]
    fn attempt(
        &self,
        rec: &Recovery<'_>,
        src: &Path,
        dst: &Path,
        meta0: &fs::Metadata,
        size: u64,
        progress: &Progress,
        overwrite: bool,
    ) -> std::result::Result<CopyReport, AttemptError> {
        let opts = &self.options;
        let tmp = temp_path(dst);
        // Declared first so it is dropped last: handles close, then the temp file is removed.
        let mut guard = TempGuard::new(tmp.clone());
        let mut lease = Lease { pool: &self.pool, bufs: self.pool.take(opts.pipeline_depth) };
        let mut warnings = Vec::new();

        let mut src_file = rec.run(src, Operation::OpenSource, None, |_| File::open(src))?;
        cache::advise_sequential(&src_file);
        let mut dst_file = rec.run(&tmp, Operation::OpenDestination, None, |_| {
            OpenOptions::new().write(true).create(true).truncate(true).open(&tmp)
        })?;

        // Only a *full disk* is worth reacting to here; filesystems that don't
        // support preallocation (ZFS, NFS, ...) simply return an error we ignore.
        if opts.preallocate && size >= PREALLOCATE_MIN {
            rec.run(&tmp, Operation::Preallocate, None, |_| match dst_file.allocate(size) {
                Err(e) if classify(&e) == ErrorClass::DiskFull => Err(e),
                _ => Ok(()),
            })?;
        }

        // ---- copy ----
        progress.set_phase(Phase::Copying);
        let hasher = (opts.verify != VerifyMode::Off).then(|| Hasher::new(opts.algorithm));
        let (copied, source_digest) = self.transfer(
            rec, &mut src_file, &mut dst_file, src, &tmp, size, hasher, &mut lease, progress,
        )?;

        // Did the source change under us? (Compared on the open handle, so a
        // rename or replacement of the path can't fool the check.)
        if let Ok(end) = src_file.metadata() {
            if end.len() != meta0.len() || end.modified().ok() != meta0.modified().ok() {
                return Err(AttemptError::Restart(
                    Operation::Read,
                    CopyError::SourceChanged(src.to_path_buf()),
                ));
            }
        }

        // ---- finish the file ----
        progress.set_phase(Phase::Finalizing);
        if copied != size {
            // Preallocation extended the file (or the source reported a bogus size).
            rec.run(&tmp, Operation::Write, None, |_| dst_file.set_len(copied))?;
        }

        let want_readback = opts.verify == VerifyMode::ReadBack;
        if opts.fsync || want_readback {
            // A failed fsync must NOT be retried on the same descriptor: on
            // Linux the kernel may already have dropped the dirty pages and a
            // second fsync can report success for data that is gone. Re-copy.
            if let Err(e) = dst_file.sync_all() {
                return Err(AttemptError::Restart(Operation::Sync, CopyError::io(Operation::Sync, &tmp, e)));
            }
        }

        // ---- verify ----
        let mut verified = false;
        if want_readback {
            cache::drop_cache(&dst_file);
            progress.set_phase(Phase::Verifying);
            let expected = source_digest.as_ref().expect("hasher present when verifying");
            let actual = self.readback_digest(rec, &tmp, copied, &mut lease.bufs[0], progress)?;
            if &actual != expected {
                return Err(AttemptError::Restart(
                    Operation::Verify,
                    CopyError::VerifyMismatch {
                        path: dst.to_path_buf(),
                        algorithm: opts.algorithm,
                        expected: expected.to_hex(),
                        actual: actual.to_hex(),
                    },
                ));
            }
            verified = true;
        }

        // Timestamps after the read-back: reading can bump atime (relatime).
        if opts.preserve_mtime {
            if let Err(e) = meta::apply_times(&dst_file, meta0) {
                warnings.push(format!("could not preserve timestamps: {e}"));
            }
        }
        if opts.preserve_acls {
            if let Err(e) = meta::copy_dacl(src, &tmp) {
                warnings.push(format!("could not preserve ACL: {e}"));
            }
        }
        if opts.preserve_xattrs {
            warnings.extend(meta::copy_xattrs(src, &tmp));
        }
        // Permissions / attributes last: a read-only file can't be cleaned up on Windows.
        if opts.preserve_permissions {
            if let Err(e) = meta::apply_permissions(&dst_file, &tmp, meta0) {
                warnings.push(format!("could not preserve permissions/attributes: {e}"));
            }
        }
        drop(dst_file);
        drop(src_file);

        // ---- commit ----
        rec.run(dst, Operation::Finalize, None, |_| crate::conflict::commit_file(&tmp, dst, overwrite))?;
        guard.disarm();
        if opts.fsync {
            cache::sync_parent_dir(dst);
        }

        Ok(CopyReport {
            src: src.to_path_buf(),
            dst: dst.to_path_buf(),
            bytes: copied,
            source_digest,
            verified,
            retries: 0,
            elapsed: Duration::ZERO,
            warnings,
        })
    }

    /// Move `size` bytes from `src_file` to `dst_file`, hashing on the way.
    #[allow(clippy::too_many_arguments)]
    fn transfer<R: Read + Seek + Send, W: Write + Seek>(
        &self,
        rec: &Recovery<'_>,
        src_file: &mut R,
        dst_file: &mut W,
        src: &Path,
        tmp: &Path,
        size: u64,
        hasher: Option<Hasher>,
        lease: &mut Lease<'_>,
        progress: &Progress,
    ) -> Step<(u64, Option<Digest>)> {
        let chunk = self.pool.chunk_size() as u64;
        let mut reader = ChunkReader { file: src_file, path: src, offset: 0, hasher };
        let mut writer = ChunkWriter { file: dst_file, path: tmp, offset: 0 };

        if lease.bufs.len() < 2 || size <= chunk {
            // ---- inline: one buffer, no threads ----
            let mut buf = lease.bufs.pop().expect("at least one buffer");
            let result = (|| -> Step<()> {
                loop {
                    check(rec.control)?;
                    let n = reader.fill(rec, &mut buf)?;
                    if n == 0 {
                        return Ok(());
                    }
                    self.throttle(n, rec.control);
                    check(rec.control)?;
                    writer.write(rec, &buf[..n])?;
                    progress.add_copied(n as u64);
                    if n < buf.len() {
                        return Ok(()); // short fill == EOF
                    }
                }
            })();
            lease.bufs.push(buf);
            result?;
        } else {
            // ---- pipelined: reader thread  ⇄  writer (this thread) ----
            let depth = lease.bufs.len();
            let (full_tx, full_rx) = sync_channel::<(AlignedBuf, usize)>(depth);
            let (free_tx, free_rx) = channel::<AlignedBuf>();
            for b in lease.bufs.drain(..) {
                let _ = free_tx.send(b);
            }

            let (w_res, r_out) = thread::scope(|s| {
                let rdr = reader;
                let reader_thread = s.spawn(move || {
                    let mut reader = rdr;
                    let mut spare = Vec::new();
                    let res = (|| -> Step<()> {
                        loop {
                            check(rec.control)?;
                            // Blocks until the writer hands a buffer back: this is the back-pressure.
                            let Ok(mut buf) = free_rx.recv() else { return Ok(()) };
                            let n = match reader.fill(rec, &mut buf) {
                                Ok(n) => n,
                                Err(e) => {
                                    spare.push(buf);
                                    return Err(e);
                                }
                            };
                            if n == 0 {
                                spare.push(buf);
                                return Ok(());
                            }
                            let eof = n < buf.len();
                            if full_tx.send((buf, n)).is_err() {
                                return Ok(()); // writer gone
                            }
                            if eof {
                                return Ok(());
                            }
                        }
                    })();
                    // `full_tx` drops here, which tells the writer "no more data".
                    (res, reader, spare, free_rx)
                });

                let w_res = (|| -> Step<()> {
                    while let Ok((buf, n)) = full_rx.recv() {
                        let step = (|| -> Step<()> {
                            self.throttle(n, rec.control);
                            check(rec.control)?;
                            writer.write(rec, &buf[..n])?;
                            progress.add_copied(n as u64);
                            Ok(())
                        })();
                        let _ = free_tx.send(buf);
                        step?;
                    }
                    Ok(())
                })();
                // Unblock the reader if we are bailing out early.
                drop(full_rx);
                drop(free_tx);

                let r_out = reader_thread.join().unwrap_or_else(|p| std::panic::resume_unwind(p));
                (w_res, r_out)
            });

            let (r_res, rdr, spare, free_rx) = r_out;
            reader = rdr;
            lease.bufs.extend(spare);
            lease.bufs.extend(free_rx.try_iter());
            // The pool tops itself up if a failure lost a buffer in a channel.
            w_res?;
            r_res?;
        }

        debug_assert_eq!(reader.offset, writer.offset);
        Ok((writer.offset, reader.hasher.take().map(Hasher::finalize)))
    }

    #[inline]
    fn throttle(&self, n: usize, control: &JobControl) {
        if let Some(l) = &self.options.rate_limit {
            l.acquire(n as u64, control);
        }
    }

    /// Hash the destination temp file by reading it back, bypassing the cache.
    ///
    /// `len` is the number of bytes written. Knowing it matters on Windows: the
    /// handle is unbuffered, so every read must start at an aligned offset with
    /// an aligned length. Reads always ask for whole aligned slices and the loop
    /// stops at `len` instead of probing past a short tail read.
    fn readback_digest(
        &self,
        rec: &Recovery<'_>,
        path: &Path,
        len: u64,
        buf: &mut AlignedBuf,
        progress: &Progress,
    ) -> Step<Digest> {
        let mut file = rec.run(path, Operation::Verify, None, |_| cache::open_uncached(path))?;
        let mut hasher = Hasher::new(self.options.algorithm);
        let mut offset = 0u64;
        while offset < len {
            check(rec.control)?;
            let want = (len - offset).min(buf.len() as u64) as usize;
            let n = fill_from(rec, &mut file, path, Operation::Verify, offset, buf, want)?;
            if n == 0 {
                break; // truncated: the digest comparison reports it
            }
            let n = n.min(want);
            hasher.update(&buf[..n]);
            offset += n as u64;
            progress.add_verified(n as u64);
        }
        Ok(hasher.finalize())
    }
}

// ---------------------------------------------------------------------------
// Chunk readers / writers with positional retry
// ---------------------------------------------------------------------------

/// Read from `file` into `buf` until at least `want` bytes are in or EOF, with
/// recovery. Each read still offers the whole rest of `buf`, so the request
/// stays aligned for unbuffered handles. A retry first seeks back to the exact
/// byte that failed, so no data is skipped or repeated.
fn fill_from<R: Read + Seek>(
    rec: &Recovery<'_>,
    file: &mut R,
    path: &Path,
    op: Operation,
    offset: u64,
    buf: &mut [u8],
    want: usize,
) -> Step<usize> {
    let mut filled = 0usize;
    rec.run(path, op, Some(offset), |attempt| {
        if attempt > 0 {
            file.seek(SeekFrom::Start(offset + filled as u64))?;
        }
        while filled < want {
            match file.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    })?;
    Ok(filled)
}

struct ChunkReader<'a, R> {
    file: &'a mut R,
    path: &'a Path,
    offset: u64,
    hasher: Option<Hasher>,
}

impl<R: Read + Seek> ChunkReader<'_, R> {
    /// Returns bytes read; a result shorter than `buf.len()` means EOF.
    fn fill(&mut self, rec: &Recovery<'_>, buf: &mut [u8]) -> Step<usize> {
        let want = buf.len();
        let n = fill_from(rec, self.file, self.path, Operation::Read, self.offset, buf, want)?;
        if let Some(h) = &mut self.hasher {
            h.update(&buf[..n]);
        }
        self.offset += n as u64;
        Ok(n)
    }
}

struct ChunkWriter<'a, W> {
    file: &'a mut W,
    path: &'a Path,
    offset: u64,
}

impl<W: Write + Seek> ChunkWriter<'_, W> {
    fn write(&mut self, rec: &Recovery<'_>, data: &[u8]) -> Step<()> {
        let base = self.offset;
        let mut written = 0usize;
        rec.run(self.path, Operation::Write, Some(base), |attempt| {
            if attempt > 0 {
                self.file.seek(SeekFrom::Start(base + written as u64))?;
            }
            while written < data.len() {
                match self.file.write(&data[written..]) {
                    Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                    Ok(n) => written += n,
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        })?;
        self.offset += data.len() as u64;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

enum AttemptError {
    Abort(Abort),
    /// Re-copy the whole file if the handler says Retry.
    Restart(Operation, CopyError),
}

impl From<Abort> for AttemptError {
    fn from(a: Abort) -> Self {
        AttemptError::Abort(a)
    }
}

/// Chunk-boundary checkpoint: blocks while paused, aborts on cancel or skip.
fn check(control: &JobControl) -> Step<()> {
    control.checkpoint().map_err(|_| Abort::Cancel)?;
    if control.take_skip() {
        return Err(Abort::Skip(CopyError::SkippedByUser));
    }
    Ok(())
}

/// `dir/name` → `dir/name.fcpart`
fn temp_path(dst: &Path) -> PathBuf {
    let mut name: OsString = dst.file_name().map(|n| n.to_os_string()).unwrap_or_default();
    name.push(TEMP_SUFFIX);
    dst.with_file_name(name)
}

/// Removes the temp file on drop unless disarmed (success, or never created).
struct TempGuard {
    path: PathBuf,
    armed: bool,
}

impl TempGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Err(e) = fs::remove_file(&self.path) {
            if e.kind() == io::ErrorKind::NotFound {
                return;
            }
            // Windows refuses to delete read-only files.
            if let Ok(m) = fs::metadata(&self.path) {
                let mut p = m.permissions();
                #[allow(clippy::permissions_set_readonly_false)]
                p.set_readonly(false);
                let _ = fs::set_permissions(&self.path, p);
                let _ = fs::remove_file(&self.path);
            }
        }
    }
}

/// Buffers borrowed from the pool; returned on drop, whatever path we leave by.
struct Lease<'a> {
    pool: &'a BufferPool,
    bufs: Vec<AlignedBuf>,
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        self.pool.give(std::mem::take(&mut self.bufs));
    }
}

// ---------------------------------------------------------------------------
// Tests: fault injection against the real pipeline
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recovery::{AutoHandler, RetryPolicy};
    use std::io::Cursor;

    /// Deterministic pseudo-random bytes.
    fn data(len: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        (0..len)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                (x >> 24) as u8
            })
            .collect()
    }

    fn eio() -> io::Error {
        io::Error::from_raw_os_error(
            #[cfg(unix)]
            libc::EIO,
            #[cfg(not(unix))]
            23,
        )
    }

    /// Reader that returns short reads and fails scheduled calls, leaving the
    /// cursor in a *wrong* place afterwards (as a real failing device might).
    struct FlakyReader {
        inner: Cursor<Vec<u8>>,
        calls: usize,
        fail_calls: Vec<usize>,
        always_fail_at: Option<u64>,
        max_read: usize,
    }

    impl Read for FlakyReader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            self.calls += 1;
            if self.fail_calls.contains(&self.calls)
                || self.always_fail_at.is_some_and(|o| self.inner.position() >= o)
            {
                self.inner.set_position(self.inner.position() + 7); // garbage position
                return Err(eio());
            }
            let n = buf.len().min(self.max_read);
            self.inner.read(&mut buf[..n])
        }
    }
    impl Seek for FlakyReader {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    /// Writer with short writes and scheduled failures.
    struct FlakyWriter {
        inner: Cursor<Vec<u8>>,
        calls: usize,
        fail_calls: Vec<usize>,
        max_write: usize,
    }
    impl Write for FlakyWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            if self.fail_calls.contains(&self.calls) {
                self.inner.set_position(self.inner.position() + 5); // garbage position
                return Err(eio());
            }
            let n = buf.len().min(self.max_write);
            self.inner.write(&buf[..n])
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    impl Seek for FlakyWriter {
        fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
            self.inner.seek(pos)
        }
    }

    fn small_chunk_copier(depth: usize) -> Copier {
        let policy = RetryPolicy {
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(2),
            ..RetryPolicy::default()
        };
        Copier::new(
            CopyOptions {
                chunk_size: 64 * 1024,
                pipeline_depth: depth,
                verify: VerifyMode::SourceHash,
                algorithm: Algorithm::Sha256,
                ..Default::default()
            },
            Arc::new(AutoHandler::new(policy)),
        )
    }

    fn expected_digest(bytes: &[u8]) -> Digest {
        let mut h = Hasher::new(Algorithm::Sha256);
        h.update(bytes);
        h.finalize()
    }

    fn run_transfer(
        copier: &Copier,
        reader: &mut FlakyReader,
        writer: &mut FlakyWriter,
        size: u64,
    ) -> (Step<(u64, Option<Digest>)>, u32) {
        let control = JobControl::new();
        let rec = Recovery::new(copier.handler(), &control);
        let progress = Progress::new();
        let mut lease = Lease { pool: &copier.pool, bufs: copier.pool.take(copier.options.pipeline_depth) };
        let hasher = Some(Hasher::new(Algorithm::Sha256));
        let r = copier.transfer(
            &rec, reader, writer, Path::new("src"), Path::new("dst"), size, hasher, &mut lease, &progress,
        );
        (r, rec.retries())
    }

    fn flaky_pair(src: &[u8], read_fail: Vec<usize>, write_fail: Vec<usize>) -> (FlakyReader, FlakyWriter) {
        (
            FlakyReader {
                inner: Cursor::new(src.to_vec()),
                calls: 0,
                fail_calls: read_fail,
                always_fail_at: None,
                max_read: 10_000,
            },
            FlakyWriter { inner: Cursor::new(Vec::new()), calls: 0, fail_calls: write_fail, max_write: 30_000 },
        )
    }

    #[test]
    fn clean_pipelined_transfer_is_byte_exact() {
        let src = data(64 * 1024 * 17 + 1234, 1);
        let c = small_chunk_copier(4);
        let (mut r, mut w) = flaky_pair(&src, vec![], vec![]);
        let (res, retries) = run_transfer(&c, &mut r, &mut w, src.len() as u64);
        let (n, digest) = res.unwrap();
        assert_eq!(n as usize, src.len());
        assert_eq!(w.inner.into_inner(), src);
        assert_eq!(digest.unwrap(), expected_digest(&src));
        assert_eq!(retries, 0);
    }

    #[test]
    fn transient_read_errors_resume_at_the_exact_byte() {
        let src = data(64 * 1024 * 9 + 77, 2);
        let c = small_chunk_copier(4);
        // Fail several reads, including back-to-back pairs, in the middle of chunks.
        // (Two in a row is the most the default BadSector policy will retry.)
        let (mut r, mut w) = flaky_pair(&src, vec![3, 4, 20, 21, 60], vec![]);
        let (res, retries) = run_transfer(&c, &mut r, &mut w, src.len() as u64);
        let (_, digest) = res.expect("transient errors must be recovered");
        assert_eq!(w.inner.into_inner(), src, "no gaps, no duplicated bytes");
        assert_eq!(digest.unwrap(), expected_digest(&src), "hash covers each byte exactly once");
        assert!(retries >= 3, "retries={retries}");
    }

    #[test]
    fn transient_write_errors_resume_at_the_exact_byte() {
        let src = data(64 * 1024 * 9 + 77, 3);
        let c = small_chunk_copier(4);
        let (mut r, mut w) = flaky_pair(&src, vec![], vec![2, 5, 6, 30]);
        let (res, retries) = run_transfer(&c, &mut r, &mut w, src.len() as u64);
        res.expect("transient errors must be recovered");
        assert_eq!(w.inner.into_inner(), src);
        assert!(retries >= 3, "retries={retries}");
    }

    #[test]
    fn inline_path_recovers_too() {
        // depth 1 => no threads, same recovery
        let src = data(64 * 1024 * 3 + 5, 4);
        let c = small_chunk_copier(1);
        let (mut r, mut w) = flaky_pair(&src, vec![2, 9], vec![3]);
        let (res, _) = run_transfer(&c, &mut r, &mut w, src.len() as u64);
        res.unwrap();
        assert_eq!(w.inner.into_inner(), src);
    }

    #[test]
    fn persistent_bad_sector_ends_in_skip_not_hang_or_panic() {
        let src = data(64 * 1024 * 8, 5);
        let c = small_chunk_copier(4);
        let (mut r, mut w) = flaky_pair(&src, vec![], vec![]);
        r.always_fail_at = Some(64 * 1024 * 3); // unreadable from the 4th chunk on
        let (res, retries) = run_transfer(&c, &mut r, &mut w, src.len() as u64);
        match res {
            Err(Abort::Skip(CopyError::Io { op: Operation::Read, .. })) => {}
            other => panic!("expected Skip on Read, got {other:?}"),
        }
        assert_eq!(retries, 2, "BadSector policy allows exactly 2 retries");
    }

    #[test]
    fn buffers_survive_failures_and_return_to_the_pool() {
        let src = data(64 * 1024 * 8, 6);
        let c = small_chunk_copier(4);
        let before = c.pool.idle();
        for _ in 0..3 {
            let (mut r, mut w) = flaky_pair(&src, vec![], vec![]);
            r.always_fail_at = Some(64 * 1024 * 2);
            let _ = run_transfer(&c, &mut r, &mut w, src.len() as u64);
            let (mut r, mut w) = flaky_pair(&src, vec![], vec![]);
            let _ = run_transfer(&c, &mut r, &mut w, src.len() as u64);
        }
        assert!(c.pool.idle() >= before, "pool shrank: {} < {}", c.pool.idle(), before);
        assert!(c.pool.idle() <= 16, "pool grew without bound");
    }

    #[test]
    fn every_size_boundary_round_trips() {
        let chunk = 64 * 1024;
        for depth in [1, 2, 4] {
            let c = small_chunk_copier(depth);
            for len in [0, 1, chunk - 1, chunk, chunk + 1, 2 * chunk - 1, 2 * chunk, 2 * chunk + 1, 5 * chunk] {
                let src = data(len, 9 + len as u64);
                let (mut r, mut w) = flaky_pair(&src, vec![], vec![]);
                let (res, _) = run_transfer(&c, &mut r, &mut w, len as u64);
                let (n, d) = res.unwrap();
                assert_eq!(n as usize, len, "depth={depth} len={len}");
                assert_eq!(w.inner.into_inner(), src, "depth={depth} len={len}");
                assert_eq!(d.unwrap(), expected_digest(&src), "depth={depth} len={len}");
            }
        }
    }

    #[test]
    fn readback_detects_a_flipped_bit_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("dst.fcpart");
        let mut bytes = data(300_000, 11);
        fs::write(&path, &bytes).unwrap();
        let expected = expected_digest(&bytes);

        let c = small_chunk_copier(2);
        let control = JobControl::new();
        let rec = Recovery::new(c.handler(), &control);
        let progress = Progress::new();
        let mut buf = AlignedBuf::new(64 * 1024);

        let good = c.readback_digest(&rec, &path, 300_000, &mut buf, &progress).unwrap();
        assert_eq!(good, expected);
        assert_eq!(progress.snapshot().bytes_verified, 300_000);

        bytes[123_456] ^= 0x01; // single flipped bit
        fs::write(&path, &bytes).unwrap();
        let bad = c.readback_digest(&rec, &path, 300_000, &mut buf, &progress).unwrap();
        assert_ne!(bad, expected);
    }
}

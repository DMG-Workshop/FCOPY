//! # fc-engine – core copy + verify engine
//!
//! A UI-agnostic, dependency-light engine for fast, safe file copying.
//!
//! | Concern | Module |
//! |---------|--------|
//! | Pre-allocated, aligned, recycled I/O buffers | [`buffer`] |
//! | Pipelined copy loop, atomic commit, read-back verification | [`copy`] |
//! | MD5 / SHA-256 / XXH3 streaming checksums | [`checksum`] |
//! | Free-space pre-check, tree sizing | [`space`] |
//! | Retry / Skip / Cancel recovery policy | [`recovery`], [`error`] |
//! | Pause / resume / cancel | [`control`] |
//! | Throttled, verification-aware progress | [`progress`] |
//! | Plan + run a multi-file job | [`batch`] |
//! | Transfer queue, decisions, telemetry events | [`queue`] |
//! | Shared, live-adjustable speed limit | [`throttle`] |
//! | Long paths, Windows-safe names, metadata/ACLs | [`platform`] |
//! | Human-readable units, `10MB` parsing | [`units`] |
//!
//! ```no_run
//! use std::sync::Arc;
//! use fc_engine::*;
//!
//! let copier = Copier::new(
//!     CopyOptions { verify: VerifyMode::ReadBack, algorithm: Algorithm::Xxh3, ..Default::default() },
//!     Arc::new(AutoHandler::default()),            // retry transient errors, skip the rest
//! );
//! let progress = Progress::with_observer(std::time::Duration::from_millis(100), |s| {
//!     eprintln!("{:.1}%  {:?}", s.fraction * 100.0, s.phase);
//! });
//! let control = JobControl::new();                 // share via Arc to pause/cancel from a UI
//!
//! let plan = plan_copy(&["/data/photos".into()], "/mnt/backup".as_ref())?;
//! let report = copier.run_plan(&plan, "/mnt/backup".as_ref(), &progress, &control)?;
//! println!("{} files, {} skipped", report.copied.len(), report.skipped.len());
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

pub mod batch;
pub mod buffer;
mod cache;
pub mod checksum;
pub mod control;
pub mod conflict;
pub mod copy;
pub mod error;
pub mod manifest;
pub mod platform;
pub mod progress;
pub mod queue;
pub mod recovery;
pub mod space;
pub mod throttle;
pub mod units;

pub use batch::{
    plan_copy, plan_copy_with, plan_file_to, remove_empty_source_dirs, remove_moved_source, remove_sent_source, BatchReport, CopyItem,
    FileEvent, Plan, PlanOptions,
};
pub use buffer::{AlignedBuf, BufferPool, DEFAULT_CHUNK_SIZE, MAX_CHUNK_SIZE, MIN_CHUNK_SIZE};
pub use checksum::{hash_file, Algorithm, Digest, Hasher};
pub use control::{Cancelled, JobControl};
pub use copy::{Copier, CopyOptions, CopyOutcome, CopyReport, VerifyMode, SMALL_FILE, TEMP_SUFFIX};
pub use error::{classify, CopyError, ErrorClass, Operation, Result};
pub use progress::{LinkStats, Phase, Progress, Snapshot};
pub use recovery::{
    AutoHandler, CancelOnError, ErrorAction, ErrorContext, ErrorHandler, FnHandler, RetryPolicy,
    SkipOnError,
};
pub use queue::{
    BackendContext, DecisionRequest, ErrorPolicy, Event, JobId, JobInfo, JobState, JobSummary, Queue, QueueConfig,
    Target, TransferBackend, TransferRequest,
};
pub use space::{check_space, measure_tree, probe_space, SpaceReport, TreeStats};
pub use throttle::RateLimiter;

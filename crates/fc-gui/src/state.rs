//! UI state, separated from drawing so it can be unit-tested headlessly.
//!
//! The only way state changes is [`AppState::apply`], called with each engine
//! [`Event`] drained from the queue's channel at the start of a frame. Drawing
//! code reads this struct and sends *commands* to the `Queue`; it never
//! mutates state directly. That one-way flow means the UI can never disagree
//! with the engine for more than one frame.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Instant, SystemTime};

use fc_engine::manifest::ManifestEntry;
use fc_engine::{DecisionRequest, Event, JobId, JobState, JobSummary, Snapshot};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogKind {
    Skipped,
    Failed,
    Warning,
    JobFailed,
}

#[derive(Debug, Clone)]
pub struct LogEntry {
    pub at: SystemTime,
    pub job: JobId,
    pub kind: LogKind,
    pub path: Option<PathBuf>,
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct JobRow {
    pub id: JobId,
    pub label: String,
    pub state: JobState,
    pub paused: bool,
    pub fraction: f64,
    pub files_total: u64,
    pub bytes_total: u64,
    pub summary: Option<JobSummary>,
}

/// Everything the dashboard shows about the running job.
#[derive(Debug, Clone)]
pub struct ActiveView {
    pub job: JobId,
    pub telemetry: Option<Snapshot>,
    /// When `telemetry` arrived, to interpolate elapsed time between ticks.
    pub received: Instant,
    pub files_copied: u64,
    pub files_skipped: u64,
    pub files_failed: u64,
}

#[derive(Default)]
pub struct AppState {
    pub jobs: Vec<JobRow>,
    pub active: Option<ActiveView>,
    pub paused: bool,
    pub speed_limit: Option<u64>,
    pub log: Vec<LogEntry>,
    /// Errors added since the log drawer was last opened (badge count).
    pub unseen: usize,
    /// Questions waiting for the user, oldest first.
    pub decisions: VecDeque<DecisionRequest>,
    /// Display info for jobs (sources / target), filled when they are queued by the UI.
    pub details: std::collections::HashMap<JobId, (Vec<PathBuf>, String)>,
    /// Each job's copied files and their digests, for saving a checksum list.
    /// Only jobs that hashed (any verify mode but Off) have entries.
    pub checksums: std::collections::HashMap<JobId, Vec<ManifestEntry>>,
}

impl AppState {
    pub fn row_mut(&mut self, id: JobId) -> Option<&mut JobRow> {
        self.jobs.iter_mut().find(|r| r.id == id)
    }

    fn push_log(&mut self, job: JobId, kind: LogKind, path: Option<PathBuf>, message: String) {
        self.log.push(LogEntry {
            at: SystemTime::now(),
            job,
            kind,
            path,
            message,
        });
        self.unseen += 1;
        // Bound memory on huge jobs with many failures.
        if self.log.len() > 10_000 {
            self.log.drain(..1_000);
        }
    }

    fn active_mut(&mut self, job: JobId) -> &mut ActiveView {
        if self.active.as_ref().map(|a| a.job) != Some(job) {
            self.active = Some(ActiveView {
                job,
                telemetry: None,
                received: Instant::now(),
                files_copied: 0,
                files_skipped: 0,
                files_failed: 0,
            });
        }
        self.active.as_mut().unwrap()
    }

    /// Fold one engine event into the UI state.
    pub fn apply(&mut self, ev: Event) {
        match ev {
            Event::Queued { job, label } => self.jobs.push(JobRow {
                id: job,
                label,
                state: JobState::Pending,
                paused: false,
                fraction: 0.0,
                files_total: 0,
                bytes_total: 0,
                summary: None,
            }),
            Event::Started {
                job,
                files,
                bytes,
                warnings,
            } => {
                if let Some(r) = self.row_mut(job) {
                    r.state = JobState::Running;
                    r.files_total = files;
                    r.bytes_total = bytes;
                }
                self.active_mut(job);
                for w in warnings {
                    self.push_log(job, LogKind::Warning, None, w);
                }
            }
            Event::Progress { job, telemetry } => {
                if let Some(r) = self.row_mut(job) {
                    r.fraction = telemetry.fraction;
                }
                let a = self.active_mut(job);
                a.telemetry = Some(telemetry);
                a.received = Instant::now();
            }
            Event::FileCopied {
                job, dst, digest, warnings, ..
            } => {
                self.active_mut(job).files_copied += 1;
                if let Some(digest) = digest {
                    self.checksums.entry(job).or_default().push(ManifestEntry { path: dst.clone(), digest });
                }
                for w in warnings {
                    self.push_log(job, LogKind::Warning, Some(dst.clone()), w);
                }
            }
            Event::FileSkipped { job, src, reason } => {
                self.active_mut(job).files_skipped += 1;
                self.push_log(job, LogKind::Skipped, Some(src), reason);
            }
            Event::FileFailed { job, src, reason } => {
                self.active_mut(job).files_failed += 1;
                self.push_log(job, LogKind::Failed, Some(src), reason);
            }
            Event::Decision(d) => self.decisions.push_back(d),
            Event::Conflict { .. } => {}
            Event::Paused => self.paused = true,
            Event::Resumed => self.paused = false,
            Event::JobPaused { job } => {
                if let Some(r) = self.row_mut(job) {
                    r.paused = true;
                }
            }
            Event::JobResumed { job } => {
                if let Some(r) = self.row_mut(job) {
                    r.paused = false;
                }
            }
            Event::SpeedLimit(l) => self.speed_limit = l,
            Event::Finished {
                job,
                state,
                summary,
            } => {
                if let JobState::Failed(msg) = &state {
                    self.push_log(job, LogKind::JobFailed, None, msg.clone());
                }
                if let Some(r) = self.row_mut(job) {
                    if state == JobState::Completed {
                        r.fraction = 1.0;
                    }
                    r.state = state;
                    r.summary = Some(summary);
                }
                if self.active.as_ref().map(|a| a.job) == Some(job) {
                    self.active = None;
                }
                // Questions for a finished job can no longer be answered.
                self.decisions.retain(|d| d.job != job);
            }
            Event::Removed { job } => {
                self.jobs.retain(|r| r.id != job);
                self.details.remove(&job);
                self.checksums.remove(&job);
            }
            Event::Idle => self.active = None,
        }
    }

    /// Log entries that are errors rather than warnings.
    #[cfg(test)]
    pub fn error_count(&self) -> usize {
        self.log
            .iter()
            .filter(|l| l.kind != LogKind::Warning)
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fc_engine::{Phase, Snapshot};
    use std::time::Duration;

    fn snap(fraction: f64) -> Snapshot {
        Snapshot {
            phase: Phase::Copying,
            current_file: Some("a.bin".into()),
            file_bytes_done: 0,
            file_bytes_total: 0,
            files_done: 0,
            files_skipped: 0,
            files_total: 2,
            bytes_copied: 0,
            bytes_verified: 0,
            bytes_total: 100,
            fraction,
            rate_bps: 1e6,
            eta: Some(Duration::from_secs(3)),
            elapsed: Duration::from_secs(1),
            link: None,
        }
    }

    #[test]
    fn lifecycle_drives_rows_active_view_and_log() {
        let mut s = AppState::default();
        s.apply(Event::Queued {
            job: 1,
            label: "photos".into(),
        });
        assert_eq!(s.jobs[0].state, JobState::Pending);
        s.apply(Event::Started {
            job: 1,
            files: 2,
            bytes: 100,
            warnings: vec!["skipped symlink".into()],
        });
        assert!(s.active.is_some());
        s.apply(Event::Progress {
            job: 1,
            telemetry: snap(0.4),
        });
        assert_eq!(s.jobs[0].fraction, 0.4);
        s.apply(Event::FileSkipped {
            job: 1,
            src: "/x".into(),
            reason: "skipped by user".into(),
        });
        assert_eq!((s.log.len(), s.error_count(), s.unseen), (2, 1, 2));
        s.apply(Event::Finished {
            job: 1,
            state: JobState::CompletedWithErrors,
            summary: JobSummary::default(),
        });
        assert!(s.active.is_none());
        assert_eq!(s.jobs[0].state, JobState::CompletedWithErrors);
    }

    #[test]
    fn pause_limit_and_removal() {
        let mut s = AppState::default();
        s.apply(Event::Paused);
        s.apply(Event::SpeedLimit(Some(10_000_000)));
        assert!(s.paused);
        assert_eq!(s.speed_limit, Some(10_000_000));
        s.apply(Event::Resumed);
        assert!(!s.paused);
        s.apply(Event::Queued {
            job: 7,
            label: "x".into(),
        });
        s.apply(Event::Removed { job: 7 });
        assert!(s.jobs.is_empty());
    }

    #[test]
    fn hashed_files_are_kept_for_a_checksum_list_until_the_job_is_removed() {
        let mut s = AppState::default();
        s.apply(Event::Queued { job: 3, label: "photos".into() });
        let copied = |dst: &str, digest: Option<&str>| Event::FileCopied {
            job: 3,
            src: "/src/a".into(),
            dst: dst.into(),
            bytes: 1,
            digest: digest.map(str::to_string),
            verified: false,
            warnings: vec![],
        };
        s.apply(copied("/out/a", Some("aa")));
        s.apply(copied("/out/b", None)); // not hashed: nothing to list
        s.apply(copied("/out/c", Some("cc")));
        let kept: Vec<_> = s.checksums[&3].iter().map(|e| (e.path.clone(), e.digest.clone())).collect();
        assert_eq!(kept, vec![(PathBuf::from("/out/a"), "aa".to_string()), (PathBuf::from("/out/c"), "cc".to_string())]);
        s.apply(Event::Removed { job: 3 });
        assert!(!s.checksums.contains_key(&3));
    }

    #[test]
    fn failed_job_is_logged() {
        let mut s = AppState::default();
        s.apply(Event::Queued {
            job: 1,
            label: "x".into(),
        });
        s.apply(Event::Finished {
            job: 1,
            state: JobState::Failed("no space".into()),
            summary: JobSummary::default(),
        });
        assert_eq!(s.log[0].kind, LogKind::JobFailed);
    }
}

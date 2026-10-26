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
}

impl AppState {
    pub fn row_mut(&mut self, id: JobId) -> Option<&mut JobRow> {
        self.jobs.iter_mut().find(|r| r.id == id)
    }

    fn push_log(&mut self, job: JobId, kind: LogKind, path: Option<PathBuf>, message: String) {
        self.log.push(LogEntry { at: SystemTime::now(), job, kind, path, message });
        self.unseen += 1;
        // Bound memory on huge jobs with many failures.
        if self.log.len() > 10_000 {
            self.log.drain(..1_000);
        }
    }

    fn active_mut(&mut self, job: JobId) -> &mut ActiveView {
        if self.active.as_ref().map(|a| a.job) != Some(job) {
            self.active = Some(ActiveView { job, telemetry: None, received: Instant::now(), files_copied: 0, files_skipped: 0, files_failed: 0 });
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
                fraction: 0.0,
                files_total: 0,
                bytes_total: 0,
                summary: None,
            }),
            Event::Started { job, files, bytes, warnings } => {
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
            Event::FileCopied { job, dst, warnings, .. } => {
                self.active_mut(job).files_copied += 1;
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
            Event::Paused => self.paused = true,
            Event::Resumed => self.paused = false,
            Event::SpeedLimit(l) => self.speed_limit = l,
            Event::Finished { job, state, summary } => {
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
            }
            Event::Idle => self.active = None,
        }
    }

    pub fn error_count(&self) -> usize {
        self.log.iter().filter(|l| l.kind != LogKind::Warning).count()
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
        s.apply(Event::Queued { job: 1, label: "photos".into() });
        assert_eq!(s.jobs[0].state, JobState::Pending);
        s.apply(Event::Started { job: 1, files: 2, bytes: 100, warnings: vec!["skipped symlink".into()] });
        assert!(s.active.is_some());
        s.apply(Event::Progress { job: 1, telemetry: snap(0.4) });
        assert_eq!(s.jobs[0].fraction, 0.4);
        s.apply(Event::FileSkipped { job: 1, src: "/x".into(), reason: "skipped by user".into() });
        assert_eq!((s.log.len(), s.error_count(), s.unseen), (2, 1, 2));
        s.apply(Event::Finished { job: 1, state: JobState::CompletedWithErrors, summary: JobSummary::default() });
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
        s.apply(Event::Queued { job: 7, label: "x".into() });
        s.apply(Event::Removed { job: 7 });
        assert!(s.jobs.is_empty());
    }

    #[test]
    fn failed_job_is_logged() {
        let mut s = AppState::default();
        s.apply(Event::Queued { job: 1, label: "x".into() });
        s.apply(Event::Finished { job: 1, state: JobState::Failed("no space".into()), summary: JobSummary::default() });
        assert_eq!(s.log[0].kind, LogKind::JobFailed);
    }
}

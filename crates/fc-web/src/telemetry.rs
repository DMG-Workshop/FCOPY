//! Live telemetry over `/ws/telemetry`.
//!
//! One producer reads the queue's events and turns them into [`Frame`]s. It
//! runs on a 100 ms tick (10 Hz), so each job appears at most once per tick
//! however fast its progress events arrive, and it sends nothing when nothing
//! changed. Each frame is broadcast once. Every socket filters the frame for its
//! own user and serializes only what it may see.
//!
//! A socket that falls behind gets a full snapshot instead of silently missing
//! frames.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use serde::Serialize;
use tokio::sync::broadcast;

use fc_engine::{Event, JobId, JobInfo, JobState};

use crate::AppState;
use crate::auth::Principal;

/// Frame interval: 10 Hz.
pub const TICK: Duration = Duration::from_millis(100);

/// One job, as the dashboard shows it.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct JobDto {
    pub id: JobId,
    pub label: String,
    pub owner: Option<String>,
    /// Where the job writes: a local folder, or the paired PC's address.
    pub destination: String,
    pub remote: bool,
    /// `pending`, `running`, `paused`, `completed`, `completed_with_errors`,
    /// `failed`, or `cancelled`.
    pub state: String,
    pub paused: bool,
    /// 0–100.
    pub percent: f64,
    pub rate_bps: f64,
    pub eta_secs: Option<f64>,
    pub current_file: Option<String>,
    pub files_total: u64,
    pub files_done: u64,
    pub files_failed: u64,
    pub bytes_total: u64,
    pub bytes_copied: u64,
}

impl JobDto {
    pub fn from_info(info: &JobInfo, owner: Option<&str>) -> Self {
        let t = info.telemetry.as_ref();
        let state = match &info.state {
            JobState::Pending => "pending",
            JobState::Running if info.paused => "paused",
            JobState::Running => "running",
            JobState::Completed => "completed",
            JobState::CompletedWithErrors => "completed_with_errors",
            JobState::Failed(_) => "failed",
            JobState::Cancelled => "cancelled",
        };
        Self {
            id: info.id,
            label: info.label.clone(),
            owner: owner.map(str::to_string),
            destination: info.target.describe(),
            remote: matches!(info.target, fc_engine::Target::Backend(_)),
            state: state.to_string(),
            paused: info.paused,
            percent: t.map_or(0.0, |t| t.fraction * 100.0),
            rate_bps: t.map_or(0.0, |t| t.rate_bps),
            eta_secs: t.and_then(|t| t.eta).map(|d| d.as_secs_f64()),
            current_file: t.and_then(|t| t.current_file.as_ref()).map(|p| p.display().to_string()),
            files_total: info.files_total,
            files_done: info.files_copied,
            files_failed: info.files_failed,
            bytes_total: info.bytes_total,
            bytes_copied: info.bytes_copied,
        }
    }
}

/// A line for the error/skip log.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct LogLine {
    pub job: JobId,
    /// `skipped`, `failed`, or `job_failed`.
    pub kind: &'static str,
    pub message: String,
}

/// One tick's worth of changes.
#[derive(Debug, Clone, Serialize, Default, PartialEq)]
pub struct Frame {
    pub jobs: Vec<JobDto>,
    pub removed: Vec<JobId>,
    pub logs: Vec<LogLine>,
}

/// Start the producer: a blocking thread drains the queue's (std) receiver
/// into an async channel, and a task turns those events into frames.
pub fn spawn_producer(state: Arc<AppState>, events: std::sync::mpsc::Receiver<Event>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    std::thread::Builder::new()
        .name("fc-web-events".into())
        .spawn(move || {
            while let Ok(ev) = events.recv() {
                if tx.send(ev).is_err() {
                    break;
                }
            }
        })
        .expect("spawn event reader");

    tokio::spawn(async move {
        let mut tick = tokio::time::interval(TICK);
        let mut dirty = BTreeSet::new();
        let mut removed = Vec::new();
        let mut logs = Vec::new();
        loop {
            tokio::select! {
                ev = rx.recv() => match ev {
                    Some(ev) => collect(ev, &mut dirty, &mut removed, &mut logs),
                    None => break,
                },
                _ = tick.tick() => {
                    if dirty.is_empty() && removed.is_empty() && logs.is_empty() {
                        continue;
                    }
                    let frame = build_frame(&state, &dirty, std::mem::take(&mut removed), std::mem::take(&mut logs));
                    dirty.clear();
                    // No subscribers is fine: nobody is watching.
                    let _ = state.telemetry.send(Arc::new(frame));
                }
            }
        }
    });
}

/// Fold one queue event into the pending tick.
fn collect(ev: Event, dirty: &mut BTreeSet<JobId>, removed: &mut Vec<JobId>, logs: &mut Vec<LogLine>) {
    match ev {
        Event::Queued { job, .. }
        | Event::Started { job, .. }
        | Event::Progress { job, .. }
        | Event::FileCopied { job, .. }
        | Event::JobPaused { job }
        | Event::JobResumed { job } => {
            dirty.insert(job);
        }
        Event::FileSkipped { job, src, reason } => {
            dirty.insert(job);
            logs.push(LogLine { job, kind: "skipped", message: format!("{}: {reason}", src.display()) });
        }
        Event::FileFailed { job, src, reason } => {
            dirty.insert(job);
            logs.push(LogLine { job, kind: "failed", message: format!("{}: {reason}", src.display()) });
        }
        Event::Finished { job, state, .. } => {
            dirty.insert(job);
            if let JobState::Failed(msg) = state {
                logs.push(LogLine { job, kind: "job_failed", message: msg });
            }
        }
        Event::Removed { job } => {
            dirty.remove(&job);
            removed.push(job);
        }
        // A `Decision` is dropped here: the web policy is automatic, so the
        // queue never asks, and dropping a request counts as Skip.
        Event::Decision(_) | Event::Paused | Event::Resumed | Event::SpeedLimit(_) | Event::Idle => {}
    }
}

fn build_frame(state: &AppState, dirty: &BTreeSet<JobId>, removed: Vec<JobId>, logs: Vec<LogLine>) -> Frame {
    let owners = state.owners.read().unwrap_or_else(|e| e.into_inner());
    let jobs = dirty
        .iter()
        .filter_map(|id| state.queue.job(*id))
        .map(|info| {
            let owner = owners.get(&info.id).map(String::as_str);
            JobDto::from_info(&info, owner)
        })
        .collect();
    Frame { jobs, removed, logs }
}

/// What one user may see of a frame. Returns `None` if nothing is left.
pub fn visible(frame: &Frame, who: &Principal, owners: &BTreeMap<JobId, String>) -> Option<Frame> {
    let mine = |job: JobId| who.is_admin() || owners.get(&job).is_some_and(|o| *o == who.name);
    let jobs: Vec<JobDto> = frame.jobs.iter().filter(|j| mine(j.id)).cloned().collect();
    let logs: Vec<LogLine> = frame.logs.iter().filter(|l| mine(l.job)).cloned().collect();
    if jobs.is_empty() && logs.is_empty() && frame.removed.is_empty() {
        return None;
    }
    Some(Frame { jobs, removed: frame.removed.clone(), logs })
}

fn owners_snapshot(state: &AppState) -> BTreeMap<JobId, String> {
    state.owners.read().unwrap_or_else(|e| e.into_inner()).iter().map(|(k, v)| (*k, v.clone())).collect()
}

/// Full state for one user, used to resync a socket that fell behind.
fn full_snapshot(state: &AppState, who: &Principal) -> Frame {
    let owners = owners_snapshot(state);
    let jobs = state
        .queue
        .jobs()
        .iter()
        .map(|info| JobDto::from_info(info, owners.get(&info.id).map(String::as_str)))
        .collect();
    visible(&Frame { jobs, removed: vec![], logs: vec![] }, who, &owners).unwrap_or_default()
}

pub async fn ws_handler(State(state): State<Arc<AppState>>, who: Principal, ws: WebSocketUpgrade) -> Response {
    let rx = state.telemetry.subscribe();
    ws.on_upgrade(move |socket| stream(socket, rx, who, state))
}

async fn stream(mut socket: WebSocket, mut rx: broadcast::Receiver<Arc<Frame>>, who: Principal, state: Arc<AppState>) {
    // Start with the current state so a new page is not blank until the next change.
    let first = serde_json::to_string(&full_snapshot(&state, &who)).unwrap_or_default();
    if socket.send(Message::Text(first.into())).await.is_err() {
        return;
    }
    loop {
        let text = match rx.recv().await {
            Ok(frame) => {
                let owners = owners_snapshot(&state);
                match visible(&frame, &who, &owners) {
                    Some(f) => serde_json::to_string(&f).unwrap_or_default(),
                    None => continue,
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                serde_json::to_string(&full_snapshot(&state, &who)).unwrap_or_default()
            }
            Err(broadcast::error::RecvError::Closed) => break,
        };
        if socket.send(Message::Text(text.into())).await.is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dto(id: JobId, owner: &str) -> JobDto {
        JobDto {
            id,
            label: format!("job {id}"),
            owner: Some(owner.into()),
            destination: "/tmp".into(),
            remote: false,
            state: "running".into(),
            paused: false,
            percent: 10.0,
            rate_bps: 0.0,
            eta_secs: None,
            current_file: None,
            files_total: 1,
            files_done: 0,
            files_failed: 0,
            bytes_total: 100,
            bytes_copied: 10,
        }
    }

    #[test]
    fn users_only_see_their_own_jobs_and_logs() {
        let owners: BTreeMap<JobId, String> = [(1, "alice".into()), (2, "bob".into())].into();
        let frame = Frame {
            jobs: vec![dto(1, "alice"), dto(2, "bob")],
            removed: vec![],
            logs: vec![
                LogLine { job: 1, kind: "failed", message: "a".into() },
                LogLine { job: 2, kind: "failed", message: "b".into() },
            ],
        };
        let bob = Principal { name: "bob".into(), role: crate::auth::Role::User };
        let f = visible(&frame, &bob, &owners).unwrap();
        assert_eq!(f.jobs.iter().map(|j| j.id).collect::<Vec<_>>(), vec![2]);
        assert_eq!(f.logs.iter().map(|l| l.job).collect::<Vec<_>>(), vec![2]);

        let admin = Principal { name: "root".into(), role: crate::auth::Role::Admin };
        assert_eq!(visible(&frame, &admin, &owners).unwrap().jobs.len(), 2);
    }

    #[test]
    fn empty_frames_are_dropped_for_a_user_with_nothing_to_see() {
        let owners: BTreeMap<JobId, String> = [(1, "alice".into())].into();
        let frame = Frame { jobs: vec![dto(1, "alice")], removed: vec![], logs: vec![] };
        let eve = Principal { name: "eve".into(), role: crate::auth::Role::User };
        assert!(visible(&frame, &eve, &owners).is_none());
    }

    #[test]
    fn dto_maps_states_and_percentages() {
        let mut info = JobInfo {
            id: 7,
            label: "x".into(),
            sources: vec![],
            target: fc_engine::Target::IntoDir("/tmp".into()),
            state: JobState::Running,
            paused: true,
            files_total: 2,
            bytes_total: 200,
            files_copied: 1,
            files_skipped: 0,
            files_failed: 0,
            bytes_copied: 100,
            telemetry: None,
            queued_at: std::time::SystemTime::now(),
            elapsed: None,
        };
        let d = JobDto::from_info(&info, Some("alice"));
        assert_eq!(d.state, "paused");
        assert_eq!(d.percent, 0.0, "no telemetry yet");
        info.paused = false;
        assert_eq!(JobDto::from_info(&info, None).state, "running");
        info.state = JobState::Failed("disk full".into());
        assert_eq!(JobDto::from_info(&info, None).state, "failed");
    }

    #[test]
    fn collect_coalesces_and_handles_removal() {
        let mut dirty = BTreeSet::new();
        let mut removed = Vec::new();
        let mut logs = Vec::new();
        for _ in 0..50 {
            collect(Event::Progress { job: 3, telemetry: dummy_snapshot() }, &mut dirty, &mut removed, &mut logs);
        }
        assert_eq!(dirty.len(), 1, "many progress events for one job collapse to one entry");
        collect(Event::Removed { job: 3 }, &mut dirty, &mut removed, &mut logs);
        assert!(dirty.is_empty());
        assert_eq!(removed, vec![3]);
    }

    fn dummy_snapshot() -> fc_engine::Snapshot {
        fc_engine::Progress::new().snapshot()
    }
}

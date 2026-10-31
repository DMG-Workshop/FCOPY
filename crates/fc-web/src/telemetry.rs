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
use fc_torrent::{TorrentEngine, TorrentPhase, TorrentStatus};

use crate::AppState;
use crate::auth::Principal;

/// One BitTorrent transfer, as the console shows it. Admins only.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct TorrentDto {
    pub id: usize,
    /// The torrent's name, or its info hash until the name is known.
    pub name: String,
    pub info_hash: String,
    /// `checking`, `downloading`, `seeding`, `paused`, or `failed`.
    pub phase: &'static str,
    pub finished: bool,
    /// 0–100.
    pub percent: f64,
    pub progress_bytes: u64,
    pub total_bytes: u64,
    pub rate_bps: f64,
    pub upload_bps: f64,
    pub uploaded_bytes: u64,
    pub live_peers: u32,
    pub error: Option<String>,
}

const MIB: f64 = 1024.0 * 1024.0;

impl TorrentDto {
    pub fn from_status(s: &TorrentStatus) -> Self {
        let phase = match s.phase() {
            TorrentPhase::WaitingForMetadata => "waiting",
            TorrentPhase::Checking => "checking",
            TorrentPhase::Downloading => "downloading",
            TorrentPhase::Seeding => "seeding",
            TorrentPhase::Paused => "paused",
            TorrentPhase::Failed => "failed",
        };
        Self {
            id: s.id,
            name: s.name.clone().unwrap_or_else(|| s.info_hash.clone()),
            info_hash: s.info_hash.clone(),
            phase,
            finished: s.finished,
            percent: s.percent(),
            progress_bytes: s.progress_bytes,
            total_bytes: s.total_bytes,
            rate_bps: s.download_mbps * MIB,
            upload_bps: s.upload_mbps * MIB,
            uploaded_bytes: s.uploaded_bytes,
            live_peers: s.live_peers,
            error: s.error.clone(),
        }
    }
}

/// The torrent lane's rows, or nothing when it is not configured.
fn torrent_rows(engine: &TorrentEngine) -> Vec<TorrentDto> {
    engine.list().iter().map(TorrentDto::from_status).collect()
}

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
    pub conflict: Option<ConflictDto>,
}

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ConflictDto {
    pub id: u64,
    pub source: String,
    pub destination: String,
    pub different: bool,
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
            conflict: info.conflict.as_ref().map(|c| ConflictDto {
                id: c.id, source: c.details.source.display().to_string(),
                destination: c.details.destination.display().to_string(), different: c.details.different,
            }),
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
    /// The whole torrent list, sent when it changed. `None` leaves the page's
    /// list as it is. Only administrators ever receive it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub torrents: Option<Vec<TorrentDto>>,
}

/// Start the producer: a blocking thread drains the queue's (std) receiver
/// into an async channel, and a task turns those events into frames.
pub fn spawn_producer(state: Arc<AppState>, events: std::sync::mpsc::Receiver<Event>) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let history = state.history.clone();
    let weak_state = Arc::downgrade(&state);
    let reader_state = weak_state.clone();
    std::thread::Builder::new()
        .name("fc-web-events".into())
        .spawn(move || {
            while let Ok(ev) = events.recv() {
                history.observe(&ev);
                if let Some(state) = reader_state.upgrade() { state.recovery.observe(&ev, state.queue.is_shutting_down()); }
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
        // Torrent stats change without queue events, so the lane is compared each tick.
        let mut last_torrents: Option<Vec<TorrentDto>> = None;
        loop {
            let event = tokio::select! {
                ev = rx.recv() => Some(ev),
                _ = tick.tick() => None,
            };
            let Some(state) = weak_state.upgrade() else { break; };
            match event {
                Some(ev) => match ev {
                    Some(ev) => {
                        if let Event::Finished { job, .. } = &ev {
                            if let Some(error) = crate::history::persistence_error(&state) {
                                logs.push(LogLine { job: *job, kind: "history_error", message: format!("Transfer history was not saved: {error}. Open History and Refresh to retry.") });
                            }
                            if let Some(error) = state.recovery.error() {
                                logs.push(LogLine { job: *job, kind: "recovery_error", message: error });
                            }
                        }
                        record_checksum(&state, &ev);
                        collect(ev, &mut dirty, &mut removed, &mut logs);
                    },
                    None => break,
                },
                None => {
                    let torrents = state.torrents.as_ref().map(torrent_rows).filter(|now| last_torrents.as_ref() != Some(now));
                    if torrents.is_some() {
                        last_torrents = torrents.clone();
                    }
                    if dirty.is_empty() && removed.is_empty() && logs.is_empty() && torrents.is_none() {
                        continue;
                    }
                    let mut frame = build_frame(&state, &dirty, std::mem::take(&mut removed), std::mem::take(&mut logs));
                    frame.torrents = torrents;
                    dirty.clear();
                    // No subscribers is fine: nobody is watching.
                    let _ = state.telemetry.send(Arc::new(frame));
                }
            }
        }
    });
}

/// Keep each hashed file's digest for the job's checksum list, and forget a
/// job's list when the job leaves the queue.
fn record_checksum(state: &AppState, ev: &Event) {
    match ev {
        Event::FileCopied { job, dst, digest: Some(digest), .. } => {
            let entry = fc_engine::manifest::ManifestEntry { path: dst.clone(), digest: digest.clone() };
            state.checksums.write().unwrap_or_else(|e| e.into_inner()).entry(*job).or_default().push(entry);
        }
        Event::Removed { job } => {
            state.checksums.write().unwrap_or_else(|e| e.into_inner()).remove(job);
        }
        _ => {}
    }
}

/// Fold one queue event into the pending tick.
fn collect(ev: Event, dirty: &mut BTreeSet<JobId>, removed: &mut Vec<JobId>, logs: &mut Vec<LogLine>) {
    match ev {
        Event::Queued { job, .. }
        | Event::Conflict { job }
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
    Frame { jobs, removed, logs, torrents: None }
}

/// What one user may see of a frame. Returns `None` if nothing is left.
pub fn visible(frame: &Frame, who: &Principal, owners: &BTreeMap<JobId, String>) -> Option<Frame> {
    let mine = |job: JobId| who.is_admin() || owners.get(&job).is_some_and(|o| *o == who.name);
    let jobs: Vec<JobDto> = frame.jobs.iter().filter(|j| mine(j.id)).cloned().collect();
    let logs: Vec<LogLine> = frame.logs.iter().filter(|l| mine(l.job)).cloned().collect();
    let torrents = if who.is_admin() { frame.torrents.clone() } else { None };
    if jobs.is_empty() && logs.is_empty() && frame.removed.is_empty() && torrents.is_none() {
        return None;
    }
    Some(Frame { jobs, removed: frame.removed.clone(), logs, torrents })
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
    let torrents = state.torrents.as_ref().map(torrent_rows);
    visible(&Frame { jobs, removed: vec![], logs: vec![], torrents }, who, &owners).unwrap_or_default()
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
            conflict: None,
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
            torrents: None,
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
        let frame = Frame { jobs: vec![dto(1, "alice")], removed: vec![], logs: vec![], torrents: None };
        let eve = Principal { name: "eve".into(), role: crate::auth::Role::User };
        assert!(visible(&frame, &eve, &owners).is_none());
    }

    #[test]
    fn torrents_reach_administrators_only() {
        let owners = BTreeMap::new();
        let frame = Frame { torrents: Some(vec![]), ..Default::default() };
        let bob = Principal { name: "bob".into(), role: crate::auth::Role::User };
        assert!(visible(&frame, &bob, &owners).is_none(), "a torrent-only frame is nothing to a user");
        let admin = Principal { name: "root".into(), role: crate::auth::Role::Admin };
        assert_eq!(visible(&frame, &admin, &owners).unwrap().torrents, Some(vec![]));
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
            conflict: None,
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

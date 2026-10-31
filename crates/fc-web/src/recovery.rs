use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use fc_engine::{CopyOptions, Event, JobId, TransferRequest};
use fc_remote::recovery::{Action, Destination, Job, RecoveryStore};
use serde::{Deserialize, Serialize};

use crate::auth::{Principal, Role};
use crate::{ApiError, AppState, within_roots};

struct Inner {
    store: RecoveryStore,
    active: HashMap<JobId, u64>,
    claims: HashMap<u64, Option<std::fs::File>>,
    error: Option<String>,
}

pub struct Recovery {
    inner: Mutex<Inner>,
    persistent: bool,
}

impl Recovery {
    pub fn open(path: Option<PathBuf>) -> io::Result<Self> {
        Ok(Self {
            persistent: path.is_some(),
            inner: Mutex::new(Inner {
                store: RecoveryStore::open(path)?,
                active: HashMap::new(),
                claims: HashMap::new(),
                error: None,
            }),
        })
    }

    pub fn enqueue(
        &self,
        state: &AppState,
        request: TransferRequest,
        job: Job,
    ) -> io::Result<JobId> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let owner = job
            .owner
            .clone()
            .ok_or_else(|| io::Error::other("web recovery requires an owner"))?;
        let saved_id = inner.store.add(job)?;
        let claim = inner.store.claim(saved_id)?;
        inner.claims.insert(saved_id, claim);
        let id = state.history.enqueue(&state.queue, request, owner);
        inner.active.insert(id, saved_id);
        Ok(id)
    }

    pub fn observe(&self, event: &Event, shutting_down: bool) {
        if shutting_down
            && matches!(
                event,
                Event::Finished {
                    state: fc_engine::JobState::Cancelled,
                    ..
                }
            )
        {
            return;
        }
        if let Event::Finished { job, .. } | Event::Removed { job } = event {
            let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(id) = inner.active.remove(job) {
                let failed = matches!(
                    event,
                    Event::Finished {
                        state: fc_engine::JobState::Failed(_),
                        ..
                    }
                ) || matches!(event, Event::Finished { summary, .. } if summary.files_failed > 0);
                if let Err(error) = if failed {
                    Ok(())
                } else {
                    inner.store.remove(id)
                } {
                    eprintln!("cannot remove finished recovery job: {error}");
                    inner.error = Some(format!(
                        "Cannot remove finished job from recovery: {error}. Refresh Recovery and discard it after fixing storage."
                    ));
                }

                inner.claims.remove(&id);
            }
        }
    }

    pub fn error(&self) -> Option<String> {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .error
            .clone()
    }
}

#[derive(Serialize)]
pub struct RecoveryResponse {
    jobs: Vec<PausedJob>,
    persistent: bool,
    error: Option<String>,
}

#[derive(Serialize)]
struct PausedJob {
    #[serde(flatten)]
    job: Job,
    state: &'static str,
}

pub async fn list(
    State(state): State<Arc<AppState>>,
    principal: Principal,
) -> Result<Response, ApiError> {
    let recovery = state.recovery.clone();
    let response = tokio::task::spawn_blocking(move || {
        let mut inner = recovery.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.store.refresh()?;
        Ok::<_, io::Error>(RecoveryResponse {
            jobs: inner
                .store
                .list()
                .iter()
                .filter(|j| {
                    !inner.active.values().any(|id| *id == j.id)
                        && (principal.role == Role::Admin
                            || j.owner.as_deref() == Some(&principal.name))
                })
                .cloned()
                .map(|job| PausedJob {
                    job,
                    state: "paused",
                })
                .collect(),
            persistent: recovery.persistent,
            error: inner.error.clone(),
        })
    })
    .await
    .map_err(|e| ApiError::Internal(format!("recovery worker failed: {e}")))?
    .map_err(|e| ApiError::Internal(format!("cannot load recovery: {e}")))?;
    Ok((
        [(axum::http::header::CACHE_CONTROL, "no-store")],
        Json(response),
    )
        .into_response())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reply {
    action: String,
    #[serde(default)]
    password: String,
}

fn validate_access(state: &AppState, principal: &Principal, job: &Job) -> Result<(), ApiError> {
    if job.owner.as_deref() != Some(&principal.name) && principal.role != Role::Admin {
        return Err(ApiError::NotFound);
    }
    if job.remote() && principal.role != Role::Admin {
        return Err(ApiError::Forbidden(
            "remote recovery requires an administrator",
        ));
    }
    if !matches!(job.destination, Destination::Download { .. }) {
        for source in &job.sources {
            if !within_roots(&state.roots, source) || !source.exists() {
                return Err(ApiError::BadRequest(
                    "recovered source is missing or outside current shares".into(),
                ));
            }
        }
    }
    let local = match &job.destination {
        Destination::Directory { path }
        | Destination::File { path }
        | Destination::Download { path, .. } => Some(path),
        _ => None,
    };
    if local.is_some_and(|p| !within_roots(&state.roots, p)) {
        return Err(ApiError::BadRequest(
            "recovered destination is outside current shares".into(),
        ));
    }
    Ok(())
}

pub async fn act(
    State(state): State<Arc<AppState>>,
    principal: Principal,
    Path(id): Path<u64>,
    Json(reply): Json<Reply>,
) -> Result<Response, ApiError> {
    if !matches!(reply.action.as_str(), "resume" | "retry" | "discard") {
        return Err(ApiError::BadRequest(
            "action must be resume, retry or discard".into(),
        ));
    }
    tokio::task::spawn_blocking(move || {
        // Same lock order as creating a job and telemetry: owners before recovery.
        let mut owners = state.owners.write().unwrap_or_else(|e| e.into_inner());
        let mut inner = state
            .recovery
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        inner
            .store
            .refresh()
            .map_err(|e| ApiError::Internal(format!("cannot load recovery: {e}")))?;
        let job = inner.store.get(id).cloned().ok_or(ApiError::NotFound)?;
        if job.owner.as_deref() != Some(&principal.name) && principal.role != Role::Admin {
            return Err(ApiError::NotFound);
        }
        if inner.active.values().any(|active| *active == id) {
            return Err(ApiError::Conflict(
                "job is already in the live queue".into(),
            ));
        }
        let claim = inner
            .store
            .claim(id)
            .map_err(|e| ApiError::Conflict(e.to_string()))?;
        if reply.action == "discard" {
            inner
                .store
                .remove(id)
                .map_err(|e| ApiError::Internal(format!("cannot discard recovery job: {e}")))?;
            inner.error = None;
            return Ok(StatusCode::NO_CONTENT.into_response());
        }
        validate_access(&state, &principal, &job)?;
        let action = if reply.action == "resume" {
            Action::Resume
        } else {
            Action::Retry
        };
        let request = job
            .request(action, reply.password)
            .map_err(|e| ApiError::BadRequest(e.to_string()))?;
        let owner = job.owner.ok_or(ApiError::NotFound)?;
        let job_id = state.history.enqueue(&state.queue, request, owner.clone());
        inner.active.insert(job_id, id);
        inner.claims.insert(id, claim);
        owners.insert(job_id, owner);
        Ok((
            StatusCode::ACCEPTED,
            Json(crate::api::Created { id: job_id }),
        )
            .into_response())
    })
    .await
    .map_err(|e| ApiError::Internal(format!("recovery worker failed: {e}")))?
}

pub fn saved_job(
    request: &TransferRequest,
    destination: Destination,
    owner: String,
    defaults: &CopyOptions,
) -> Result<Job, ApiError> {
    Job::new(request, destination, Some(owner), defaults)
        .map_err(|e| ApiError::BadRequest(e.to_string()))
}

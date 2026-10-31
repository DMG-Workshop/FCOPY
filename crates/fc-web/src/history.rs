//! History is recorded before events reach telemetry, even with no browser connected.

use std::collections::HashMap;
use std::io;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use fc_engine::{Event, JobId, Queue, TransferRequest};
use fc_remote::history::{Entry, HistoryStore, Pending};

use crate::{ApiError, AppState};
use crate::auth::{Principal, Role};

pub struct History {
    store: Mutex<HistoryStore>,
    pending: Mutex<HashMap<JobId, Pending>>,
    error: Mutex<Option<String>>,
    persistent: bool,
}

impl History {
    pub fn open(path: Option<PathBuf>) -> io::Result<Self> {
        Ok(Self {
            persistent: path.is_some(),
            store: Mutex::new(HistoryStore::open(path)?),
            pending: Mutex::new(HashMap::new()),
            error: Mutex::new(None),
        })
    }

    pub fn enqueue(&self, queue: &Queue, request: TransferRequest, owner: String) -> JobId {
        let metadata = Pending::new(&request, Some(owner));
        // The event reader cannot finish a fast job before its metadata is registered.
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let id = queue.add(request);
        pending.insert(id, metadata);
        id
    }

    pub fn observe(&self, event: &Event) {
        let entry = {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            match event {
                Event::Finished { job, state, summary } => pending.remove(job).map(|metadata| metadata.finish(state, summary)),
                _ => return,
            }
        };
        if let Some(entry) = entry {
            let result = entry.and_then(|entry| self.store.lock().unwrap_or_else(|e| e.into_inner()).record(entry));
            if let Err(error) = &result {
                eprintln!("transfer history could not be saved: {error}");
            }
            *self.error.lock().unwrap_or_else(|e| e.into_inner()) = result.err().map(|e| e.to_string());
        }
    }
}

#[derive(serde::Serialize)]
pub struct HistoryResponse {
    entries: Vec<Entry>,
    persistent: bool,
}

pub async fn list(State(state): State<Arc<AppState>>, principal: Principal) -> Result<Response, ApiError> {
    let history = state.history.clone();
    let entries = tokio::task::spawn_blocking(move || {
        let result = history.store.lock().unwrap_or_else(|e| e.into_inner()).list();
        *history.error.lock().unwrap_or_else(|e| e.into_inner()) = result.as_ref().err().map(|e| e.to_string());
        result
    }).await.map_err(|e| ApiError::Internal(format!("history worker failed: {e}")))?
        .map_err(|e| ApiError::Internal(format!("cannot load transfer history: {e}")))?;
    Ok(([(axum::http::header::CACHE_CONTROL, "no-store")], Json(HistoryResponse {
        entries: entries.into_iter().filter(|e| principal.role == Role::Admin || e.owner.as_deref() == Some(&principal.name)).collect(),
        persistent: state.history.persistent,
    })).into_response())
}

pub async fn clear(State(state): State<Arc<AppState>>, principal: Principal) -> Result<StatusCode, ApiError> {
    let history = state.history.clone();
    tokio::task::spawn_blocking(move || {
        let owner = (principal.role != Role::Admin).then_some(principal.name.as_str());
        history.store.lock().unwrap_or_else(|e| e.into_inner()).clear(owner)
    }).await.map_err(|e| ApiError::Internal(format!("history worker failed: {e}")))?
        .map_err(|e| ApiError::Internal(format!("cannot clear transfer history: {e}")))?;
    *state.history.error.lock().unwrap_or_else(|e| e.into_inner()) = None;
    Ok(StatusCode::NO_CONTENT)
}

pub fn persistence_error(state: &AppState) -> Option<String> {
    state.history.error.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

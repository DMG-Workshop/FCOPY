//! REST handlers. Every handler that touches a job goes through [`owned_job`],
//! so a user can only see or change their own transfers. Admins see all.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::StatusCode;
use axum::http::header::{COOKIE, SET_COOKIE};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use fc_engine::{JobId, JobInfo, TransferRequest};

use crate::auth::{self, Principal, Role, SESSION_COOKIE};
use crate::telemetry::JobDto;
use crate::{ApiError, AppState, within_roots};

/// Most sources accepted in one request.
const MAX_SOURCES: usize = 1000;

#[derive(Deserialize)]
pub struct LoginReq {
    pub user: String,
    pub password: String,
}

#[derive(Serialize)]
pub struct Me {
    pub name: String,
    pub role: Role,
}

impl From<Principal> for Me {
    fn from(p: Principal) -> Self {
        Self { name: p.name, role: p.role }
    }
}

pub async fn login(
    State(s): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(req): Json<LoginReq>,
) -> Result<Response, ApiError> {
    let ip = addr.ip().to_canonical();
    if let Err(wait) = s.limiter.check(ip, Some(&req.user)) {
        return Err(ApiError::LockedOut(wait));
    }
    let Some(account) = s.users.authenticate(&req.user, &req.password) else {
        s.limiter.failure(ip, Some(&req.user));
        return Err(ApiError::Unauthorized);
    };
    s.limiter.success(ip, &account.name);

    let principal = Principal { name: account.name.clone(), role: account.role };
    let token = s.sessions.issue(principal.clone());
    let cookie = auth::session_cookie(&token, s.sessions.ttl(), s.cookie_secure);
    Ok(([(SET_COOKIE, cookie)], Json(Me::from(principal))).into_response())
}

pub async fn logout(State(s): State<Arc<AppState>>, headers: axum::http::HeaderMap) -> Response {
    if let Some(token) = headers
        .get(COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|c| auth::cookie_value(c, SESSION_COOKIE))
    {
        s.sessions.revoke(token);
    }
    ([(SET_COOKIE, auth::clear_session_cookie(s.cookie_secure))], StatusCode::NO_CONTENT).into_response()
}

pub async fn me(p: Principal) -> Json<Me> {
    Json(p.into())
}

pub async fn list_transfers(State(s): State<Arc<AppState>>, p: Principal) -> Json<Vec<JobDto>> {
    let owners = s.owners.read().unwrap_or_else(|e| e.into_inner());
    let jobs = s
        .queue
        .jobs()
        .iter()
        .map(|info| (info, owners.get(&info.id).cloned()))
        .filter(|(_, owner)| p.is_admin() || owner.as_deref() == Some(p.name.as_str()))
        .map(|(info, owner)| JobDto::from_info(info, owner.as_deref()))
        .collect();
    Json(jobs)
}

#[derive(Deserialize)]
pub struct CreateTransfer {
    /// Absolute paths of files or folders to copy.
    pub sources: Vec<PathBuf>,
    /// Destination folder. Created if missing.
    pub dest: PathBuf,
    pub label: Option<String>,
}

#[derive(Serialize)]
pub struct Created {
    pub id: JobId,
}

pub async fn create_transfer(
    State(s): State<Arc<AppState>>,
    p: Principal,
    Json(req): Json<CreateTransfer>,
) -> Result<(StatusCode, Json<Created>), ApiError> {
    if req.sources.is_empty() || req.sources.len() > MAX_SOURCES {
        return Err(ApiError::BadRequest(format!("give between 1 and {MAX_SOURCES} sources")));
    }
    for src in &req.sources {
        if !within_roots(&s.roots, src) || !src.exists() {
            return Err(ApiError::BadRequest(format!("source not found inside a share: {}", src.display())));
        }
    }
    if !within_roots(&s.roots, &req.dest) {
        return Err(ApiError::BadRequest("destination is outside every share".into()));
    }

    let mut request = TransferRequest::into_dir(req.sources, &req.dest);
    if let Some(label) = req.label.filter(|l| !l.trim().is_empty()) {
        request = request.with_label(label);
    }
    let id = s.queue.add(request);
    s.owners.write().unwrap_or_else(|e| e.into_inner()).insert(id, p.name);
    Ok((StatusCode::ACCEPTED, Json(Created { id })))
}

/// Look up a job the caller may act on. Other users' jobs look missing, so
/// their existence isn't revealed.
fn owned_job(s: &AppState, p: &Principal, id: JobId) -> Result<JobInfo, ApiError> {
    let info = s.queue.job(id).ok_or(ApiError::NotFound)?;
    let owner = s.owners.read().unwrap_or_else(|e| e.into_inner()).get(&id).cloned();
    if p.is_admin() || owner.as_deref() == Some(p.name.as_str()) {
        Ok(info)
    } else {
        Err(ApiError::NotFound)
    }
}

/// Cancel a pending or running job, or remove a finished one from the list.
pub async fn cancel_transfer(State(s): State<Arc<AppState>>, p: Principal, Path(id): Path<JobId>) -> Result<StatusCode, ApiError> {
    owned_job(&s, &p, id)?;
    if s.queue.cancel(id) || s.queue.remove(id) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::Conflict("job cannot be cancelled or removed right now".into()))
    }
}

pub async fn pause_transfer(State(s): State<Arc<AppState>>, p: Principal, Path(id): Path<JobId>) -> Result<StatusCode, ApiError> {
    owned_job(&s, &p, id)?;
    if s.queue.pause_job(id) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::Conflict("job has already finished".into()))
    }
}

pub async fn resume_transfer(State(s): State<Arc<AppState>>, p: Principal, Path(id): Path<JobId>) -> Result<StatusCode, ApiError> {
    owned_job(&s, &p, id)?;
    if s.queue.resume_job(id) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::NotFound)
    }
}

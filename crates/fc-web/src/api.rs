//! REST handlers. Every handler that touches a job goes through [`owned_job`],
//! so a user can only see or change their own transfers. Admins see all.
//! Pairing, settings and accounts are admin-only.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::extract::{ConnectInfo, Path, State};
use axum::http::StatusCode;
use axum::http::header::{COOKIE, SET_COOKIE};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;

use fc_engine::{ErrorPolicy, JobId, JobInfo, RetryPolicy, TransferRequest};
use fc_remote::client::{ClientError, Connector, RemoteSpec, Session};

use crate::auth::{self, AuthError, Principal, Role, SESSION_COOKIE};
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

// ---------------------------------------------------------------------------
// Admin only: paired clients, settings, accounts
// ---------------------------------------------------------------------------

/// Pause or resume the whole queue. Admins only: it affects every user's jobs.
pub async fn pause_queue(State(s): State<Arc<AppState>>, p: Principal) -> Result<StatusCode, ApiError> {
    require_admin(&p)?;
    s.queue.pause();
    Ok(StatusCode::NO_CONTENT)
}

pub async fn resume_queue(State(s): State<Arc<AppState>>, p: Principal) -> Result<StatusCode, ApiError> {
    require_admin(&p)?;
    s.queue.resume();
    Ok(StatusCode::NO_CONTENT)
}

fn require_admin(p: &Principal) -> Result<(), ApiError> {
    if p.is_admin() { Ok(()) } else { Err(ApiError::Forbidden("administrators only")) }
}

#[derive(Serialize)]
pub struct ClientDto {
    pub id: u64,
    pub name: String,
    pub target: String,
    pub fingerprint: String,
    /// `online`, `offline`, or `identity_changed` (the agent now presents a different certificate).
    pub status: &'static str,
}

/// Check every paired agent in parallel, each with a short timeout.
pub async fn list_clients(State(s): State<Arc<AppState>>, p: Principal) -> Result<Json<Vec<ClientDto>>, ApiError> {
    require_admin(&p)?;
    let profiles = s.clients.list();
    let probes: Vec<_> = profiles
        .iter()
        .map(|pr| {
            let (target, fp) = (pr.target.clone(), pr.fingerprint.clone());
            tokio::spawn(async move {
                let Ok(spec) = RemoteSpec::parse(&target) else { return "offline" };
                match fc_remote::client::probe(&spec, Some(fp)).await {
                    Ok(_) => "online",
                    Err(ClientError::HostKeyChanged { .. }) => "identity_changed",
                    Err(_) => "offline",
                }
            })
        })
        .collect();

    let mut out = Vec::with_capacity(profiles.len());
    for (pr, probe) in profiles.into_iter().zip(probes) {
        let status = probe.await.unwrap_or("offline");
        out.push(ClientDto { id: pr.id, name: pr.name, target: pr.target, fingerprint: pr.fingerprint, status });
    }
    Ok(Json(out))
}

#[derive(Deserialize)]
pub struct PairReq {
    pub name: String,
    /// `[user@]host[:port]:/share/path`.
    pub target: String,
    /// Used once to log in and check the pairing. It is never stored.
    pub password: String,
    /// Absent on the first call. The admin confirms the fingerprint returned by
    /// that call, then calls again with it. The login only runs with the pin.
    pub expected_fingerprint: Option<String>,
}

/// Pair a remote PC in two steps, so a person confirms the fingerprint before
/// any login is attempted:
///
/// 1. Without `expected_fingerprint`: the agent is probed and its fingerprint
///    returned with `409` and `needs_confirmation: true`. Nothing is saved.
/// 2. With it: the login runs with that exact pin. If it succeeds, the profile
///    is saved with the confirmed fingerprint.
pub async fn pair_client(State(s): State<Arc<AppState>>, p: Principal, Json(req): Json<PairReq>) -> Result<Response, ApiError> {
    require_admin(&p)?;
    let name = req.name.trim().to_string();
    if name.is_empty() || name.chars().count() > 64 {
        return Err(ApiError::BadRequest("name must be 1 to 64 characters".into()));
    }
    let spec = RemoteSpec::parse(&req.target).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    if req.password.is_empty() {
        return Err(ApiError::BadRequest("password or access key is required".into()));
    }

    let Some(expected) = req.expected_fingerprint else {
        let presented = fc_remote::client::probe(&spec, None)
            .await
            .map_err(|e| ApiError::Remote(StatusCode::BAD_GATEWAY, format!("cannot reach {spec}: {e}")))?;
        let body = json!({
            "needs_confirmation": true,
            "fingerprint": presented,
            "error": "confirm this fingerprint on the other PC, then pair again with it",
        });
        return Ok((StatusCode::CONFLICT, Json(body)).into_response());
    };

    let connector = Connector::new(spec.clone()).password(req.password).pinned(Some(expected.clone()));
    match Session::open(&connector, None).await {
        Ok(session) => {
            session.close().await;
            let profile = s
                .clients
                .add(name, spec.to_string(), expected.clone())
                .map_err(|e| ApiError::Remote(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            let dto = ClientDto { id: profile.id, name: profile.name, target: profile.target, fingerprint: expected, status: "online" };
            Ok((StatusCode::CREATED, Json(dto)).into_response())
        }
        Err(ClientError::HostKeyChanged { presented, .. }) => Err(ApiError::Conflict(format!(
            "the agent's identity does not match the fingerprint you confirmed; it presents {presented}"
        ))),
        Err(ClientError::AuthFailed { reason, .. }) => {
            Err(ApiError::Remote(StatusCode::UNAUTHORIZED, format!("the agent refused the login: {reason}")))
        }
        Err(e) => Err(ApiError::Remote(StatusCode::BAD_GATEWAY, e.to_string())),
    }
}

pub async fn unpair_client(State(s): State<Arc<AppState>>, p: Principal, Path(id): Path<u64>) -> Result<StatusCode, ApiError> {
    require_admin(&p)?;
    match s.clients.remove(id) {
        Ok(true) => Ok(StatusCode::NO_CONTENT),
        Ok(false) => Err(ApiError::NotFound),
        Err(e) => Err(ApiError::Remote(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

/// Settings the dashboard can change while the queue runs.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    /// Total throughput cap in bytes per second. `null` means unlimited.
    pub speed_limit_bps: Option<u64>,
    /// `retry` (back off and try again) or `skip` (move on at once).
    pub error_policy: String,
}

/// Largest speed limit accepted: 1 GiB/s.
const MAX_SPEED_LIMIT: u64 = 1 << 30;

fn current_settings(s: &AppState) -> Settings {
    let error_policy = match s.queue.error_policy() {
        ErrorPolicy::Auto(_) => "retry",
        ErrorPolicy::Skip => "skip",
        ErrorPolicy::Cancel => "cancel",
        ErrorPolicy::Ask => "ask",
    };
    Settings { speed_limit_bps: s.queue.speed_limit(), error_policy: error_policy.into() }
}

pub async fn get_settings(State(s): State<Arc<AppState>>, p: Principal) -> Result<Json<Settings>, ApiError> {
    require_admin(&p)?;
    Ok(Json(current_settings(&s)))
}

pub async fn put_settings(State(s): State<Arc<AppState>>, p: Principal, Json(req): Json<Settings>) -> Result<Json<Settings>, ApiError> {
    require_admin(&p)?;
    // 0 is accepted as "unlimited", the same as null.
    let limit = req.speed_limit_bps.filter(|&b| b > 0);
    if limit.is_some_and(|b| b > MAX_SPEED_LIMIT) {
        return Err(ApiError::BadRequest("speed limit may be at most 1 GiB/s".into()));
    }
    let policy = match req.error_policy.as_str() {
        "retry" => ErrorPolicy::Auto(RetryPolicy::default()),
        "skip" => ErrorPolicy::Skip,
        _ => return Err(ApiError::BadRequest("error policy must be 'retry' or 'skip'".into())),
    };
    s.queue.set_speed_limit(limit);
    s.queue.set_error_policy(policy);
    Ok(Json(current_settings(&s)))
}

#[derive(Serialize)]
pub struct UserDto {
    pub name: String,
    pub role: Role,
}

pub async fn list_users(State(s): State<Arc<AppState>>, p: Principal) -> Result<Json<Vec<UserDto>>, ApiError> {
    require_admin(&p)?;
    Ok(Json(s.users.list().into_iter().map(|(name, role)| UserDto { name, role }).collect()))
}

#[derive(Deserialize)]
pub struct CreateUser {
    pub name: String,
    pub password: String,
    pub role: Role,
}

pub async fn create_user(
    State(s): State<Arc<AppState>>,
    p: Principal,
    Json(req): Json<CreateUser>,
) -> Result<(StatusCode, Json<UserDto>), ApiError> {
    require_admin(&p)?;
    match s.users.add(&req.name, &req.password, req.role) {
        Ok(()) => Ok((StatusCode::CREATED, Json(UserDto { name: req.name, role: req.role }))),
        Err(AuthError::Exists) => Err(ApiError::Conflict("an account with that name exists".into())),
        Err(e) => Err(ApiError::BadRequest(e.to_string())),
    }
}

pub async fn delete_user(State(s): State<Arc<AppState>>, p: Principal, Path(name): Path<String>) -> Result<StatusCode, ApiError> {
    require_admin(&p)?;
    if name == p.name {
        return Err(ApiError::Conflict("you cannot delete the account you are signed in with".into()));
    }
    if !s.users.remove(&name) {
        return Err(ApiError::NotFound);
    }
    s.sessions.revoke_user(&name);
    Ok(StatusCode::NO_CONTENT)
}

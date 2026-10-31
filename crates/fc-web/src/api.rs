//! REST handlers. Every handler that touches a job goes through [`owned_job`],
//! so a user can only see or change their own transfers. Admins see all.
//! Pairing, settings and accounts are admin-only.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::{ConnectInfo, Path, Query, State};
use axum::http::StatusCode;
use axum::http::header::{COOKIE, SET_COOKIE};
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::json;

use fc_engine::{ErrorPolicy, JobId, JobInfo, RetryPolicy, TransferRequest};
use fc_remote::backend::{PullBackend, RemoteBackend};
use fc_remote::client::{ClientError, Connector, RemoteSpec, Session};
use fc_torrent::{TorrentEngine, TorrentError};

use crate::auth::{self, AuthError, Principal, Role, SESSION_COOKIE};
use crate::telemetry::{JobDto, TorrentDto};
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
    ConnectInfo(addr): ConnectInfo<crate::https::PeerAddr>,
    Json(req): Json<LoginReq>,
) -> Result<Response, ApiError> {
    let ip = addr.0.ip().to_canonical();
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

#[derive(Serialize)]
pub struct Info {
    pub scheme: &'static str,
    /// `SHA256:…` of the console's certificate.
    pub certificate: Option<String>,
    /// Whether this console has a BitTorrent lane.
    pub torrents: bool,
}

/// Public. The sign-in page shows the certificate fingerprint so it can be
/// compared with the one the agent shows.
pub async fn info(State(s): State<Arc<AppState>>) -> Json<Info> {
    let scheme = if s.cookie_secure { "https" } else { "http" };
    Json(Info { scheme, certificate: s.certificate_fingerprint.clone(), torrents: s.torrents.is_some() })
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
    /// Destination folder. Created if missing. On a remote job this is a folder
    /// on the paired PC, below its share, for example `backup/photos`.
    pub dest: PathBuf,
    pub label: Option<String>,
    /// Send to a paired PC instead of this machine. Admins only.
    pub remote: Option<RemoteTarget>,
    #[serde(default = "default_conflict")]
    pub conflict: String,
    /// Delete each source once its copy is synced and read back. Not allowed
    /// when downloading from another PC.
    #[serde(default)]
    pub move_source: bool,
}

fn default_conflict() -> String { "ask".into() }

fn conflict_policy(value: &str) -> Result<fc_engine::conflict::ConflictPolicy, ApiError> {
    use fc_engine::conflict::ConflictPolicy as P;
    match value {
        "ask" => Ok(P::Ask), "replace" => Ok(P::Replace), "skip" => Ok(P::Skip),
        "keep_both" => Ok(P::KeepBoth), "compare" => Ok(P::Compare),
        _ => Err(ApiError::BadRequest("conflict must be ask, replace, skip, keep_both or compare".into())),
    }
}

#[derive(Deserialize)]
pub struct RemoteTarget {
    pub client_id: u64,
    /// Login for the PC. Used for this job only, never stored.
    pub password: String,
    #[serde(default)]
    pub direction: TransferDirection,
}

#[derive(Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransferDirection {
    #[default]
    Upload,
    Download,
}

/// The profile's address, with the path replaced by the job's folder. The
/// profile's user and port are kept.
fn remote_spec(target: &str, dest: &str) -> Result<RemoteSpec, fc_remote::client::SpecError> {
    let base = RemoteSpec::parse(target)?;
    let mut spec = RemoteSpec::parse(&format!("{}:/{}", base.host_port(), dest.trim_matches('/')))?;
    spec.user = base.user;
    Ok(spec)
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
    let policy = conflict_policy(&req.conflict)?;
    if req.sources.is_empty() || req.sources.len() > MAX_SOURCES {
        return Err(ApiError::BadRequest(format!("give between 1 and {MAX_SOURCES} sources")));
    }
    let download = req.remote.as_ref().is_some_and(|r| r.direction == TransferDirection::Download);
    if req.remote.is_some() {
        require_admin(&p)?;
    }
    if download && req.move_source {
        return Err(ApiError::BadRequest("files on another PC can be copied, not moved".into()));
    }
    let move_source = req.move_source;
    if !download {
        for src in &req.sources {
            if !within_roots(&s.roots, src) || !src.exists() {
                return Err(ApiError::BadRequest(format!("source not found inside a share: {}", src.display())));
            }
        }
    }
    let (mut request, destination) = match req.remote {
        None => {
            if !within_roots(&s.roots, &req.dest) {
                return Err(ApiError::BadRequest("destination is outside every share".into()));
            }
            (TransferRequest::into_dir(req.sources, &req.dest), fc_remote::recovery::Destination::Directory { path: req.dest })
        }
        Some(remote) => {
            require_admin(&p)?;
            if remote.password.is_empty() {
                return Err(ApiError::BadRequest("the PC's password or access key is required".into()));
            }
            let profile = s.clients.get(remote.client_id).ok_or(ApiError::NotFound)?;
            let spec = if download {
                if !within_roots(&s.roots, &req.dest) {
                    return Err(ApiError::BadRequest("destination is outside every share".into()));
                }
                for src in &req.sources {
                    remote_spec(&profile.target, &src.to_string_lossy()).map_err(|e| ApiError::BadRequest(e.to_string()))?;
                }
                RemoteSpec::parse(&profile.target).map_err(|e| ApiError::BadRequest(e.to_string()))?
            } else {
                remote_spec(&profile.target, &req.dest.to_string_lossy()).map_err(|e| ApiError::BadRequest(e.to_string()))?
            };
            let connector = Connector::new(spec).password(remote.password).pinned(Some(profile.fingerprint));
            let backend = RemoteBackend::new(connector).map_err(|e| ApiError::Remote(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
            let destination = backend.recovery_destination(download.then(|| req.dest.clone()))
                .map_err(|e| ApiError::BadRequest(e.to_string()))?;
            if download {
                (TransferRequest::to_backend(req.sources, Arc::new(PullBackend::new(Arc::new(backend), req.dest))), destination)
            } else {
                (TransferRequest::to_backend(req.sources, Arc::new(backend)), destination)
            }
        }
    };
    if let Some(label) = req.label.filter(|l| !l.trim().is_empty()) {
        request = request.with_label(label);
    }
    if move_source {
        request = request.with_options(fc_engine::CopyOptions { move_source: true, ..s.queue.copy_options() });
    }
    let request = request.with_conflict_policy(policy);
    let job = crate::recovery::saved_job(&request, destination, p.name.clone(), &s.queue.copy_options())?;
    let state = s.clone();
    let owner = p.name.clone();
    let id = tokio::task::spawn_blocking(move || {
        let mut owners = state.owners.write().unwrap_or_else(|e| e.into_inner());
        let id = state.recovery.enqueue(&state, request, job)?;
        owners.insert(id, owner);
        Ok::<_, std::io::Error>(id)
    }).await.map_err(|e| ApiError::Internal(format!("recovery worker failed: {e}")))?
        .map_err(|e| ApiError::Internal(format!("job was not started because recovery could not be saved: {e}")))?;
    // A worker can already be waiting before ownership is registered.
    if let Some(info) = s.queue.job(id) {
        let frame = crate::telemetry::Frame {
            jobs: vec![crate::telemetry::JobDto::from_info(&info, Some(&p.name))],
            ..Default::default()
        };
        let _ = s.telemetry.send(Arc::new(frame));
    }
    Ok((StatusCode::ACCEPTED, Json(Created { id })))
}

#[derive(Deserialize)]
pub struct BrowseRemote {
    pub password: String,
    /// Empty to list the authenticated account's shares.
    #[serde(default)]
    pub path: String,
}

pub async fn browse_client(
    State(s): State<Arc<AppState>>,
    p: Principal,
    Path(id): Path<u64>,
    Json(req): Json<BrowseRemote>,
) -> Result<Response, ApiError> {
    require_admin(&p)?;
    let profile = s.clients.get(id).ok_or(ApiError::NotFound)?;
    if req.password.is_empty() {
        return Err(ApiError::BadRequest("the PC's password or access key is required".into()));
    }
    let path = if req.path.is_empty() {
        String::new()
    } else {
        remote_spec(&profile.target, &req.path).map_err(|e| ApiError::BadRequest(e.to_string()))?.path
    };
    let spec = RemoteSpec::parse(&profile.target).map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let connector = Connector::new(spec).password(req.password).pinned(Some(profile.fingerprint));
    let entries = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        let session = Session::open(&connector, None).await?;
        let result = session.browse(&path).await;
        session.close().await;
        result
    }).await.map_err(|_| ApiError::Remote(StatusCode::GATEWAY_TIMEOUT, "remote browsing timed out".into()))?
        .map_err(|e| ApiError::Remote(StatusCode::BAD_GATEWAY, e.to_string()))?;
    Ok(([(axum::http::header::CACHE_CONTROL, "no-store")], Json(json!({ "path": path, "entries": entries }))).into_response())
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

#[derive(Deserialize)]
pub struct ConflictReply {
    pub conflict_id: u64,
    pub action: String,
    #[serde(default)]
    pub apply_to_all: bool,
}

pub async fn resolve_conflict(State(s): State<Arc<AppState>>, p: Principal, Path(id): Path<JobId>, Json(reply): Json<ConflictReply>) -> Result<StatusCode, ApiError> {
    owned_job(&s, &p, id)?;
    use fc_engine::conflict::{ConflictAction as A, ConflictChoice};
    let action = match reply.action.as_str() {
        "replace" => A::Replace, "skip" => A::Skip, "keep_both" => A::KeepBoth,
        "compare" => A::Compare, "cancel" => A::Cancel,
        _ => return Err(ApiError::BadRequest("unknown conflict action".into())),
    };
    if s.queue.resolve_conflict(id, reply.conflict_id, ConflictChoice { action, apply_to_all: reply.apply_to_all }) {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(ApiError::Conflict("conflict is stale, cancelled, or already compared".into()))
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

#[derive(Serialize)]
pub struct QueueState {
    pub paused: bool,
}

/// Whether the whole queue is paused. Any signed-in user may read it.
pub async fn queue_state(State(s): State<Arc<AppState>>, _p: Principal) -> Json<QueueState> {
    Json(QueueState { paused: s.queue.is_paused() })
}

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

pub async fn vpn_providers(p: Principal) -> Result<Json<&'static [fc_remote::vpn::Provider]>, ApiError> {
    require_admin(&p)?;
    Ok(Json(fc_remote::vpn::PROVIDERS))
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
        Err(e @ (AuthError::Storage(_) | AuthError::Hash(_))) => Err(ApiError::Internal(e.to_string())),
        Err(e) => Err(ApiError::BadRequest(e.to_string())),
    }
}

pub async fn delete_user(State(s): State<Arc<AppState>>, p: Principal, Path(name): Path<String>) -> Result<StatusCode, ApiError> {
    require_admin(&p)?;
    if name == p.name {
        return Err(ApiError::Conflict("you cannot delete the account you are signed in with".into()));
    }
    if !s.users.remove(&name).map_err(|error| ApiError::Internal(error.to_string()))? {
        return Err(ApiError::NotFound);
    }
    s.sessions.revoke_user(&name);
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// BitTorrent: admin only. The engine is shared by the console, so its transfers
// have no owner, the same as paired PCs.
// ---------------------------------------------------------------------------

/// Largest `.torrent` file accepted: 4 MiB. A metainfo file for a multi-terabyte
/// set is still only a few megabytes.
pub const MAX_TORRENT_FILE: usize = 4 << 20;
/// Longest magnet link accepted.
const MAX_MAGNET: usize = 8 << 10;
/// Most peer addresses given with one `.torrent` file.
const MAX_PEERS: usize = 50;

fn engine(s: &AppState) -> Result<&TorrentEngine, ApiError> {
    s.torrents.as_ref().ok_or(ApiError::NotFound)
}

fn torrent_error(e: TorrentError) -> ApiError {
    match e {
        TorrentError::NotFound(_) => ApiError::NotFound,
        other => ApiError::Remote(StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
    }
}

pub async fn list_torrents(State(s): State<Arc<AppState>>, p: Principal) -> Result<Json<Vec<TorrentDto>>, ApiError> {
    require_admin(&p)?;
    let rows = engine(&s)?.list().iter().map(TorrentDto::from_status).collect();
    Ok(Json(rows))
}

#[derive(Deserialize)]
pub struct MagnetReq {
    pub magnet: String,
    /// Comma-separated `host:port` peers to ask for the file list. Trackers are off.
    pub peers: Option<String>,
}

/// Add a magnet link. The transfer is listed at once, as waiting for its file
/// list until peers send it, then it becomes an ordinary download under the
/// same id.
pub async fn add_magnet(State(s): State<Arc<AppState>>, p: Principal, Json(req): Json<MagnetReq>) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    require_admin(&p)?;
    let engine = engine(&s)?;
    let magnet = req.magnet.trim();
    if !magnet.starts_with("magnet:?") || magnet.len() > MAX_MAGNET {
        return Err(ApiError::BadRequest("paste a magnet link that starts with magnet:?".into()));
    }
    let peers = parse_peers(req.peers.as_deref())?;
    let id = engine.add_magnet(magnet, false, peers).await.map_err(|e| ApiError::BadRequest(e.to_string()))?;
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))))
}

#[derive(Deserialize)]
pub struct TorrentFileQuery {
    /// Comma-separated `host:port` peers to contact directly. Trackers are off,
    /// so a file torrent with no peers given finds none.
    pub peers: Option<String>,
}

/// Add a `.torrent` file. The body is the raw file.
pub async fn add_torrent_file(
    State(s): State<Arc<AppState>>,
    p: Principal,
    Query(q): Query<TorrentFileQuery>,
    body: Bytes,
) -> Result<(StatusCode, Json<serde_json::Value>), ApiError> {
    require_admin(&p)?;
    let engine = engine(&s)?;
    // A metainfo file is a bencoded dictionary.
    if body.first() != Some(&b'd') {
        return Err(ApiError::BadRequest("that is not a .torrent file".into()));
    }
    let peers = parse_peers(q.peers.as_deref())?;
    let id = engine.add_torrent_bytes(body.to_vec(), false, peers).await.map_err(|e| ApiError::BadRequest(e.to_string()))?;
    Ok((StatusCode::CREATED, Json(json!({ "id": id }))))
}

fn parse_peers(raw: Option<&str>) -> Result<Vec<SocketAddr>, ApiError> {
    let Some(raw) = raw.filter(|r| !r.trim().is_empty()) else { return Ok(vec![]) };
    let peers: Vec<SocketAddr> = raw
        .split(',')
        .map(|p| p.trim().parse().map_err(|_| ApiError::BadRequest(format!("not a host:port address: {}", p.trim()))))
        .collect::<Result<_, _>>()?;
    if peers.len() > MAX_PEERS {
        return Err(ApiError::BadRequest(format!("give at most {MAX_PEERS} peers")));
    }
    Ok(peers)
}

pub async fn pause_torrent(State(s): State<Arc<AppState>>, p: Principal, Path(id): Path<usize>) -> Result<StatusCode, ApiError> {
    require_admin(&p)?;
    engine(&s)?.pause(id).await.map_err(torrent_error)?;
    Ok(StatusCode::NO_CONTENT)
}

pub async fn resume_torrent(State(s): State<Arc<AppState>>, p: Principal, Path(id): Path<usize>) -> Result<StatusCode, ApiError> {
    require_admin(&p)?;
    engine(&s)?.resume(id).await.map_err(torrent_error)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub struct RemoveTorrentQuery {
    /// Also delete the downloaded data. Off unless asked for.
    #[serde(default)]
    pub delete_files: bool,
}

pub async fn remove_torrent(
    State(s): State<Arc<AppState>>,
    p: Principal,
    Path(id): Path<usize>,
    Query(q): Query<RemoveTorrentQuery>,
) -> Result<StatusCode, ApiError> {
    require_admin(&p)?;
    engine(&s)?.remove(id, q.delete_files).await.map_err(torrent_error)?;
    Ok(StatusCode::NO_CONTENT)
}

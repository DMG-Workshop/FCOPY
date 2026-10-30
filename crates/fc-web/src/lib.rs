//! Local web management dashboard for fcopy.
//!
//! ```text
//! browser ──HTTP──▶ router ─▶ api handlers ─▶ fc_engine::Queue
//!    │                 │
//!    │                 └─ Principal extractor: session cookie | API key
//!    └──WS /ws/telemetry ◀── telemetry producer ◀── Queue events (one reader)
//! ```
//!
//! [`AppState`] holds everything the handlers share. The telemetry producer is
//! the only reader of the queue's event stream: it coalesces updates into
//! frames at 10 Hz and fans them out, so cost doesn't grow with browser count.

pub mod api;
pub mod clients;
pub mod https;
pub mod auth;
pub mod telemetry;

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use axum::Router;
use axum::extract::FromRequestParts;
use axum::http::header::{AUTHORIZATION, COOKIE};
use axum::http::request::Parts;
use axum::http::{Method, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post};
use axum::Json;
use tokio::sync::broadcast;

use fc_engine::{Event, JobId, Queue};
use fc_remote::access::LoginLimiter;
use fc_torrent::TorrentEngine;
use fc_remote::config::Limits;

use auth::{ApiKeys, AuthError, HashCost, Principal, SessionStore, UserStore, SESSION_COOKIE};

/// Header a browser session must send on every state-changing request. A cross-site
/// page cannot set it without a CORS preflight, which this server never grants.
pub const CSRF_HEADER: &str = "x-fcopy-request";

/// Everything the handlers share.
pub struct AppState {
    pub queue: Queue,
    pub users: UserStore,
    pub sessions: SessionStore,
    pub api_keys: ApiKeys,
    pub limiter: LoginLimiter,
    /// Canonical directories that transfers may read from or write to.
    pub roots: Vec<PathBuf>,
    /// Which account created each job.
    pub owners: RwLock<HashMap<JobId, String>>,
    pub telemetry: broadcast::Sender<Arc<telemetry::Frame>>,
    pub clients: clients::ClientStore,
    pub cookie_secure: bool,
    /// The console certificate's fingerprint, shown on the sign-in page to compare by eye.
    pub certificate_fingerprint: Option<String>,
    /// The BitTorrent lane. `None` when the console runs without one; the
    /// torrent routes then answer 404 and the tab stays off.
    pub torrents: Option<TorrentEngine>,
}

/// Settings for [`build`].
pub struct Config {
    pub roots: Vec<PathBuf>,
    pub cookie_secure: bool,
    pub session_ttl: Duration,
    pub hash_cost: HashCost,
    /// Where paired clients are saved. `None` keeps them in memory.
    pub state_dir: Option<PathBuf>,
    pub certificate_fingerprint: Option<String>,
    pub torrents: Option<TorrentEngine>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            roots: vec![],
            cookie_secure: true,
            session_ttl: Duration::from_secs(12 * 3600),
            hash_cost: HashCost::default(),
            state_dir: None,
            certificate_fingerprint: None,
            torrents: None,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum BuildError {
    #[error(transparent)]
    Auth(#[from] AuthError),
    #[error("share root {0} does not exist or is not a directory")]
    BadRoot(PathBuf),
    #[error(transparent)]
    Store(#[from] clients::StoreError),
}

/// Create the shared state and start the telemetry producer. Must be called
/// inside a Tokio runtime. Add accounts with `state.users.add(..)` afterwards.
pub fn build(queue: Queue, events: std::sync::mpsc::Receiver<Event>, config: Config) -> Result<Arc<AppState>, BuildError> {
    let roots = config
        .roots
        .iter()
        .map(|r| r.canonicalize().map_err(|_| BuildError::BadRoot(r.clone())))
        .collect::<Result<Vec<_>, _>>()?;
    let (telemetry, _) = broadcast::channel(64);
    let state = Arc::new(AppState {
        queue,
        users: UserStore::new(config.hash_cost)?,
        sessions: SessionStore::new(config.session_ttl),
        api_keys: ApiKeys::default(),
        limiter: LoginLimiter::new(Limits::default()),
        roots,
        owners: RwLock::new(HashMap::new()),
        telemetry,
        clients: clients::ClientStore::open(config.state_dir.map(|d| d.join("clients.json")))?,
        cookie_secure: config.cookie_secure,
        certificate_fingerprint: config.certificate_fingerprint,
        torrents: config.torrents,
    });
    telemetry::spawn_producer(state.clone(), events);
    Ok(state)
}

const INDEX_HTML: &str = include_str!("../static/index.html");
const APP_JS: &str = include_str!("../static/app.js");
const APP_CSS: &str = include_str!("../static/app.css");
const ALPINE_JS: &str = include_str!("../static/alpine.min.js");

/// Content-Security-Policy for the dashboard: everything comes from this server.
/// Alpine's standard build compiles expressions with `Function`, which needs
/// `unsafe-eval`. Switching to the `alpinejs/csp` build would remove that.
const CSP: &str = "default-src 'self'; script-src 'self' 'unsafe-eval'; style-src 'self'; \
                   img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'";

async fn asset(content_type: &'static str, body: &'static str) -> Response {
    (
        [
            (axum::http::header::CONTENT_TYPE, content_type),
            (axum::http::header::CONTENT_SECURITY_POLICY, CSP),
            (axum::http::header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (axum::http::header::CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
        .into_response()
}

/// The HTTP routes.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(|| asset("text/html; charset=utf-8", INDEX_HTML)))
        .route("/static/app.js", get(|| asset("text/javascript; charset=utf-8", APP_JS)))
        .route("/static/app.css", get(|| asset("text/css; charset=utf-8", APP_CSS)))
        .route("/static/alpine.min.js", get(|| asset("text/javascript; charset=utf-8", ALPINE_JS)))
        .route("/api/auth/login", post(api::login))
        .route("/api/auth/logout", post(api::logout))
        .route("/api/info", get(api::info))
        .route("/api/me", get(api::me))
        .route("/api/transfers", get(api::list_transfers).post(api::create_transfer))
        .route("/api/transfers/{id}", delete(api::cancel_transfer))
        .route("/api/transfers/{id}/pause", post(api::pause_transfer))
        .route("/api/transfers/{id}/resume", post(api::resume_transfer))
        .route("/api/queue", get(api::queue_state))
        .route("/api/queue/pause", post(api::pause_queue))
        .route("/api/queue/resume", post(api::resume_queue))
        .route("/api/clients", get(api::list_clients).post(api::pair_client))
        .route("/api/clients/{id}", delete(api::unpair_client))
        .route("/api/clients/{id}/browse", post(api::browse_client))
        .route("/api/settings", get(api::get_settings).put(api::put_settings))
        .route("/api/settings/vpn", get(api::vpn_providers))
        .route("/api/users", get(api::list_users).post(api::create_user))
        .route("/api/users/{name}", delete(api::delete_user))
        .route("/api/torrents", get(api::list_torrents))
        .route("/api/torrents/magnet", post(api::add_magnet))
        .route(
            "/api/torrents/file",
            post(api::add_torrent_file).layer(axum::extract::DefaultBodyLimit::max(api::MAX_TORRENT_FILE)),
        )
        .route("/api/torrents/{id}", delete(api::remove_torrent))
        .route("/api/torrents/{id}/pause", post(api::pause_torrent))
        .route("/api/torrents/{id}/resume", post(api::resume_torrent))
        .route("/ws/telemetry", get(telemetry::ws_handler))
        .with_state(state)
}

/// Who is calling: a bearer API key, or a session cookie that passes the
/// CSRF header check on state-changing methods.
impl FromRequestParts<Arc<AppState>> for Principal {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &Arc<AppState>) -> Result<Self, Self::Rejection> {
        if let Some(key) = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
        {
            return state.api_keys.lookup(key.trim()).ok_or(ApiError::Unauthorized);
        }

        let token = parts
            .headers
            .get(COOKIE)
            .and_then(|v| v.to_str().ok())
            .and_then(|c| auth::cookie_value(c, SESSION_COOKIE))
            .ok_or(ApiError::Unauthorized)?;
        let principal = state.sessions.lookup(token).ok_or(ApiError::Unauthorized)?;
        if parts.method != Method::GET && !parts.headers.contains_key(CSRF_HEADER) {
            return Err(ApiError::Forbidden("missing X-Fcopy-Request header"));
        }
        Ok(principal)
    }
}

/// Error responses are JSON: `{"error": "..."}`.
#[derive(Debug)]
pub enum ApiError {
    Unauthorized,
    Forbidden(&'static str),
    NotFound,
    BadRequest(String),
    Conflict(String),
    LockedOut(Duration),
    /// A call to a paired agent failed; the status says whether it was refused or unreachable.
    Remote(StatusCode, String),
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, message) = match &self {
            ApiError::Unauthorized => (StatusCode::UNAUTHORIZED, "sign in required".to_string()),
            ApiError::Forbidden(m) => (StatusCode::FORBIDDEN, m.to_string()),
            ApiError::NotFound => (StatusCode::NOT_FOUND, "not found".to_string()),
            ApiError::BadRequest(m) => (StatusCode::BAD_REQUEST, m.clone()),
            ApiError::Conflict(m) => (StatusCode::CONFLICT, m.clone()),
            ApiError::Remote(status, m) => (*status, m.clone()),
            ApiError::LockedOut(wait) => (
                StatusCode::TOO_MANY_REQUESTS,
                format!("too many failed sign-ins; try again in {} s", wait.as_secs().max(1)),
            ),
        };
        (status, Json(serde_json::json!({ "error": message }))).into_response()
    }
}

/// Is `path` inside one of the configured roots? Resolves symlinks, so a link
/// inside a share that points outside it is refused. A path that doesn't exist
/// yet is checked through its parent.
pub fn within_roots(roots: &[PathBuf], path: &Path) -> bool {
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
        return false;
    }
    let base = if path.exists() { Some(path) } else { path.parent() };
    let Some(real) = base.and_then(|b| b.canonicalize().ok()) else { return false };
    roots.iter().any(|root| real.starts_with(root))
}

/// Serve the console over HTTPS on an already-bound listener.
///
/// Set `Config::cookie_secure` to true when calling this, so the session
/// cookie is only sent over TLS.
pub async fn serve(state: Arc<AppState>, listener: https::TlsListener) -> std::io::Result<()> {
    axum::serve(listener, router(state).into_make_service_with_connect_info::<https::PeerAddr>()).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roots_contain_paths_and_refuse_escapes() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let roots = vec![root.path().canonicalize().unwrap()];
        let inside = root.path().join("sub");
        std::fs::create_dir(&inside).unwrap();
        std::fs::write(inside.join("f.txt"), b"x").unwrap();

        assert!(within_roots(&roots, &inside));
        assert!(within_roots(&roots, &inside.join("f.txt")));
        assert!(within_roots(&roots, &inside.join("not-yet-created")), "new name under a real dir");
        assert!(!within_roots(&roots, outside.path()));
        assert!(!within_roots(&roots, &root.path().join("..").join("x")), "parent traversal");
        assert!(!within_roots(&roots, Path::new("relative/path")));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_out_of_a_root_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
        let roots = vec![root.path().canonicalize().unwrap()];
        assert!(!within_roots(&roots, &root.path().join("link")));
    }
}

//! The BitTorrent routes: admin-only, file and magnet adds, pause, resume and
//! remove. Uses a real engine, seeding and removing on this machine only.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::ConnectInfo;
use axum::http::header::{CONTENT_TYPE, COOKIE, SET_COOKIE};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use fc_engine::{Queue, QueueConfig};
use fc_torrent::{EngineConfig, TorrentEngine};
use fc_web::auth::{HashCost, Role};
use fc_web::{AppState, Config, build, router};

struct Env {
    state: Arc<AppState>,
    router: Router,
    dir: tempfile::TempDir,
}

async fn env(with_torrents: bool) -> Env {
    let dir = tempfile::tempdir().unwrap();
    let torrents = if with_torrents {
        Some(TorrentEngine::start(EngineConfig::new(dir.path().join("torrents"))).await.unwrap())
    } else {
        None
    };
    let (queue, events) = Queue::start(QueueConfig { telemetry_interval: Duration::from_millis(20), ..Default::default() });
    let state = build(
        queue,
        events,
        Config {
            roots: vec![dir.path().canonicalize().unwrap()],
            cookie_secure: false,
            session_ttl: Duration::from_secs(3600),
            hash_cost: HashCost { m_kib: 256, t: 1, p: 1 },
            state_dir: None,
            certificate_fingerprint: None,
            torrents,
        },
    )
    .unwrap();
    state.users.add("admin", "admin-password", Role::Admin).unwrap();
    state.users.add("bob", "bobs-password", Role::User).unwrap();
    let router = router(state.clone());
    Env { state, router, dir }
}

struct Reply {
    status: StatusCode,
    json: Value,
    set_cookie: Option<String>,
}

/// One request. `content_type` sets the body's type; `cookie` signs it in.
async fn send(e: &Env, method: &str, uri: &str, cookie: Option<&str>, body: Body, content_type: Option<&str>) -> Reply {
    let mut b = Request::builder().method(method).uri(uri).header("x-fcopy-request", "1");
    if let Some(c) = cookie {
        b = b.header(COOKIE, c);
    }
    if let Some(ct) = content_type {
        b = b.header(CONTENT_TYPE, ct);
    }
    let mut req = b.body(body).unwrap();
    req.extensions_mut().insert(ConnectInfo(fc_web::https::PeerAddr(SocketAddr::from(([127, 0, 0, 1], 40_000)))));
    let resp = e.router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let set_cookie = resp.headers().get(SET_COOKIE).and_then(|v| v.to_str().ok()).map(str::to_string);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let json = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::Null) };
    Reply { status, json, set_cookie }
}

async fn json_call(e: &Env, method: &str, uri: &str, cookie: Option<&str>, body: Value) -> Reply {
    send(e, method, uri, cookie, Body::from(body.to_string()), Some("application/json")).await
}

async fn login(e: &Env, user: &str, password: &str) -> String {
    let r = json_call(e, "POST", "/api/auth/login", None, json!({ "user": user, "password": password })).await;
    assert_eq!(r.status, StatusCode::OK, "login {user}");
    r.set_cookie.unwrap().split(';').next().unwrap().to_string()
}

fn data(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i.wrapping_mul(31) ^ (i >> 3)) as u8).collect()
}

/// A `.torrent` for a file written to `dir`, made by the engine itself.
async fn torrent_for(dir: &std::path::Path, name: &str) -> Vec<u8> {
    let source = dir.join(name);
    std::fs::write(&source, data(128 * 1024)).unwrap();
    TorrentEngine::create_file_torrent(&source, Some(name)).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn torrents_are_off_without_an_engine() {
    let e = env(false).await;
    let info = send(&e, "GET", "/api/info", None, Body::empty(), None).await;
    assert_eq!(info.json["torrents"], false);
    let admin = login(&e, "admin", "admin-password").await;
    let r = send(&e, "GET", "/api/torrents", Some(&admin), Body::empty(), None).await;
    assert_eq!(r.status, StatusCode::NOT_FOUND);
}

#[tokio::test(flavor = "multi_thread")]
async fn torrent_routes_are_admin_only() {
    let e = env(true).await;
    let bob = login(&e, "bob", "bobs-password").await;
    let r = send(&e, "GET", "/api/torrents", Some(&bob), Body::empty(), None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = json_call(&e, "POST", "/api/torrents/magnet", Some(&bob), json!({ "magnet": "magnet:?xt=urn:btih:00" })).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    e.state.torrents.as_ref().unwrap().shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_magnet_must_look_like_one() {
    let e = env(true).await;
    let admin = login(&e, "admin", "admin-password").await;
    let r = json_call(&e, "POST", "/api/torrents/magnet", Some(&admin), json!({ "magnet": "https://example.com" })).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    e.state.torrents.as_ref().unwrap().shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_body_that_is_not_a_torrent_is_refused() {
    let e = env(true).await;
    let admin = login(&e, "admin", "admin-password").await;
    let r = send(&e, "POST", "/api/torrents/file", Some(&admin), Body::from("hello"), Some("application/x-bittorrent")).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    e.state.torrents.as_ref().unwrap().shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_torrent_file_is_added_listed_paused_resumed_and_removed() {
    let e = env(true).await;
    let admin = login(&e, "admin", "admin-password").await;
    let torrents_dir = e.dir.path().join("torrents");
    std::fs::create_dir_all(&torrents_dir).unwrap();
    let file = torrent_for(&torrents_dir, "kept.bin").await;

    // The data is already in the engine's output folder, so it checks and seeds.
    let r = send(&e, "POST", "/api/torrents/file", Some(&admin), Body::from(file), Some("application/x-bittorrent")).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    let id = r.json["id"].as_u64().unwrap();

    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let list = send(&e, "GET", "/api/torrents", Some(&admin), Body::empty(), None).await;
        let row = list.json.as_array().unwrap().iter().find(|t| t["id"] == id).cloned().unwrap_or_default();
        if row["phase"] == "seeding" {
            assert_eq!(row["name"], "kept.bin");
            break;
        }
        assert!(Instant::now() < deadline, "torrent never reached seeding");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let paused = send(&e, "POST", &format!("/api/torrents/{id}/pause"), Some(&admin), Body::empty(), None).await;
    assert_eq!(paused.status, StatusCode::NO_CONTENT);
    let resumed = send(&e, "POST", &format!("/api/torrents/{id}/resume"), Some(&admin), Body::empty(), None).await;
    assert_eq!(resumed.status, StatusCode::NO_CONTENT);

    let kept = torrents_dir.join("kept.bin");
    let gone = send(&e, "DELETE", &format!("/api/torrents/{id}?delete_files=true"), Some(&admin), Body::empty(), None).await;
    assert_eq!(gone.status, StatusCode::NO_CONTENT);
    assert!(!kept.exists(), "delete_files removes the data");
    let list = send(&e, "GET", "/api/torrents", Some(&admin), Body::empty(), None).await;
    assert_eq!(list.json, json!([]));

    let missing = send(&e, "POST", &format!("/api/torrents/{id}/pause"), Some(&admin), Body::empty(), None).await;
    assert_eq!(missing.status, StatusCode::NOT_FOUND);
    e.state.torrents.as_ref().unwrap().shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_bad_peer_address_is_refused() {
    let e = env(true).await;
    let admin = login(&e, "admin", "admin-password").await;
    let file = torrent_for(e.dir.path(), "peers.bin").await;
    let r = send(&e, "POST", "/api/torrents/file?peers=not-an-address", Some(&admin), Body::from(file), Some("application/x-bittorrent")).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
    e.state.torrents.as_ref().unwrap().shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_magnet_link_answers_pending_when_no_peers_send_its_list() {
    let e = env(true).await;
    let admin = login(&e, "admin", "admin-password").await;
    let magnet = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=example";
    let r = json_call(&e, "POST", "/api/torrents/magnet", Some(&admin), json!({ "magnet": magnet })).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    let id = r.json["id"].as_u64().unwrap();

    // The row is listed at once, as waiting, under the id the add returned.
    let list = send(&e, "GET", "/api/torrents", Some(&admin), Body::empty(), None).await;
    let row = list.json.as_array().unwrap().iter().find(|t| t["id"] == id).cloned().unwrap_or_default();
    assert_eq!(row["phase"], "waiting");
    assert_eq!(row["name"], "example");
    e.state.torrents.as_ref().unwrap().shutdown().await;
}

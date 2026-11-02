//! The read-only disk routes: administrators only, no serial numbers, and health
//! only for a disk the enumeration returned.

use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::ConnectInfo;
use axum::http::header::{COOKIE, SET_COOKIE};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use fc_engine::{Queue, QueueConfig};
use fc_web::auth::{HashCost, Role};
use fc_web::{Config, build, router};

struct Env {
    router: Router,
    _dir: tempfile::TempDir,
}

async fn env() -> Env {
    let dir = tempfile::tempdir().unwrap();
    let (queue, events) = Queue::start(QueueConfig {
        telemetry_interval: Duration::from_millis(20),
        ..Default::default()
    });
    let state = build(
        queue,
        events,
        Config {
            roots: vec![dir.path().canonicalize().unwrap()],
            cookie_secure: false,
            session_ttl: Duration::from_secs(3600),
            hash_cost: HashCost {
                m_kib: 256,
                t: 1,
                p: 1,
            },
            state_dir: None,
            certificate_fingerprint: None,
            torrents: None,
        },
    )
    .unwrap();
    state
        .users
        .add("admin", "admin-password", Role::Admin)
        .unwrap();
    state.users.add("bob", "bobs-password", Role::User).unwrap();
    let router = router(state.clone());
    Env { router, _dir: dir }
}

struct Reply {
    status: StatusCode,
    body: String,
    json: Value,
}

async fn get(e: &Env, uri: &str, cookie: Option<&str>) -> Reply {
    let mut b = Request::builder()
        .method("GET")
        .uri(uri)
        .header("x-fcopy-request", "1");
    if let Some(c) = cookie {
        b = b.header(COOKIE, c);
    }
    let mut req = b.body(Body::empty()).unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(fc_web::https::PeerAddr(SocketAddr::from((
            [127, 0, 0, 1],
            40_000,
        )))));
    let resp = e.router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let body = String::from_utf8_lossy(&bytes).into_owned();
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    Reply { status, body, json }
}

async fn login(e: &Env, user: &str, password: &str) -> String {
    let body = json!({ "user": user, "password": password }).to_string();
    let mut req = Request::builder()
        .method("POST")
        .uri("/api/auth/login")
        .header("x-fcopy-request", "1")
        .header("content-type", "application/json")
        .body(Body::from(body))
        .unwrap();
    req.extensions_mut()
        .insert(ConnectInfo(fc_web::https::PeerAddr(SocketAddr::from((
            [127, 0, 0, 1],
            40_000,
        )))));
    let resp = e.router.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "login {user}");
    resp.headers()
        .get(SET_COOKIE)
        .and_then(|v| v.to_str().ok())
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn storage_routes_refuse_anonymous_and_non_admin_callers() {
    let e = env().await;
    let anon = get(&e, "/api/storage/disks", None).await;
    assert_eq!(anon.status, StatusCode::UNAUTHORIZED);

    let bob = login(&e, "bob", "bobs-password").await;
    let r = get(&e, "/api/storage/disks", Some(&bob)).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = get(&e, "/api/storage/health?device=/dev/vda", Some(&bob)).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

// Disk inspection reads Linux sysfs, so these two run on Linux only. Elsewhere the
// routes answer 501 (see the test below).
#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn an_administrator_sees_disks_without_serial_numbers() {
    let e = env().await;
    let admin = login(&e, "admin", "admin-password").await;
    let r = get(&e, "/api/storage/disks", Some(&admin)).await;
    assert_eq!(r.status, StatusCode::OK);
    assert!(r.json.is_array(), "a list of disks: {}", r.body);
    assert!(
        !r.body.to_lowercase().contains("serial"),
        "no serial field is returned: {}",
        r.body
    );
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "multi_thread")]
async fn health_is_refused_for_a_path_that_is_not_a_listed_disk() {
    let e = env().await;
    let admin = login(&e, "admin", "admin-password").await;
    for device in ["/etc/passwd", "/dev/sda --option", "../../dev/mem"] {
        let uri = format!("/api/storage/health?device={}", device.replace(' ', "%20"));
        let r = get(&e, &uri, Some(&admin)).await;
        assert_eq!(r.status, StatusCode::NOT_FOUND, "device {device}");
    }
}

#[cfg(not(target_os = "linux"))]
#[tokio::test(flavor = "multi_thread")]
async fn disk_inspection_says_it_is_unavailable_outside_linux() {
    let e = env().await;
    let admin = login(&e, "admin", "admin-password").await;
    let r = get(&e, "/api/storage/disks", Some(&admin)).await;
    assert_eq!(r.status, StatusCode::NOT_IMPLEMENTED, "body: {}", r.body);
    assert!(r.body.contains("Linux"), "says why: {}", r.body);
}

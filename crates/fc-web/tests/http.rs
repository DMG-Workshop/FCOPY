//! HTTP-level tests: the real router, driven request by request.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::ConnectInfo;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE, COOKIE, SET_COOKIE};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use fc_engine::{CopyOptions, Queue, QueueConfig};
use fc_web::auth::{HashCost, Principal, Role};
use fc_web::{AppState, Config, build, router};

struct App {
    state: Arc<AppState>,
    router: Router,
    root: tempfile::TempDir,
}

async fn setup() -> App {
    let root = tempfile::tempdir().unwrap();
    let (queue, events) = Queue::start(QueueConfig {
        telemetry_interval: Duration::from_millis(20),
        options: CopyOptions { chunk_size: 64 * 1024, ..Default::default() },
        ..Default::default()
    });
    let state = build(
        queue,
        events,
        Config {
            roots: vec![root.path().to_path_buf()],
            cookie_secure: false,
            session_ttl: Duration::from_secs(3600),
            hash_cost: HashCost { m_kib: 256, t: 1, p: 1 },
            state_dir: None,
            certificate_fingerprint: None,
            torrents: None,
        },
    )
    .unwrap();
    state.users.add("alice", "password1", Role::Admin).unwrap();
    state.users.add("bob", "bobs-password", Role::User).unwrap();
    let router = router(state.clone());
    App { state, router, root }
}

struct Reply {
    status: StatusCode,
    json: Value,
    set_cookie: Option<String>,
}

async fn send(app: &App, method: &str, uri: &str, headers: &[(&str, &str)], body: Option<Value>) -> Reply {
    let mut b = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let body = match body {
        Some(v) => {
            b = b.header(CONTENT_TYPE, "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let mut req = b.body(body).unwrap();
    req.extensions_mut().insert(ConnectInfo(fc_web::https::PeerAddr(SocketAddr::from(([127, 0, 0, 1], 40_000)))));
    let resp = app.router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let set_cookie = resp.headers().get(SET_COOKIE).and_then(|v| v.to_str().ok()).map(str::to_string);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let json = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::Null) };
    Reply { status, json, set_cookie }
}

/// Log in and return the `fc_session=…` pair to send back as a `Cookie` header.
async fn login(app: &App, user: &str, password: &str) -> String {
    let r = send(app, "POST", "/api/auth/login", &[], Some(json!({ "user": user, "password": password }))).await;
    assert_eq!(r.status, StatusCode::OK, "login {user}");
    let set = r.set_cookie.expect("login sets a cookie");
    assert!(set.contains("HttpOnly") && set.contains("SameSite=Strict"), "{set}");
    set.split(';').next().unwrap().to_string()
}

const CSRF: (&str, &str) = ("x-fcopy-request", "1");

#[tokio::test]
async fn login_me_and_logout() {
    let app = setup().await;
    let cookie = login(&app, "alice", "password1").await;

    let me = send(&app, "GET", "/api/me", &[(COOKIE.as_str(), cookie.as_str())], None).await;
    assert_eq!(me.status, StatusCode::OK);
    assert_eq!(me.json["name"], "alice");
    assert_eq!(me.json["role"], "admin");

    let out = send(&app, "POST", "/api/auth/logout", &[(COOKIE.as_str(), cookie.as_str()), CSRF], None).await;
    assert_eq!(out.status, StatusCode::NO_CONTENT);
    assert!(out.set_cookie.unwrap().contains("Max-Age=0"));

    let after = send(&app, "GET", "/api/me", &[(COOKIE.as_str(), cookie.as_str())], None).await;
    assert_eq!(after.status, StatusCode::UNAUTHORIZED, "logout revokes the session on the server");
}

#[tokio::test]
async fn wrong_password_and_unknown_user_look_the_same() {
    let app = setup().await;
    let wrong = send(&app, "POST", "/api/auth/login", &[], Some(json!({ "user": "alice", "password": "nope-nope" }))).await;
    let ghost = send(&app, "POST", "/api/auth/login", &[], Some(json!({ "user": "ghost", "password": "nope-nope" }))).await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
    assert_eq!(ghost.status, StatusCode::UNAUTHORIZED);
    assert_eq!(wrong.json, ghost.json, "same body, so names can't be enumerated");
    assert!(wrong.set_cookie.is_none());
}

#[tokio::test]
async fn repeated_failures_lock_the_address_out() {
    let app = setup().await;
    let mut saw_lockout = false;
    for _ in 0..10 {
        let r = send(&app, "POST", "/api/auth/login", &[], Some(json!({ "user": "bob", "password": "guess-guess" }))).await;
        saw_lockout |= r.status == StatusCode::TOO_MANY_REQUESTS;
    }
    assert!(saw_lockout, "the login limiter must engage");
    // Even the right password is refused while locked out.
    let r = send(&app, "POST", "/api/auth/login", &[], Some(json!({ "user": "bob", "password": "bobs-password" }))).await;
    assert_eq!(r.status, StatusCode::TOO_MANY_REQUESTS);
}

#[tokio::test]
async fn unauthenticated_requests_are_refused() {
    let app = setup().await;
    assert_eq!(send(&app, "GET", "/api/transfers", &[], None).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(send(&app, "GET", "/api/me", &[(COOKIE.as_str(), "fc_session=garbage")], None).await.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn cookie_writes_need_the_csrf_header() {
    let app = setup().await;
    let cookie = login(&app, "alice", "password1").await;
    let body = json!({ "sources": ["/nope"], "dest": "/nope" });
    let no_header = send(&app, "POST", "/api/transfers", &[(COOKIE.as_str(), cookie.as_str())], Some(body)).await;
    assert_eq!(no_header.status, StatusCode::FORBIDDEN, "cross-site form posts must not work");
}

#[tokio::test]
async fn users_see_only_their_own_transfers() {
    let app = setup().await;
    std::fs::write(app.root.path().join("a.txt"), b"hello").unwrap();
    let alice = login(&app, "alice", "password1").await;
    let bob = login(&app, "bob", "bobs-password").await;
    app.state.queue.pause(); // keep the job pending so it stays listed

    let created = send(
        &app,
        "POST",
        "/api/transfers",
        &[(COOKIE.as_str(), alice.as_str()), CSRF],
        Some(json!({ "sources": [app.root.path().join("a.txt")], "dest": app.root.path().join("out") })),
    )
    .await;
    assert_eq!(created.status, StatusCode::ACCEPTED);
    let id = created.json["id"].as_u64().unwrap();

    let bob_list = send(&app, "GET", "/api/transfers", &[(COOKIE.as_str(), bob.as_str())], None).await;
    assert_eq!(bob_list.json.as_array().unwrap().len(), 0, "bob must not see alice's job");

    let alice_list = send(&app, "GET", "/api/transfers", &[(COOKIE.as_str(), alice.as_str())], None).await;
    assert_eq!(alice_list.json[0]["owner"], "alice");

    // Bob cannot touch alice's job, and the answer does not reveal that it exists.
    let bob_pause = send(&app, "POST", &format!("/api/transfers/{id}/pause"), &[(COOKIE.as_str(), bob.as_str()), CSRF], None).await;
    assert_eq!(bob_pause.status, StatusCode::NOT_FOUND);
    let bob_missing = send(&app, "POST", "/api/transfers/999999/pause", &[(COOKIE.as_str(), bob.as_str()), CSRF], None).await;
    assert_eq!(bob_missing.json, bob_pause.json);

    app.state.queue.resume();
}

#[tokio::test]
async fn pause_resume_and_cancel_a_job_over_http() {
    let app = setup().await;
    std::fs::write(app.root.path().join("a.txt"), b"hello").unwrap();
    let alice = login(&app, "alice", "password1").await;
    app.state.queue.pause(); // hold the worker so the job stays pending

    let created = send(
        &app,
        "POST",
        "/api/transfers",
        &[(COOKIE.as_str(), alice.as_str()), CSRF],
        Some(json!({ "sources": [app.root.path().join("a.txt")], "dest": app.root.path().join("out"), "label": "docs" })),
    )
    .await;
    let id = created.json["id"].as_u64().unwrap();

    let p = send(&app, "POST", &format!("/api/transfers/{id}/pause"), &[(COOKIE.as_str(), alice.as_str()), CSRF], None).await;
    assert_eq!(p.status, StatusCode::NO_CONTENT);
    let list = send(&app, "GET", "/api/transfers", &[(COOKIE.as_str(), alice.as_str())], None).await;
    assert_eq!(list.json[0]["paused"], true);
    assert_eq!(list.json[0]["label"], "docs");

    let r = send(&app, "POST", &format!("/api/transfers/{id}/resume"), &[(COOKIE.as_str(), alice.as_str()), CSRF], None).await;
    assert_eq!(r.status, StatusCode::NO_CONTENT);
    let list = send(&app, "GET", "/api/transfers", &[(COOKIE.as_str(), alice.as_str())], None).await;
    assert_eq!(list.json[0]["paused"], false);

    let c = send(&app, "DELETE", &format!("/api/transfers/{id}"), &[(COOKIE.as_str(), alice.as_str()), CSRF], None).await;
    assert_eq!(c.status, StatusCode::NO_CONTENT);
    app.state.queue.resume();
}

#[tokio::test]
async fn transfers_outside_the_shares_are_refused() {
    let app = setup().await;
    let alice = login(&app, "alice", "password1").await;
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("secret.txt"), b"x").unwrap();

    let cases = [
        json!({ "sources": [outside.path().join("secret.txt")], "dest": app.root.path() }),
        json!({ "sources": [app.root.path().join("..").join("secret.txt")], "dest": app.root.path() }),
        json!({ "sources": [], "dest": app.root.path() }),
        json!({ "sources": [app.root.path().join("missing.txt")], "dest": app.root.path() }),
        json!({ "sources": [outside.path().join("secret.txt")], "dest": outside.path() }),
    ];
    for body in cases {
        let r = send(&app, "POST", "/api/transfers", &[(COOKIE.as_str(), alice.as_str()), CSRF], Some(body.clone())).await;
        assert_eq!(r.status, StatusCode::BAD_REQUEST, "{body}");
    }
}

#[tokio::test]
async fn api_keys_work_for_scripts_without_a_cookie() {
    let app = setup().await;
    let key = app.state.api_keys.create(Principal { name: "nightly".into(), role: Role::User });
    let bearer = format!("Bearer {key}");
    let r = send(&app, "GET", "/api/me", &[(AUTHORIZATION.as_str(), bearer.as_str())], None).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(r.json["name"], "nightly");

    // Keys are not cookies: a wrong key is refused.
    let wrong = send(&app, "GET", "/api/me", &[(AUTHORIZATION.as_str(), "Bearer not-a-key")], None).await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn dashboard_assets_are_served_with_a_strict_policy() {
    let app = setup().await;
    for (uri, ctype) in [
        ("/", "text/html"),
        ("/static/app.js", "text/javascript"),
        ("/static/app.css", "text/css"),
        ("/static/alpine.min.js", "text/javascript"),
    ] {
        let mut req = Request::builder().method("GET").uri(uri).body(Body::empty()).unwrap();
        req.extensions_mut().insert(ConnectInfo(fc_web::https::PeerAddr(SocketAddr::from(([127, 0, 0, 1], 40_000)))));
        let resp = app.router.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{uri}");
        let headers = resp.headers();
        assert!(headers["content-type"].to_str().unwrap().starts_with(ctype), "{uri}");
        let csp = headers["content-security-policy"].to_str().unwrap();
        assert!(csp.contains("default-src 'self'") && csp.contains("frame-ancestors 'none'"), "{csp}");
        assert_eq!(headers["x-content-type-options"], "nosniff");
    }
}

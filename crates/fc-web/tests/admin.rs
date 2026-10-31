//! Admin endpoints: pairing against a real agent over loopback QUIC, runtime
//! settings, and account management.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::connect_info::ConnectInfo;
use axum::http::header::{COOKIE, CONTENT_TYPE, SET_COOKIE};
use axum::http::{Request, StatusCode};
use serde_json::{Value, json};
use tower::ServiceExt;

use fc_engine::{CopyOptions, Queue, QueueConfig};
use fc_remote::config::{AccountKind, AgentConfig, Share};
use fc_remote::protocol::KdfParams;
use fc_remote::server::{Server, ServerHandle};
use fc_remote::tls::Identity;
use fc_web::auth::{HashCost, Role};
use fc_web::{AppState, Config, build, router};

struct Env {
    state: Arc<AppState>,
    /// Local share root the dashboard may read from.
    root: tempfile::TempDir,
    router: Router,
    state_dir: tempfile::TempDir,
    _share: tempfile::TempDir,
    agent: ServerHandle,
    agent_port: u16,
    agent_fp: String,
}

/// A dashboard with an admin and a regular user, plus a real agent to pair with.
async fn env() -> Env {
    let share = tempfile::tempdir().unwrap();
    let mut cfg = AgentConfig::new("nas");
    cfg.listen = "127.0.0.1:0".parse().unwrap();
    cfg.kdf = KdfParams { m_cost: 256, t_cost: 1, p_cost: 1 };
    cfg.shares.push(Share { name: "backup".into(), path: share.path().into(), writable: true });
    cfg.set_account("alice", AccountKind::Interactive, "agent-pass-1", None).unwrap();
    let identity = Identity::generate("127.0.0.1").unwrap();
    let server = Server::bind(cfg, &identity).unwrap();
    let agent = server.handle();
    let agent_port = agent.local_addr().unwrap().port();
    let agent_fp = agent.fingerprint().to_string();
    tokio::spawn(server.run());

    let state_dir = tempfile::tempdir().unwrap();
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
            state_dir: Some(state_dir.path().to_path_buf()),
            certificate_fingerprint: None,
            torrents: None,
        },
    )
    .unwrap();
    state.users.add("admin", "admin-password", Role::Admin).unwrap();
    state.users.add("bob", "bobs-password", Role::User).unwrap();
    let router = router(state.clone());
    Env { state, router, root, state_dir, _share: share, agent, agent_port, agent_fp }
}

struct Reply {
    status: StatusCode,
    json: Value,
    set_cookie: Option<String>,
}

async fn send(e: &Env, method: &str, uri: &str, cookie: Option<&str>, body: Option<Value>) -> Reply {
    let mut b = Request::builder().method(method).uri(uri).header("x-fcopy-request", "1");
    if let Some(c) = cookie {
        b = b.header(COOKIE, c);
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
    let resp = e.router.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let set_cookie = resp.headers().get(SET_COOKIE).and_then(|v| v.to_str().ok()).map(str::to_string);
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
    let json = if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::Null) };
    Reply { status, json, set_cookie }
}

async fn login(e: &Env, user: &str, password: &str) -> String {
    let r = send(e, "POST", "/api/auth/login", None, Some(json!({ "user": user, "password": password }))).await;
    assert_eq!(r.status, StatusCode::OK, "login {user}");
    r.set_cookie.unwrap().split(';').next().unwrap().to_string()
}

fn target(e: &Env) -> String {
    format!("alice@127.0.0.1:{}:/backup", e.agent_port)
}

async fn history_entries(e: &Env, cookie: &str, count: usize) -> Value {
    for _ in 0..200 {
        let response = send(e, "GET", "/api/history", Some(cookie), None).await;
        if response.status == StatusCode::OK && response.json["entries"].as_array().unwrap().len() == count {
            return response.json;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("history did not reach {count} entries");
}

async fn history_job(e: &Env, cookie: &str, name: &str) -> u64 {
    let src = e.root.path().join(name);
    std::fs::write(&src, b"history bytes").unwrap();
    let response = send(e, "POST", "/api/transfers", Some(cookie), Some(json!({
        "sources": [src], "dest": e.root.path().join("history-out"), "label": name,
    }))).await;
    assert_eq!(response.status, StatusCode::ACCEPTED);
    response.json["id"].as_u64().unwrap()
}

async fn restart_recovery(e: &mut Env) {
    e.state.queue.shutdown();
    let (queue, events) = Queue::start(QueueConfig::default());
    let state = build(queue, events, Config {
        roots: vec![e.root.path().into()], cookie_secure: false,
        hash_cost: HashCost { m_kib: 256, t: 1, p: 1 },
        state_dir: Some(e.state_dir.path().into()), ..Default::default()
    }).unwrap();
    state.users.add("admin", "admin-password", Role::Admin).unwrap();
    state.users.add("bob", "bobs-password", Role::User).unwrap();
    e.router = router(state.clone());
    e.state = state;
    // Let any old event-reader references finish and release advisory claims.
    tokio::time::sleep(Duration::from_millis(150)).await;
}

#[tokio::test]
async fn recovery_restarts_paused_and_resumes_with_content_comparison() {
    let mut e = env().await;
    let bob = login(&e, "bob", "bobs-password").await;
    e.state.queue.pause();
    history_job(&e, &bob, "recover-local").await;
    assert!(send(&e, "GET", "/api/recovery", Some(&bob), None).await.json["jobs"].as_array().unwrap().is_empty(), "active records are not recoverable twice");
    restart_recovery(&mut e).await;
    let bob = login(&e, "bob", "bobs-password").await;
    let rows = send(&e, "GET", "/api/recovery", Some(&bob), None).await;
    assert_eq!(rows.status, StatusCode::OK);
    assert_eq!(rows.json["jobs"][0]["state"], "paused");
    assert_eq!(rows.json["persistent"], true);
    assert!(e.state.queue.jobs().is_empty());
    assert!(!e.root.path().join("history-out").exists());
    let id = rows.json["jobs"][0]["id"].as_u64().unwrap();
    let out = e.root.path().join("history-out");
    std::fs::create_dir(&out).unwrap();
    std::fs::write(out.join("recover-local"), b"history bytes").unwrap();
    let response = send(&e, "POST", &format!("/api/recovery/{id}"), Some(&bob), Some(json!({"action":"resume"}))).await;
    assert_eq!(response.status, StatusCode::ACCEPTED, "{:?}", response.json);
    let history = history_entries(&e, &bob, 1).await;
    assert_eq!(history["entries"][0]["files_copied"], 0);
    assert_eq!(history["entries"][0]["files_skipped"], 1);
    assert_eq!(std::fs::read(out.join("recover-local")).unwrap(), b"history bytes");
    assert!(send(&e, "GET", "/api/recovery", Some(&bob), None).await.json["jobs"].as_array().unwrap().is_empty());
    assert_eq!(send(&e, "POST", &format!("/api/recovery/{id}"), Some(&bob), Some(json!({"action":"resume"}))).await.status, StatusCode::NOT_FOUND);
    e.state.queue.shutdown();
    e.agent.shutdown();
}

#[tokio::test]
async fn recovery_is_owner_scoped_csrf_protected_and_discard_keeps_partial_data() {
    let mut e = env().await;
    let bob = login(&e, "bob", "bobs-password").await;
    let admin = login(&e, "admin", "admin-password").await;
    e.state.queue.pause();
    history_job(&e, &bob, "bob-recovery").await;
    history_job(&e, &admin, "admin-recovery").await;
    restart_recovery(&mut e).await;
    let bob = login(&e, "bob", "bobs-password").await;
    let admin = login(&e, "admin", "admin-password").await;
    assert_eq!(send(&e, "GET", "/api/recovery", None, None).await.status, StatusCode::UNAUTHORIZED);
    let own = send(&e, "GET", "/api/recovery", Some(&bob), None).await.json;
    let all = send(&e, "GET", "/api/recovery", Some(&admin), None).await.json;
    assert_eq!(own["jobs"].as_array().unwrap().len(), 1);
    assert_eq!(all["jobs"].as_array().unwrap().len(), 2);
    let other = all["jobs"].as_array().unwrap().iter().find(|j| j["owner"] == "admin").unwrap()["id"].as_u64().unwrap();
    assert_eq!(send(&e, "POST", &format!("/api/recovery/{other}"), Some(&bob), Some(json!({"action":"discard"}))).await.status, StatusCode::NOT_FOUND);
    let id = own["jobs"][0]["id"].as_u64().unwrap();
    let uri = format!("/api/recovery/{id}");
    let mut request = Request::builder().method("POST").uri(&uri).header(COOKIE, &bob).header(CONTENT_TYPE, "application/json")
        .body(Body::from(r#"{"action":"discard"}"#)).unwrap();
    request.extensions_mut().insert(ConnectInfo(fc_web::https::PeerAddr(SocketAddr::from(([127, 0, 0, 1], 40_000)))));
    assert_eq!(e.router.clone().oneshot(request).await.unwrap().status(), StatusCode::FORBIDDEN);
    let partial = e.root.path().join("history-out/bob-recovery.fcpart");
    std::fs::create_dir(partial.parent().unwrap()).unwrap();
    std::fs::write(&partial, b"preserve partial").unwrap();
    assert_eq!(send(&e, "POST", &uri, Some(&bob), Some(json!({"action":"discard"}))).await.status, StatusCode::NO_CONTENT);
    assert_eq!(std::fs::read(partial).unwrap(), b"preserve partial");
    assert!(e.state.queue.jobs().is_empty());
    // Admin resuming Bob's jobs must keep Bob as the owner (covered below by a new record).
    e.state.queue.pause();
    history_job(&e, &bob, "admin-resumes-bob").await;
    restart_recovery(&mut e).await;
    let admin = login(&e, "admin", "admin-password").await;
    let rows = send(&e, "GET", "/api/recovery", Some(&admin), None).await.json;
    let id = rows["jobs"].as_array().unwrap().iter().find(|j| j["label"] == "admin-resumes-bob").unwrap()["id"].as_u64().unwrap();
    assert_eq!(send(&e, "POST", &format!("/api/recovery/{id}"), Some(&admin), Some(json!({"action":"retry"}))).await.status, StatusCode::ACCEPTED);
    let bob = login(&e, "bob", "bobs-password").await;
    assert_eq!(history_entries(&e, &bob, 1).await["entries"][0]["owner"], "bob");
    e.state.queue.shutdown();
    e.agent.shutdown();
}

#[tokio::test]
async fn recovery_save_failure_prevents_execution_and_changed_roots_are_rechecked() {
    let mut e = env().await;
    let bob = login(&e, "bob", "bobs-password").await;
    let src = e.root.path().join("not-started");
    std::fs::write(&src, b"safe").unwrap();
    let blocker = e.state_dir.path().join("unfinished-jobs.tmp");
    std::fs::create_dir(&blocker).unwrap();
    let r = send(&e, "POST", "/api/transfers", Some(&bob), Some(json!({"sources":[src],"dest":e.root.path().join("out")}))).await;
    assert_eq!(r.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(r.json["error"].as_str().unwrap().contains("not started"));
    assert!(e.state.queue.jobs().is_empty());
    assert!(!e.root.path().join("out").exists());
    std::fs::remove_dir(blocker).unwrap();
    e.state.queue.pause();
    history_job(&e, &bob, "root-changes").await;
    restart_recovery(&mut e).await;
    let bob = login(&e, "bob", "bobs-password").await;
    let id = send(&e, "GET", "/api/recovery", Some(&bob), None).await.json["jobs"][0]["id"].as_u64().unwrap();
    // A once-valid source has disappeared. Recovery must not blindly enqueue it.
    std::fs::remove_file(e.root.path().join("root-changes")).unwrap();
    assert_eq!(send(&e, "POST", &format!("/api/recovery/{id}"), Some(&bob), Some(json!({"action":"resume"}))).await.status, StatusCode::BAD_REQUEST);
    assert!(e.state.queue.jobs().is_empty());
    assert_eq!(send(&e, "GET", "/api/recovery", Some(&bob), None).await.json["jobs"].as_array().unwrap().len(), 1);
    // A formerly allowed root is not necessarily shared by the current config.
    let former_root = tempfile::tempdir().unwrap();
    let source = former_root.path().join("outside-current-shares");
    std::fs::write(&source, b"former share").unwrap();
    let request = fc_engine::TransferRequest::into_dir(vec![source], e.root.path().join("out"));
    let job = fc_remote::recovery::Job::new(&request, fc_remote::recovery::Destination::Directory { path: e.root.path().join("out") }, Some("bob".into()), &CopyOptions::default()).unwrap();
    let mut store = fc_remote::recovery::RecoveryStore::open(Some(e.state_dir.path().join("unfinished-jobs.toml"))).unwrap();
    let outside = store.add(job).unwrap();
    assert_eq!(send(&e, "POST", &format!("/api/recovery/{outside}"), Some(&bob), Some(json!({"action":"resume"}))).await.status, StatusCode::BAD_REQUEST);
    assert!(e.state.queue.jobs().is_empty());
    e.state.queue.shutdown();
    e.agent.shutdown();
}

#[tokio::test]
async fn recovery_keeps_active_jobs_on_shutdown_but_not_explicit_cancel() {
    let mut e = env().await;
    let bob = login(&e, "bob", "bobs-password").await;
    std::fs::create_dir(e.root.path().join("history-out")).unwrap();
    std::fs::write(e.root.path().join("history-out/active-shutdown"), b"different").unwrap();
    history_job(&e, &bob, "active-shutdown").await;
    wait_for_conflict(&e, &bob, None).await;
    e.state.queue.shutdown();
    history_entries(&e, &bob, 1).await;
    restart_recovery(&mut e).await;
    let bob = login(&e, "bob", "bobs-password").await;
    let rows = send(&e, "GET", "/api/recovery", Some(&bob), None).await.json;
    assert_eq!(rows["jobs"].as_array().unwrap().len(), 1);
    let id = rows["jobs"][0]["id"].as_u64().unwrap();
    let resumed = send(&e, "POST", &format!("/api/recovery/{id}"), Some(&bob), Some(json!({"action":"resume"}))).await;
    assert_eq!(resumed.status, StatusCode::ACCEPTED);
    let conflict = wait_for_conflict(&e, &bob, None).await;
    assert_eq!(conflict["different"], true, "Resume must ask again for different content");
    let live_id = resumed.json["id"].as_u64().unwrap();
    assert_eq!(send(&e, "POST", &format!("/api/recovery/{id}"), Some(&bob), Some(json!({"action":"retry"}))).await.status, StatusCode::CONFLICT);
    assert_eq!(send(&e, "DELETE", &format!("/api/transfers/{live_id}"), Some(&bob), None).await.status, StatusCode::NO_CONTENT);
    history_entries(&e, &bob, 2).await;
    assert!(send(&e, "GET", "/api/recovery", Some(&bob), None).await.json["jobs"].as_array().unwrap().is_empty());
    assert_eq!(std::fs::read(e.root.path().join("history-out/active-shutdown")).unwrap(), b"different");
    e.state.queue.shutdown();
    e.agent.shutdown();
}

#[tokio::test]
async fn recovery_remote_jobs_prompt_again_and_bad_credentials_preserve_recovery() {
    let mut e = env().await;
    let admin = login(&e, "admin", "admin-password").await;
    let client = paired_client(&e, &admin).await;
    std::fs::write(e._share.path().join("remote-source"), b"download me").unwrap();
    e.state.queue.pause();
    for (direction, sources, dest) in [
        ("upload", vec![e.root.path().join("upload-source")], std::path::PathBuf::from("backup/recovered")),
        ("download", vec![std::path::PathBuf::from("backup/remote-source")], e.root.path().join("downloaded")),
    ] {
        if direction == "upload" { std::fs::write(&sources[0], b"upload me").unwrap(); }
        let r = send(&e, "POST", "/api/transfers", Some(&admin), Some(json!({
            "sources": sources, "dest":dest, "remote":{"client_id":client,"password":"agent-pass-1","direction":direction}
        }))).await;
        assert_eq!(r.status, StatusCode::ACCEPTED, "{:?}", r.json);
    }
    let saved = std::fs::read_to_string(e.state_dir.path().join("unfinished-jobs.toml")).unwrap();
    assert!(!saved.contains("agent-pass-1") && !saved.contains("password") && !saved.contains("token"));
    restart_recovery(&mut e).await;
    let admin = login(&e, "admin", "admin-password").await;
    let rows = send(&e, "GET", "/api/recovery", Some(&admin), None).await.json;
    assert_eq!(rows["jobs"].as_array().unwrap().len(), 2);
    assert!(!rows.to_string().contains("agent-pass-1"));
    assert!(e.state.queue.jobs().is_empty());
    assert!(!e.root.path().join("downloaded").exists());
    let upload = rows["jobs"].as_array().unwrap().iter().find(|j| j["destination"]["kind"] == "upload").unwrap()["id"].as_u64().unwrap();
    let uri = format!("/api/recovery/{upload}");
    assert_eq!(send(&e, "POST", &uri, Some(&admin), Some(json!({"action":"resume"}))).await.status, StatusCode::BAD_REQUEST);
    assert!(e.state.queue.jobs().is_empty());
    assert_eq!(send(&e, "POST", &uri, Some(&admin), Some(json!({"action":"resume","password":"wrong-password"}))).await.status, StatusCode::ACCEPTED);
    history_entries(&e, &admin, 1).await;
    let rows = send(&e, "GET", "/api/recovery", Some(&admin), None).await.json;
    assert_eq!(rows["jobs"].as_array().unwrap().len(), 2, "failed authentication must not lose the saved job");
    assert_eq!(send(&e, "POST", &uri, Some(&admin), Some(json!({"action":"resume","password":"agent-pass-1"}))).await.status, StatusCode::ACCEPTED);
    history_entries(&e, &admin, 2).await;
    let download = rows["jobs"].as_array().unwrap().iter().find(|j| j["destination"]["kind"] == "download").unwrap()["id"].as_u64().unwrap();
    assert_eq!(send(&e, "POST", &format!("/api/recovery/{download}"), Some(&admin), Some(json!({"action":"resume","password":"agent-pass-1"}))).await.status, StatusCode::ACCEPTED);
    history_entries(&e, &admin, 3).await;
    assert_eq!(std::fs::read(e._share.path().join("recovered/upload-source")).unwrap(), b"upload me");
    assert_eq!(std::fs::read(e.root.path().join("downloaded/remote-source")).unwrap(), b"download me");
    assert!(send(&e, "GET", "/api/recovery", Some(&admin), None).await.json["jobs"].as_array().unwrap().is_empty());
    e.state.queue.shutdown();
    e.agent.shutdown();
}

#[tokio::test]
async fn history_survives_restart_filters_owners_and_clears_without_touching_files() {
    let mut e = env().await;
    assert_eq!(send(&e, "GET", "/api/history", None, None).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(send(&e, "DELETE", "/api/history", None, None).await.status, StatusCode::UNAUTHORIZED);
    let bob = login(&e, "bob", "bobs-password").await;
    let admin = login(&e, "admin", "admin-password").await;
    let first_job = history_job(&e, &bob, "bob-first").await;
    history_job(&e, &admin, "admin-first").await;
    let bob_history = history_entries(&e, &bob, 1).await;
    assert_eq!(bob_history["persistent"], true);
    assert_eq!(bob_history["entries"][0]["owner"], "bob");
    let all = history_entries(&e, &admin, 2).await;
    let oldest_id = all["entries"][1]["id"].as_u64().unwrap();
    assert_eq!(all["entries"][1]["outcome"], "completed");
    assert_eq!(all["entries"][1]["files_copied"], 1);
    assert_eq!(all["entries"][1]["bytes_copied"], 13);
    assert!(all["entries"][1]["finished_at_ms"].as_u64().unwrap() >= all["entries"][1]["queued_at_ms"].as_u64().unwrap());
    // Removing the queue row does not remove its independent history record.
    assert_eq!(send(&e, "DELETE", &format!("/api/transfers/{first_job}"), Some(&bob), None).await.status, StatusCode::NO_CONTENT);
    assert_eq!(history_entries(&e, &bob, 1).await["entries"][0]["id"], oldest_id);
    e.state.queue.shutdown();
    let (queue, events) = Queue::start(QueueConfig::default());
    let state = build(queue, events, Config {
        roots: vec![e.root.path().into()],
        cookie_secure: false,
        hash_cost: HashCost { m_kib: 256, t: 1, p: 1 },
        state_dir: Some(e.state_dir.path().into()),
        ..Default::default()
    }).unwrap();
    state.users.add("admin", "admin-password", Role::Admin).unwrap();
    state.users.add("bob", "bobs-password", Role::User).unwrap();
    e.router = router(state.clone());
    e.state = state;
    let bob = login(&e, "bob", "bobs-password").await;
    let admin = login(&e, "admin", "admin-password").await;
    assert_eq!(history_entries(&e, &admin, 2).await["entries"], all["entries"]);
    assert_eq!(history_job(&e, &bob, "bob-after-restart").await, first_job, "queue IDs restart");
    let bob_history = history_entries(&e, &bob, 2).await;
    assert!(bob_history["entries"][0]["id"].as_u64().unwrap() > oldest_id);
    let mut request = Request::builder().method("DELETE").uri("/api/history").header(COOKIE, &bob).body(Body::empty()).unwrap();
    request.extensions_mut().insert(ConnectInfo(fc_web::https::PeerAddr(SocketAddr::from(([127, 0, 0, 1], 40_000)))));
    assert_eq!(e.router.clone().oneshot(request).await.unwrap().status(), StatusCode::FORBIDDEN);
    assert_eq!(send(&e, "DELETE", "/api/history", Some(&bob), None).await.status, StatusCode::NO_CONTENT);
    history_entries(&e, &bob, 0).await;
    let admin_history = history_entries(&e, &admin, 1).await;
    assert_eq!(admin_history["entries"][0]["owner"], "admin");
    assert_eq!(send(&e, "DELETE", "/api/history", Some(&admin), None).await.status, StatusCode::NO_CONTENT);
    history_entries(&e, &admin, 0).await;
    assert_eq!(std::fs::read(e.root.path().join("history-out/bob-first")).unwrap(), b"history bytes");
    assert!(!e.state.queue.jobs().is_empty(), "clearing history must not clear the live queue");
    e.state.queue.shutdown();
    e.agent.shutdown();
}

#[tokio::test]
async fn history_write_errors_are_reported_and_refresh_retries_without_duplicates() {
    let e = env().await;
    let admin = login(&e, "admin", "admin-password").await;
    let mut frames = e.state.telemetry.subscribe();
    let blocker = e.state_dir.path().join("transfer-history.tmp");
    std::fs::create_dir(&blocker).unwrap();
    history_job(&e, &admin, "retry-summary").await;
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let frame = frames.recv().await.unwrap();
            if frame.logs.iter().any(|log| log.kind == "history_error") { break; }
        }
    }).await.unwrap();
    let failed = send(&e, "GET", "/api/history", Some(&admin), None).await;
    assert_eq!(failed.status, StatusCode::INTERNAL_SERVER_ERROR);
    assert!(failed.json["error"].as_str().unwrap().contains("history"));
    std::fs::remove_dir(&blocker).unwrap();
    let history = history_entries(&e, &admin, 1).await;
    assert_eq!(history["entries"][0]["label"], "retry-summary");
    assert_eq!(history_entries(&e, &admin, 1).await["entries"], history["entries"]);
    e.state.queue.shutdown();
    e.agent.shutdown();
}

#[tokio::test]
async fn history_records_cancelled_pending_jobs_and_keeps_new_jobs_after_clear() {
    let e = env().await;
    let bob = login(&e, "bob", "bobs-password").await;
    e.state.queue.pause();
    let id = history_job(&e, &bob, "cancelled-before-start").await;
    assert_eq!(send(&e, "DELETE", "/api/history", Some(&bob), None).await.status, StatusCode::NO_CONTENT);
    assert_eq!(send(&e, "DELETE", &format!("/api/transfers/{id}"), Some(&bob), None).await.status, StatusCode::NO_CONTENT);
    let history = history_entries(&e, &bob, 1).await;
    assert_eq!(history["entries"][0]["outcome"], "cancelled");
    assert_eq!(history["entries"][0]["files_copied"], 0);
    assert_eq!(history["entries"][0]["label"], "cancelled-before-start");
    e.state.queue.shutdown();
    e.agent.shutdown();
}

#[tokio::test]
async fn history_without_a_state_directory_is_explicitly_memory_only() {
    let mut e = env().await;
    e.state.queue.shutdown();
    let (queue, events) = Queue::start(QueueConfig::default());
    let state = build(queue, events, Config {
        roots: vec![e.root.path().into()],
        cookie_secure: false,
        hash_cost: HashCost { m_kib: 256, t: 1, p: 1 },
        ..Default::default()
    }).unwrap();
    state.users.add("bob", "bobs-password", Role::User).unwrap();
    e.router = router(state.clone());
    e.state = state;
    let bob = login(&e, "bob", "bobs-password").await;
    history_job(&e, &bob, "memory-summary").await;
    let history = history_entries(&e, &bob, 1).await;
    assert_eq!(history["persistent"], false);
    e.state.queue.shutdown();
    e.agent.shutdown();
}

fn pair_body(e: &Env, expected: Option<&str>, password: &str) -> Value {
    let mut b = json!({ "name": "nas", "target": target(e), "password": password });
    if let Some(fp) = expected {
        b["expected_fingerprint"] = json!(fp);
    }
    b
}

// ---------------------------------------------------------------------------
// Pairing
// ---------------------------------------------------------------------------

#[tokio::test]
async fn vpn_catalog_is_admin_only_and_has_ten_services_plus_two_private_networks() {
    let e = env().await;
    assert_eq!(send(&e, "GET", "/api/settings/vpn", None, None).await.status, StatusCode::UNAUTHORIZED);
    let bob = login(&e, "bob", "bobs-password").await;
    assert_eq!(send(&e, "GET", "/api/settings/vpn", Some(&bob), None).await.status, StatusCode::FORBIDDEN);
    let admin = login(&e, "admin", "admin-password").await;
    let reply = send(&e, "GET", "/api/settings/vpn", Some(&admin), None).await;
    assert_eq!(reply.status, StatusCode::OK);
    let providers = reply.json.as_array().unwrap();
    assert_eq!(providers.iter().filter(|p| p["mesh"].is_null()).count(), 10);
    assert_eq!(providers.iter().filter(|p| !p["mesh"].is_null()).count(), 2);
    for p in providers {
        assert!(p["download"].as_str().unwrap().starts_with("https://"));
        assert!(p["guide"].as_str().unwrap().starts_with("https://"));
        assert!(p.get("apps").is_none(), "local executable paths must not be exposed");
        if !p["mesh"].is_null() {
            fc_remote::client::RemoteSpec::parse(p["mesh"]["example"].as_str().unwrap()).unwrap();
        }
    }
}

#[tokio::test]
async fn pairing_is_admin_only() {
    let e = env().await;
    let bob = login(&e, "bob", "bobs-password").await;
    let r = send(&e, "POST", "/api/clients", Some(&bob), Some(pair_body(&e, None, "agent-pass-1"))).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
    let r = send(&e, "GET", "/api/clients", Some(&bob), None).await;
    assert_eq!(r.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn first_call_returns_the_fingerprint_and_saves_nothing() {
    let e = env().await;
    let admin = login(&e, "admin", "admin-password").await;
    let r = send(&e, "POST", "/api/clients", Some(&admin), Some(pair_body(&e, None, "agent-pass-1"))).await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert_eq!(r.json["needs_confirmation"], true);
    assert_eq!(r.json["fingerprint"], e.agent_fp);

    let list = send(&e, "GET", "/api/clients", Some(&admin), None).await;
    assert_eq!(list.json, json!([]), "nothing is paired until the fingerprint is confirmed");
}

#[tokio::test]
async fn confirmed_fingerprint_pairs_and_the_client_shows_online() {
    let e = env().await;
    let admin = login(&e, "admin", "admin-password").await;
    let r = send(&e, "POST", "/api/clients", Some(&admin), Some(pair_body(&e, Some(&e.agent_fp), "agent-pass-1"))).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    assert_eq!(r.json["fingerprint"], e.agent_fp);

    let list = send(&e, "GET", "/api/clients", Some(&admin), None).await;
    assert_eq!(list.json[0]["name"], "nas");
    assert_eq!(list.json[0]["status"], "online");

    // The password is never written to disk.
    let saved = std::fs::read_to_string(e.state_dir.path().join("clients.json")).unwrap();
    assert!(!saved.contains("agent-pass-1"));
    assert!(saved.contains(&e.agent_fp));
}

#[tokio::test]
async fn a_different_fingerprint_than_confirmed_is_refused() {
    let e = env().await;
    let admin = login(&e, "admin", "admin-password").await;
    let r = send(&e, "POST", "/api/clients", Some(&admin), Some(pair_body(&e, Some("SHA256:not-this-one"), "agent-pass-1"))).await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert!(r.json["error"].as_str().unwrap().contains(&e.agent_fp));
    assert_eq!(send(&e, "GET", "/api/clients", Some(&admin), None).await.json, json!([]));
}

#[tokio::test]
async fn a_wrong_password_is_refused_and_nothing_is_saved() {
    let e = env().await;
    let admin = login(&e, "admin", "admin-password").await;
    let r = send(&e, "POST", "/api/clients", Some(&admin), Some(pair_body(&e, Some(&e.agent_fp), "not-the-password"))).await;
    assert_eq!(r.status, StatusCode::UNAUTHORIZED);
    assert_eq!(send(&e, "GET", "/api/clients", Some(&admin), None).await.json, json!([]));
}

#[tokio::test]
async fn an_unreachable_host_is_reported_as_bad_gateway() {
    let e = env().await;
    let admin = login(&e, "admin", "admin-password").await;
    let body = json!({ "name": "ghost", "target": "alice@127.0.0.1:1:/backup", "password": "x" });
    let r = send(&e, "POST", "/api/clients", Some(&admin), Some(body)).await;
    assert_eq!(r.status, StatusCode::BAD_GATEWAY, "{}", r.json);
}

#[tokio::test]
async fn an_invalid_target_is_rejected_before_any_network_call() {
    let e = env().await;
    let admin = login(&e, "admin", "admin-password").await;
    let body = json!({ "name": "bad", "target": "C:/Users/me", "password": "x" });
    let r = send(&e, "POST", "/api/clients", Some(&admin), Some(body)).await;
    assert_eq!(r.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn unpairing_removes_the_profile() {
    let e = env().await;
    let admin = login(&e, "admin", "admin-password").await;
    send(&e, "POST", "/api/clients", Some(&admin), Some(pair_body(&e, Some(&e.agent_fp), "agent-pass-1"))).await;
    let id = send(&e, "GET", "/api/clients", Some(&admin), None).await.json[0]["id"].as_u64().unwrap();
    assert_eq!(send(&e, "DELETE", &format!("/api/clients/{id}"), Some(&admin), None).await.status, StatusCode::NO_CONTENT);
    assert_eq!(send(&e, "DELETE", &format!("/api/clients/{id}"), Some(&admin), None).await.status, StatusCode::NOT_FOUND);
    assert_eq!(send(&e, "GET", "/api/clients", Some(&admin), None).await.json, json!([]));
    e.agent.shutdown();
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

#[tokio::test]
async fn settings_change_the_running_queue() {
    let e = env().await;
    let admin = login(&e, "admin", "admin-password").await;

    let before = send(&e, "GET", "/api/settings", Some(&admin), None).await;
    assert_eq!(before.json, json!({ "speed_limit_bps": null, "error_policy": "retry" }));

    let r = send(&e, "PUT", "/api/settings", Some(&admin), Some(json!({ "speed_limit_bps": 10_000_000, "error_policy": "skip" }))).await;
    assert_eq!(r.status, StatusCode::OK);
    assert_eq!(e.state.queue.speed_limit(), Some(10_000_000));
    assert!(matches!(e.state.queue.error_policy(), fc_engine::ErrorPolicy::Skip));

    // Zero means unlimited.
    send(&e, "PUT", "/api/settings", Some(&admin), Some(json!({ "speed_limit_bps": 0, "error_policy": "retry" }))).await;
    assert_eq!(e.state.queue.speed_limit(), None);
}

#[tokio::test]
async fn settings_reject_out_of_range_values() {
    let e = env().await;
    let admin = login(&e, "admin", "admin-password").await;
    let too_fast = json!({ "speed_limit_bps": (1u64 << 30) + 1, "error_policy": "retry" });
    assert_eq!(send(&e, "PUT", "/api/settings", Some(&admin), Some(too_fast)).await.status, StatusCode::BAD_REQUEST);
    let bad_policy = json!({ "speed_limit_bps": null, "error_policy": "cancel" });
    assert_eq!(send(&e, "PUT", "/api/settings", Some(&admin), Some(bad_policy)).await.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn settings_are_admin_only() {
    let e = env().await;
    let bob = login(&e, "bob", "bobs-password").await;
    assert_eq!(send(&e, "GET", "/api/settings", Some(&bob), None).await.status, StatusCode::FORBIDDEN);
    let body = json!({ "speed_limit_bps": 1, "error_policy": "skip" });
    assert_eq!(send(&e, "PUT", "/api/settings", Some(&bob), Some(body)).await.status, StatusCode::FORBIDDEN);
}

// ---------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn admins_can_create_list_and_delete_accounts() {
    let e = env().await;
    let admin = login(&e, "admin", "admin-password").await;

    let created = send(&e, "POST", "/api/users", Some(&admin), Some(json!({ "name": "carol", "password": "carols-pass", "role": "user" }))).await;
    assert_eq!(created.status, StatusCode::CREATED);
    assert_eq!(send(&e, "POST", "/api/users", Some(&admin), Some(json!({ "name": "carol", "password": "carols-pass", "role": "user" }))).await.status, StatusCode::CONFLICT);
    assert_eq!(send(&e, "POST", "/api/users", Some(&admin), Some(json!({ "name": "dan", "password": "short", "role": "user" }))).await.status, StatusCode::BAD_REQUEST);

    let list = send(&e, "GET", "/api/users", Some(&admin), None).await;
    let names: Vec<_> = list.json.as_array().unwrap().iter().map(|u| u["name"].as_str().unwrap().to_string()).collect();
    assert_eq!(names, vec!["admin", "bob", "carol"]);

    let carol = login(&e, "carol", "carols-pass").await;
    assert_eq!(send(&e, "GET", "/api/me", Some(&carol), None).await.status, StatusCode::OK);

    assert_eq!(send(&e, "DELETE", "/api/users/carol", Some(&admin), None).await.status, StatusCode::NO_CONTENT);
    // Her session ends with the account.
    assert_eq!(send(&e, "GET", "/api/me", Some(&carol), None).await.status, StatusCode::UNAUTHORIZED);
    assert_eq!(send(&e, "POST", "/api/auth/login", None, Some(json!({ "user": "carol", "password": "carols-pass" }))).await.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn an_admin_cannot_delete_the_account_they_are_using() {
    let e = env().await;
    let admin = login(&e, "admin", "admin-password").await;
    let r = send(&e, "DELETE", "/api/users/admin", Some(&admin), None).await;
    assert_eq!(r.status, StatusCode::CONFLICT);
    assert_eq!(send(&e, "GET", "/api/me", Some(&admin), None).await.status, StatusCode::OK);
}

#[tokio::test]
async fn regular_users_cannot_manage_accounts() {
    let e = env().await;
    let bob = login(&e, "bob", "bobs-password").await;
    assert_eq!(send(&e, "GET", "/api/users", Some(&bob), None).await.status, StatusCode::FORBIDDEN);
    assert_eq!(send(&e, "POST", "/api/users", Some(&bob), Some(json!({ "name": "x", "password": "longenough", "role": "admin" }))).await.status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn pause_and_resume_the_whole_queue_is_admin_only() {
    let e = env().await;
    let admin = login(&e, "admin", "admin-password").await;
    let bob = login(&e, "bob", "bobs-password").await;
    assert_eq!(send(&e, "POST", "/api/queue/pause", Some(&bob), None).await.status, StatusCode::FORBIDDEN);
    assert_eq!(send(&e, "POST", "/api/queue/pause", Some(&admin), None).await.status, StatusCode::NO_CONTENT);
    assert!(e.state.queue.is_paused());
    assert_eq!(send(&e, "POST", "/api/queue/resume", Some(&admin), None).await.status, StatusCode::NO_CONTENT);
    assert!(!e.state.queue.is_paused());
}

#[tokio::test]
async fn any_user_can_read_whether_the_queue_is_paused() {
    let e = env().await;
    let admin = login(&e, "admin", "admin-password").await;
    let bob = login(&e, "bob", "bobs-password").await;
    assert_eq!(send(&e, "GET", "/api/queue", Some(&bob), None).await.json["paused"], false);
    send(&e, "POST", "/api/queue/pause", Some(&admin), None).await;
    assert_eq!(send(&e, "GET", "/api/queue", Some(&bob), None).await.json["paused"], true);
    send(&e, "POST", "/api/queue/resume", Some(&admin), None).await;
}

/// Pair the test agent, then return the new client's id.
async fn paired_client(e: &Env, admin: &str) -> u64 {
    let r = send(e, "POST", "/api/clients", Some(admin), Some(pair_body(e, Some(&e.agent_fp), "agent-pass-1"))).await;
    assert_eq!(r.status, StatusCode::CREATED, "{}", r.json);
    send(e, "GET", "/api/clients", Some(admin), None).await.json[0]["id"].as_u64().unwrap()
}

#[tokio::test]
async fn remote_browser_and_download_use_pairing_permissions_and_local_roots() {
    let e = env().await;
    let admin = login(&e, "admin", "admin-password").await;
    let bob = login(&e, "bob", "bobs-password").await;
    let id = paired_client(&e, &admin).await;
    std::fs::create_dir_all(e._share.path().join("tree/empty")).unwrap();
    std::fs::write(e._share.path().join("tree/file"), b"downloaded").unwrap();
    let uri = format!("/api/clients/{id}/browse");
    let browse = json!({ "password": "agent-pass-1", "path": "" });
    assert_eq!(send(&e, "POST", &uri, Some(&bob), Some(browse.clone())).await.status, StatusCode::FORBIDDEN);
    assert_eq!(send(&e, "POST", &uri, None, Some(browse.clone())).await.status, StatusCode::UNAUTHORIZED);
    let shares = send(&e, "POST", &uri, Some(&admin), Some(browse)).await;
    assert_eq!(shares.status, StatusCode::OK, "{}", shares.json);
    assert_eq!(shares.json["entries"][0]["name"], "backup");
    let listing = send(&e, "POST", &uri, Some(&admin), Some(json!({ "password": "agent-pass-1", "path": "backup/tree" }))).await;
    assert_eq!(listing.status, StatusCode::OK);
    assert_eq!(listing.json["entries"][0]["kind"], "Dir");
    assert_eq!(listing.json["entries"][1]["name"], "file");
    let invalid = send(&e, "POST", &uri, Some(&admin), Some(json!({ "password": "agent-pass-1", "path": "backup/../secret" }))).await;
    assert_eq!(invalid.status, StatusCode::BAD_REQUEST);
    let wrong = send(&e, "POST", &uri, Some(&admin), Some(json!({ "password": "wrong", "path": "" }))).await;
    assert_eq!(wrong.status, StatusCode::BAD_GATEWAY);

    let destination = e.root.path().join("restore");
    let mut body = json!({
        "sources": ["backup/tree"], "dest": destination,
        "remote": { "client_id": id, "password": "agent-pass-1", "direction": "download" }
    });
    assert_eq!(send(&e, "POST", "/api/transfers", Some(&bob), Some(body.clone())).await.status, StatusCode::FORBIDDEN);
    let outside = tempfile::tempdir().unwrap();
    body["dest"] = json!(outside.path());
    assert_eq!(send(&e, "POST", "/api/transfers", Some(&admin), Some(body.clone())).await.status, StatusCode::BAD_REQUEST);
    body["dest"] = json!(destination);
    e.state.queue.pause();
    let created = send(&e, "POST", "/api/transfers", Some(&admin), Some(body)).await;
    assert_eq!(created.status, StatusCode::ACCEPTED, "{}", created.json);
    assert!(!destination.exists(), "submission must not touch the destination");
    e.state.queue.resume();
    let job = wait_for_state(&e, &admin, "completed").await;
    assert!(job["destination"].as_str().unwrap().contains("restore"));
    assert_eq!(std::fs::read(destination.join("tree/file")).unwrap(), b"downloaded");
    assert!(destination.join("tree/empty").is_dir());
    let visible = send(&e, "GET", "/api/transfers", Some(&bob), None).await;
    assert_eq!(visible.json, json!([]));
    e.agent.shutdown();
}

fn remote_job(e: &Env, client_id: u64, dest: &str, password: &str) -> Value {
    let file = e.root.path().join("note.txt");
    std::fs::write(&file, b"remote note").unwrap();
    json!({
        "sources": [file],
        "dest": dest,
        "label": "web remote",
        "remote": { "client_id": client_id, "password": password },
    })
}

async fn wait_for_state(e: &Env, cookie: &str, state: &str) -> Value {
    for _ in 0..200 {
        let list = send(e, "GET", "/api/transfers", Some(cookie), None).await.json;
        if let Some(job) = list.as_array().and_then(|a| a.first()) {
            if job["state"] == state {
                return job.clone();
            }
            assert_ne!(job["state"], "failed", "job failed: {job}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("job never reached {state}");
}

async fn wait_for_conflict(e: &Env, cookie: &str, previous: Option<u64>) -> Value {
    for _ in 0..200 {
        let jobs = send(e, "GET", "/api/transfers", Some(cookie), None).await.json;
        if let Some(conflict) = jobs.as_array().unwrap().iter().map(|j| &j["conflict"])
            .find(|c| c.is_object() && c["id"].as_u64() != previous) {
            return conflict.clone();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("conflict not published");
}

#[tokio::test]
async fn conflicts_default_to_ask_and_require_owner_csrf_and_current_nonce() {
    let e = env().await;
    e.state.users.add("carol", "carols-password", Role::User).unwrap();
    let bob = login(&e, "bob", "bobs-password").await;
    let carol = login(&e, "carol", "carols-password").await;
    let admin = login(&e, "admin", "admin-password").await;
    let src = e.root.path().join("note.txt");
    let out = e.root.path().join("out");
    std::fs::create_dir(&out).unwrap();
    std::fs::write(&src, b"new").unwrap();
    std::fs::write(out.join("note.txt"), b"old").unwrap();
    let body = json!({ "sources": [src], "dest": out });
    let mut invalid = body.clone();
    invalid["conflict"] = json!("invalid");
    assert_eq!(send(&e, "POST", "/api/transfers", Some(&bob), Some(invalid)).await.status, StatusCode::BAD_REQUEST);
    let created = send(&e, "POST", "/api/transfers", Some(&bob), Some(body)).await;
    assert_eq!(created.status, StatusCode::ACCEPTED);
    let id = created.json["id"].as_u64().unwrap();
    let conflict = wait_for_conflict(&e, &bob, None).await;
    assert_eq!(conflict["different"], false);
    let uri = format!("/api/transfers/{id}/conflict");
    let reply = json!({ "conflict_id": conflict["id"], "action": "compare", "apply_to_all": true });
    assert_eq!(send(&e, "POST", &uri, Some(&carol), Some(reply.clone())).await.status, StatusCode::NOT_FOUND);
    let mut no_csrf = Request::builder().method("POST").uri(&uri)
        .header(COOKIE, &bob).header(CONTENT_TYPE, "application/json")
        .body(Body::from(reply.to_string())).unwrap();
    no_csrf.extensions_mut().insert(ConnectInfo(fc_web::https::PeerAddr(SocketAddr::from(([127, 0, 0, 1], 40_000)))));
    assert_eq!(e.router.clone().oneshot(no_csrf).await.unwrap().status(), StatusCode::FORBIDDEN);
    let mut invalid = reply.clone();
    invalid["action"] = json!("invalid");
    assert_eq!(send(&e, "POST", &uri, Some(&bob), Some(invalid)).await.status, StatusCode::BAD_REQUEST);
    let mut stale = reply.clone();
    stale["conflict_id"] = json!(0);
    assert_eq!(send(&e, "POST", &uri, Some(&bob), Some(stale)).await.status, StatusCode::CONFLICT);
    assert_eq!(send(&e, "POST", &uri, Some(&bob), Some(reply.clone())).await.status, StatusCode::NO_CONTENT);
    let second = wait_for_conflict(&e, &bob, conflict["id"].as_u64()).await;
    assert_eq!(second["different"], true);
    assert_eq!(send(&e, "POST", &uri, Some(&bob), Some(reply)).await.status, StatusCode::CONFLICT);
    let mut reply = json!({ "conflict_id": second["id"], "action": "compare", "apply_to_all": false });
    assert_eq!(send(&e, "POST", &uri, Some(&bob), Some(reply.clone())).await.status, StatusCode::CONFLICT);
    reply["action"] = json!("keep_both");
    assert_eq!(send(&e, "POST", &uri, Some(&admin), Some(reply)).await.status, StatusCode::NO_CONTENT);
    let job = wait_for_state(&e, &bob, "completed").await;
    assert!(job["conflict"].is_null());
    assert_eq!(std::fs::read(out.join("note.txt")).unwrap(), b"old");
    assert_eq!(std::fs::read(out.join("note (copy 1).txt")).unwrap(), b"new");
    e.state.queue.shutdown();
    e.agent.shutdown();
}

#[tokio::test]
async fn add_task_to_a_paired_pc_lands_the_files_there() {
    let e = env().await;
    let admin = login(&e, "admin", "admin-password").await;
    let id = paired_client(&e, &admin).await;

    let r = send(&e, "POST", "/api/transfers", Some(&admin), Some(remote_job(&e, id, "backup/from-web", "agent-pass-1"))).await;
    assert_eq!(r.status, StatusCode::ACCEPTED, "{}", r.json);

    let job = wait_for_state(&e, &admin, "completed").await;
    assert_eq!(job["remote"], true);
    assert!(job["destination"].as_str().unwrap().contains("backup/from-web"), "{job}");
    assert_eq!(std::fs::read(e._share.path().join("from-web/note.txt")).unwrap(), b"remote note");
    e.agent.shutdown();
}

#[tokio::test]
async fn the_password_is_never_returned_by_the_api() {
    let e = env().await;
    let admin = login(&e, "admin", "admin-password").await;
    let id = paired_client(&e, &admin).await;
    send(&e, "POST", "/api/transfers", Some(&admin), Some(remote_job(&e, id, "backup/quiet", "agent-pass-1"))).await;
    wait_for_state(&e, &admin, "completed").await;
    let list = send(&e, "GET", "/api/transfers", Some(&admin), None).await;
    assert!(!list.json.to_string().contains("agent-pass-1"));
    let history = history_entries(&e, &admin, 1).await;
    assert_eq!(history["entries"][0]["remote"], true);
    assert_eq!(history["entries"][0]["outcome"], "completed");
    assert!(!history.to_string().contains("agent-pass-1"));
    let saved = std::fs::read_to_string(e.state_dir.path().join("transfer-history.toml")).unwrap();
    assert!(!saved.contains("agent-pass-1") && !saved.contains("password") && !saved.contains("fingerprint"));
    e.agent.shutdown();
}

#[tokio::test]
async fn remote_jobs_are_refused_for_users_and_bad_input() {
    let e = env().await;
    let admin = login(&e, "admin", "admin-password").await;
    let bob = login(&e, "bob", "bobs-password").await;
    let id = paired_client(&e, &admin).await;

    let as_bob = send(&e, "POST", "/api/transfers", Some(&bob), Some(remote_job(&e, id, "backup/x", "agent-pass-1"))).await;
    assert_eq!(as_bob.status, StatusCode::FORBIDDEN);

    let unknown = send(&e, "POST", "/api/transfers", Some(&admin), Some(remote_job(&e, 999, "backup/x", "agent-pass-1"))).await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);

    let no_password = send(&e, "POST", "/api/transfers", Some(&admin), Some(remote_job(&e, id, "backup/x", ""))).await;
    assert_eq!(no_password.status, StatusCode::BAD_REQUEST);

    let escape = send(&e, "POST", "/api/transfers", Some(&admin), Some(remote_job(&e, id, "backup/../../etc", "agent-pass-1"))).await;
    assert_eq!(escape.status, StatusCode::BAD_REQUEST);

    let no_share = send(&e, "POST", "/api/transfers", Some(&admin), Some(remote_job(&e, id, "", "agent-pass-1"))).await;
    assert_eq!(no_share.status, StatusCode::BAD_REQUEST);
    e.agent.shutdown();
}

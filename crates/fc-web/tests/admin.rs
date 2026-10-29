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

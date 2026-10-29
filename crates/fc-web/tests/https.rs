//! The console over TLS: certificate pinning, the cookie flags, WebSocket
//! upgrades, and that plain HTTP gets no answer. Real sockets, real TLS 1.3.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use fc_engine::{Queue, QueueConfig};
use fc_remote::tls::{Identity, PinnedVerifier, https_server_config};
use fc_web::auth::{HashCost, Role};
use fc_web::https::TlsListener;
use fc_web::{AppState, Config, build, serve};
use rustls::pki_types::ServerName;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

type Tls = tokio_rustls::client::TlsStream<TcpStream>;

/// A console on a random loopback port, with one admin account.
async fn console() -> (Arc<AppState>, SocketAddr, String) {
    let identity = Identity::generate("127.0.0.1").unwrap();
    let fingerprint = identity.fingerprint();
    let root = tempfile::tempdir().unwrap();
    let (queue, events) = Queue::start(QueueConfig::default());
    let state = build(
        queue,
        events,
        Config {
            roots: vec![root.path().to_path_buf()],
            cookie_secure: true,
            session_ttl: Duration::from_secs(3600),
            hash_cost: HashCost { m_kib: 256, t: 1, p: 1 },
            state_dir: None,
            certificate_fingerprint: Some(fingerprint.clone()),
            torrents: None,
        },
    )
    .unwrap();
    state.users.add("alice", "password1", Role::Admin).unwrap();
    let listener = TlsListener::bind("127.0.0.1:0".parse().unwrap(), https_server_config(&identity).unwrap())
        .await
        .unwrap();
    let addr = listener.local_addr();
    tokio::spawn(serve(state.clone(), listener));
    (state, addr, fingerprint)
}

/// Connect with TLS 1.3, accepting only the certificate with this fingerprint.
async fn connect(addr: SocketAddr, pin: &str) -> std::io::Result<Tls> {
    let verifier = PinnedVerifier::new(Some(pin.to_string()));
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    let tcp = TcpStream::connect(addr).await?;
    TlsConnector::from(Arc::new(config)).connect(ServerName::IpAddress(addr.ip().into()), tcp).await
}

/// Send a request and return the response head (status line and headers).
async fn head(conn: &mut Tls, raw: &str) -> String {
    conn.write_all(raw.as_bytes()).await.unwrap();
    let mut buf = Vec::new();
    let read = async {
        let mut chunk = [0u8; 1024];
        loop {
            let n = conn.read(&mut chunk).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(5), read).await.expect("response head");
    let end = buf.windows(4).position(|w| w == b"\r\n\r\n").map_or(buf.len(), |p| p + 4);
    String::from_utf8_lossy(&buf[..end]).to_string()
}

fn login_request(user: &str, password: &str) -> String {
    let body = format!(r#"{{"user":"{user}","password":"{password}"}}"#);
    format!(
        "POST /api/auth/login HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

#[tokio::test]
async fn the_console_is_served_over_tls_with_its_certificate_on_the_info_route() {
    let (_state, addr, fp) = console().await;
    let mut conn = connect(addr, &fp).await.expect("pinned handshake");
    let page = head(&mut conn, "GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").await;
    assert!(page.starts_with("HTTP/1.1 200"), "{page}");

    let mut conn = connect(addr, &fp).await.unwrap();
    let info = head(&mut conn, "GET /api/info HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n").await;
    assert!(info.starts_with("HTTP/1.1 200"), "{info}");
}

#[tokio::test]
async fn a_different_certificate_stops_the_handshake() {
    let (_state, addr, _fp) = console().await;
    assert!(connect(addr, "SHA256:not-this-certificate").await.is_err());
}

#[tokio::test]
async fn sign_in_sets_a_secure_http_only_cookie() {
    let (_state, addr, fp) = console().await;
    let mut conn = connect(addr, &fp).await.unwrap();
    let reply = head(&mut conn, &login_request("alice", "password1")).await;
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    let cookie = reply.lines().find(|l| l.to_ascii_lowercase().starts_with("set-cookie:")).expect("cookie");
    assert!(cookie.contains("fc_session="), "{cookie}");
    assert!(cookie.contains("Secure") && cookie.contains("HttpOnly") && cookie.contains("SameSite=Strict"), "{cookie}");
}

#[tokio::test]
async fn a_websocket_upgrade_works_over_tls_with_the_session() {
    let (_state, addr, fp) = console().await;
    let mut conn = connect(addr, &fp).await.unwrap();
    let reply = head(&mut conn, &login_request("alice", "password1")).await;
    let cookie = reply
        .lines()
        .find(|l| l.to_ascii_lowercase().starts_with("set-cookie:"))
        .and_then(|l| l.split(';').next())
        .and_then(|pair| pair.split_once(':'))
        .map(|(_, v)| v.trim().to_string())
        .expect("session cookie");

    let mut ws = connect(addr, &fp).await.unwrap();
    let upgrade = format!(
        "GET /ws/telemetry HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nCookie: {cookie}\r\n\r\n"
    );
    let reply = head(&mut ws, &upgrade).await;
    assert!(reply.starts_with("HTTP/1.1 101"), "{reply}");
}

#[tokio::test]
async fn plain_http_to_the_console_port_gets_no_answer() {
    let (_state, addr, _fp) = console().await;
    let mut tcp = TcpStream::connect(addr).await.unwrap();
    tcp.write_all(b"GET /api/me HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n").await.unwrap();
    let mut buf = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(5), tcp.read_to_end(&mut buf)).await;
    let text = String::from_utf8_lossy(&buf);
    assert!(!text.contains("HTTP/1.1 "), "the console must not answer plain HTTP: {text}");
}

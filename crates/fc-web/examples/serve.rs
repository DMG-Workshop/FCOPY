//! Run the console on 127.0.0.1 for manual testing.
//!
//!     cargo run -p fc-web --example serve -- <share-root> [port] [--http]
//!
//! Serves HTTPS with this machine's agent identity (set FCOPY_AGENT_DIR to
//! choose where it is kept). `--http` serves plain HTTP for local tests only.
//! Creates an `admin` account with a random password, printed once.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use fc_engine::{CopyOptions, Queue, QueueConfig};
use fc_remote::tls::{Identity, https_server_config};
use fc_torrent::{EngineConfig, TorrentEngine};
use fc_web::auth::{HashCost, Role};
use fc_web::https::TlsListener;
use fc_web::{Config, build, router, serve};

#[tokio::main]
async fn main() {
    let plain = std::env::args().any(|a| a == "--http");
    let mut args = std::env::args().skip(1).filter(|a| a != "--http");
    let root: PathBuf = args.next().expect("usage: serve <share-root> [port] [--http]").into();
    let port: u16 = args.next().map(|p| p.parse().expect("port")).unwrap_or(8443);

    let identity = Identity::load_or_create(&fc_remote::config::default_agent_dir(), "localhost").expect("identity");
    let fingerprint = identity.fingerprint();

    let (queue, events) = Queue::start(QueueConfig {
        telemetry_interval: Duration::from_millis(100),
        options: CopyOptions::default(),
        ..Default::default()
    });
    // BitTorrent downloads go to a folder of their own, below the share root.
    // Transfers and waiting magnets are kept in the agent's folder, so they survive a restart.
    let torrent_state = fc_remote::config::default_agent_dir().join("torrents");
    let torrents = TorrentEngine::start(EngineConfig {
        state_dir: Some(torrent_state),
        ..EngineConfig::new(root.join("BitTorrent"))
    })
    .await
    .expect("torrent engine");
    let state = build(
        queue,
        events,
        Config {
            roots: vec![root.clone()],
            cookie_secure: !plain,
            session_ttl: Duration::from_secs(12 * 3600),
            hash_cost: HashCost::default(),
            state_dir: None,
            certificate_fingerprint: Some(fingerprint.clone()),
            torrents: Some(torrents),
        },
    )
    .expect("start console");

    let password = fc_remote::crypto::generate_access_key();
    state.users.add("admin", &password, Role::Admin).expect("create admin");
    let scheme = if plain { "http" } else { "https" };
    println!("open {scheme}://127.0.0.1:{port}/  user: admin  password: {password}");
    println!("certificate: {fingerprint}");

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    if plain {
        let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
        axum::serve(listener, router(state).into_make_service_with_connect_info::<fc_web::https::PeerAddr>())
            .await
            .expect("serve");
    } else {
        let listener = TlsListener::bind(addr, https_server_config(&identity).expect("tls config")).await.expect("bind");
        serve(state, listener).await.expect("serve");
    }
}

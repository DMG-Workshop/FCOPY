//! Run the console on 127.0.0.1 for manual testing.
//!
//!     cargo run -p fc-web --example serve -- <share-root> [port] [--http] [--bind IP]
//!     cargo run -p fc-web --example serve -- <share-root> [port] --bind 0.0.0.0 --tls-cert fullchain.pem --tls-key privkey.pem [--torrent-dht] [--torrent-trackers]
//!
//! Serves HTTPS with this machine's agent identity (set FCOPY_AGENT_DIR to
//! choose where it is kept). `--http` serves plain HTTP for local tests only.
//! Creates an `admin` account with a random password, printed once.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use fc_engine::{CopyOptions, Queue, QueueConfig};
use fc_remote::tls::{Identity, https_server_config, https_server_config_from_pem};
use fc_torrent::{EngineConfig, TorrentEngine};
use fc_web::auth::{HashCost, Role};
use fc_web::https::TlsListener;
use fc_web::{Config, build, router, serve};

#[tokio::main]
async fn main() {
    let mut plain = false;
    let mut root = None;
    let mut port = None;
    let mut bind = None;
    let mut cert_path = None;
    let mut key_path = None;
    let mut torrent_dht = false;
    let mut torrent_trackers = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--http" => plain = true,
            "--torrent-dht" => torrent_dht = true,
            "--torrent-trackers" => torrent_trackers = true,
            "--bind" => {
                bind = Some(
                    args.next()
                        .expect("--bind needs an IP address")
                        .parse()
                        .expect("invalid bind IP"),
                )
            }
            "--tls-cert" => {
                cert_path = Some(PathBuf::from(args.next().expect("--tls-cert needs a path")))
            }
            "--tls-key" => {
                key_path = Some(PathBuf::from(args.next().expect("--tls-key needs a path")))
            }
            _ if root.is_none() => root = Some(PathBuf::from(arg)),
            _ if port.is_none() => port = Some(arg.parse::<u16>().expect("port must be a number")),
            _ => panic!("unexpected argument: {arg}"),
        }
    }
    let root = root.expect(
        "usage: serve <share-root> [port] [--http] [--bind IP] [--tls-cert PEM --tls-key PEM]",
    );
    let port = port.unwrap_or(8443);
    let bind = bind.unwrap_or(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
    if plain && !bind.is_loopback() {
        panic!("--http is restricted to loopback; use HTTPS for LAN access");
    }
    let (tls_config, fingerprint) = match (plain, cert_path, key_path) {
        (true, None, None) => (None, None),
        (true, _, _) => panic!("--tls-cert/--tls-key cannot be used with --http"),
        (false, Some(cert), Some(key)) => {
            let (config, fp) =
                https_server_config_from_pem(&cert, &key).expect("load TLS certificate and key");
            (Some(config), Some(fp))
        }
        (false, None, None) => {
            let identity =
                Identity::load_or_create(&fc_remote::config::default_agent_dir(), "localhost")
                    .expect("identity");
            let fingerprint = identity.fingerprint();
            (
                Some(https_server_config(&identity).expect("tls config")),
                Some(fingerprint),
            )
        }
        (false, _, _) => panic!("--tls-cert and --tls-key must be supplied together"),
    };

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
        dht: torrent_dht,
        trackers: torrent_trackers,
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
            certificate_fingerprint: fingerprint.clone(),
            torrents: Some(torrents),
        },
    )
    .expect("start console");

    let password = fc_remote::crypto::generate_access_key();
    state
        .users
        .add("admin", &password, Role::Admin)
        .expect("create admin");
    let scheme = if plain { "http" } else { "https" };
    let display_addr = if bind.is_unspecified() {
        SocketAddr::from(([127, 0, 0, 1], port))
    } else {
        SocketAddr::new(bind, port)
    };
    println!("listening on {}", SocketAddr::new(bind, port));
    println!("open {scheme}://{display_addr}/  user: admin  password: {password}");
    if bind.is_unspecified() {
        println!("from another device, use this computer's LAN IP or DNS name");
    }
    if let Some(fingerprint) = &fingerprint {
        println!("certificate fingerprint: {fingerprint}");
    }

    let addr = SocketAddr::new(bind, port);
    if plain {
        let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
        axum::serve(
            listener,
            router(state).into_make_service_with_connect_info::<fc_web::https::PeerAddr>(),
        )
        .await
        .expect("serve");
    } else {
        let listener = TlsListener::bind(addr, tls_config.expect("TLS config"))
            .await
            .expect("bind");
        serve(state, listener).await.expect("serve");
    }
}

//! Run the dashboard on 127.0.0.1 for manual testing.
//!
//!     cargo run -p fc-web --example serve -- <share-root> [port]
//!
//! Creates an `admin` account with a random password, printed once. Plain
//! HTTP, so cookies are not `Secure`; never expose this beyond localhost.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use fc_engine::{CopyOptions, Queue, QueueConfig};
use fc_web::auth::{HashCost, Role};
use fc_web::{Config, build, router};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let root: PathBuf = args.next().expect("usage: serve <share-root> [port]").into();
    let port: u16 = args.next().map(|p| p.parse().expect("port")).unwrap_or(8080);

    let (queue, events) = Queue::start(QueueConfig {
        telemetry_interval: Duration::from_millis(100),
        options: CopyOptions::default(),
        ..Default::default()
    });
    let state = build(
        queue,
        events,
        Config { roots: vec![root], cookie_secure: false, session_ttl: Duration::from_secs(12 * 3600), hash_cost: HashCost::default() },
    )
    .expect("start dashboard");

    let password = fc_remote::crypto::generate_access_key();
    state.users.add("admin", &password, Role::Admin).expect("create admin");
    println!("open http://127.0.0.1:{port}/  user: admin  password: {password}");

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await.expect("bind");
    axum::serve(listener, router(state).into_make_service_with_connect_info::<SocketAddr>())
        .await
        .expect("serve");
}

//! Persistent HTTPS console for containers and native services.

use std::fs::OpenOptions;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use fc_engine::{Queue, QueueConfig};
use fc_remote::tls::{Identity, https_server_config, https_server_config_from_pem};
use fc_torrent::{EngineConfig, TorrentEngine};
use fc_web::auth::{Role, UserStore};
use fc_web::https::{PeerAddr, TlsListener};
use fc_web::{Config, build, router};

#[derive(Parser)]
#[command(name = "fcopy-web", version, about = "Persistent FCOPY HTTPS console")]
struct Args {
    #[arg(long, env = "FCOPY_WEB_CONFIG", default_value = "/config")]
    config: PathBuf,
    #[arg(long, env = "FCOPY_WEB_ROOT", default_value = "/data")]
    root: PathBuf,
    #[arg(long, env = "FCOPY_WEB_LISTEN", default_value = "0.0.0.0:8443")]
    listen: SocketAddr,
    #[arg(long, requires = "tls_key", env = "FCOPY_TLS_CERT")]
    tls_cert: Option<PathBuf>,
    #[arg(long, requires = "tls_cert", env = "FCOPY_TLS_KEY")]
    tls_key: Option<PathBuf>,
    /// Read the initial admin password from a mounted secret, only on the first run.
    #[arg(long, env = "FCOPY_ADMIN_PASSWORD_FILE")]
    admin_password_file: Option<PathBuf>,
    #[arg(long)]
    no_torrents: bool,
    #[arg(long)]
    torrent_dht: bool,
    #[arg(long)]
    torrent_trackers: bool,
}

fn bootstrap(
    users: &UserStore,
    password_file: Option<&std::path::Path>,
) -> Result<Option<String>, Box<dyn std::error::Error>> {
    if !users.is_empty() {
        if !users.list().iter().any(|(_, role)| *role == Role::Admin) {
            return Err("The account store has no administrator; restore a valid backup".into());
        }
        return Ok(None);
    }
    let generated = password_file.is_none();
    let password = match password_file {
        Some(path) => std::fs::read_to_string(path)?
            .trim_end_matches(['\r', '\n'])
            .to_string(),
        None => fc_remote::crypto::generate_access_key(),
    };
    users.add("admin", &password, Role::Admin)?;
    Ok(generated.then_some(password))
}

async fn run(args: Args) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(&args.config)?;
    let config = args.config.canonicalize()?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(config.join("web.lock"))?;
    fs4::FileExt::try_lock(&lock).map_err(|error| {
        io::Error::other(format!(
            "Cannot claim console state (another server may be running): {error}"
        ))
    })?;
    let root = args.root.canonicalize()?;
    if !root.is_dir() || config.starts_with(&root) || root.starts_with(&config) {
        return Err(
            "The data root must be a directory separate from the configuration directory".into(),
        );
    }
    if let Some(path) = &args.tls_key {
        if path.canonicalize()?.starts_with(&root) {
            return Err(
                "Private keys and bootstrap password files must be outside the data share".into(),
            );
        }
    }
    let (tls, fingerprint) = match (&args.tls_cert, &args.tls_key) {
        (Some(cert), Some(key)) => https_server_config_from_pem(cert, key)?,
        (None, None) => {
            let identity = Identity::load_or_create(&config.join("identity"), "fcopy-web")?;
            (https_server_config(&identity)?, identity.fingerprint())
        }
        _ => return Err("Supply both --tls-cert and --tls-key".into()),
    };
    let listener = TlsListener::bind(args.listen, tls).await?;
    let (queue, events) = Queue::start(QueueConfig {
        telemetry_interval: Duration::from_millis(100),
        ..Default::default()
    });
    let torrents = if args.no_torrents {
        None
    } else {
        Some(
            TorrentEngine::start(EngineConfig {
                state_dir: Some(config.join("torrents")),
                dht: args.torrent_dht,
                trackers: args.torrent_trackers,
                ..EngineConfig::new(root.join("BitTorrent"))
            })
            .await?,
        )
    };
    let state = build(
        queue,
        events,
        Config {
            roots: vec![root.clone()],
            state_dir: Some(config),
            torrents,
            certificate_fingerprint: Some(fingerprint.clone()),
            ..Default::default()
        },
    )?;
    if state.users.is_empty() {
        if let Some(path) = &args.admin_password_file {
            if path.canonicalize()?.starts_with(&root) {
                return Err("Bootstrap password files must be outside the data share".into());
            }
        }
    }
    if let Some(password) = bootstrap(&state.users, args.admin_password_file.as_deref())? {
        println!("Initial administrator: admin");
        println!("Initial password (shown only on first initialization): {password}");
        println!("Create your own administrator in Settings, then remove the bootstrap account.");
    }
    println!("HTTPS listening on {}", listener.local_addr());
    println!("Certificate fingerprint: {fingerprint}");
    let shutdown_state = state.clone();
    let shutdown = async move {
        if let Err(error) = shutdown_signal().await {
            eprintln!("Shutdown signal listener failed: {error}");
        }
        let queue_state = shutdown_state.clone();
        if let Err(error) = tokio::task::spawn_blocking(move || queue_state.queue.shutdown()).await
        {
            eprintln!("Queue shutdown failed: {error}");
        }
        if let Some(torrents) = &shutdown_state.torrents {
            torrents.shutdown().await;
        }
    };
    axum::serve(
        listener,
        router(state).into_make_service_with_connect_info::<PeerAddr>(),
    )
    .with_graceful_shutdown(shutdown)
    .await?;
    Ok(())
}

async fn shutdown_signal() -> io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(not(unix))]
    tokio::signal::ctrl_c().await
}

#[tokio::main]
async fn main() {
    if let Err(error) = run(Args::parse()).await {
        eprintln!("fcopy-web: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fc_web::auth::HashCost;

    const CHEAP: HashCost = HashCost {
        m_kib: 256,
        t: 1,
        p: 1,
    };

    #[test]
    fn initial_credentials_survive_restart_and_are_never_reset() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("users.json");
        let users = UserStore::open(CHEAP, Some(path.clone())).unwrap();
        let password = bootstrap(&users, None).unwrap().unwrap();
        assert!(users.authenticate("admin", &password).is_some());
        drop(users);
        let users = UserStore::open(CHEAP, Some(path.clone())).unwrap();
        assert!(
            bootstrap(&users, Some(std::path::Path::new("not-read-on-restart")))
                .unwrap()
                .is_none()
        );
        assert!(users.authenticate("admin", &password).is_some());
        assert!(!std::fs::read_to_string(path).unwrap().contains(&password));
    }

    #[test]
    fn bootstrap_secret_is_used_once_and_weak_passwords_fail() {
        let directory = tempfile::tempdir().unwrap();
        let secret = directory.path().join("secret");
        std::fs::write(&secret, "a-long-initial-password\n").unwrap();
        let users = UserStore::new(CHEAP).unwrap();
        assert!(bootstrap(&users, Some(&secret)).unwrap().is_none());
        assert!(
            users
                .authenticate("admin", "a-long-initial-password")
                .is_some()
        );
        std::fs::write(&secret, "short").unwrap();
        assert!(bootstrap(&UserStore::new(CHEAP).unwrap(), Some(&secret)).is_err());
    }
}

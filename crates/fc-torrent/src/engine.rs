//! A small, fcopy-shaped wrapper over `librqbit`: add a magnet link or a
//! `.torrent` file, list transfers, pause, resume, remove, and create a
//! `.torrent` file from a folder.
//!
//! The engine is async and holds no thread of its own: callers run it on the
//! runtime they already have. DHT and tracker announces are off by default, so
//! a session on a LAN or in tests never reaches the public internet. Peers
//! come from the `.torrent` file's trackers when they are enabled, or from
//! peers given explicitly.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use librqbit::{
    AddTorrent, AddTorrentOptions, CreateTorrentOptions, ListenerMode, ListenerOptions, Session, SessionOptions,
    TorrentStatsState, create_torrent,
};
use librqbit::spawn_utils::BlockingSpawner;

#[derive(Debug, thiserror::Error)]
pub enum TorrentError {
    #[error("torrent engine: {0}")]
    Engine(String),
    #[error("no transfer with id {0}")]
    NotFound(usize),
    #[error("the magnet link is still waiting for metadata from peers; it will appear in the list when it arrives")]
    MetadataPending,
}

impl From<anyhow::Error> for TorrentError {
    fn from(e: anyhow::Error) -> Self {
        TorrentError::Engine(format!("{e:#}"))
    }
}

/// Settings for [`TorrentEngine::start`].
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Where downloads are written, and where seeds are read from.
    pub output_dir: PathBuf,
    /// TCP port for incoming peers. `None` listens for nothing: outgoing only.
    pub listen_port: Option<u16>,
    /// Use the DHT to find peers. Off by default.
    pub dht: bool,
    /// Announce to the trackers listed in a torrent. Off by default.
    pub trackers: bool,
}

impl EngineConfig {
    pub fn new(output_dir: impl Into<PathBuf>) -> Self {
        Self { output_dir: output_dir.into(), listen_port: None, dht: false, trackers: false }
    }
}

/// What a transfer is doing right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TorrentState {
    /// Checking existing data, or waiting for metadata from a magnet link.
    Initializing,
    /// Downloading or seeding.
    Live,
    Paused,
    Error,
}

/// One transfer, as the UI shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct TorrentStatus {
    pub id: usize,
    pub info_hash: String,
    pub name: Option<String>,
    pub state: TorrentState,
    /// True once every piece is present and verified.
    pub finished: bool,
    pub progress_bytes: u64,
    pub total_bytes: u64,
    pub download_mbps: f64,
    pub upload_mbps: f64,
    pub uploaded_bytes: u64,
    pub live_peers: u32,
    pub error: Option<String>,
}

impl TorrentStatus {
    /// 0.0 to 100.0.
    pub fn percent(&self) -> f64 {
        if self.total_bytes == 0 { 0.0 } else { self.progress_bytes as f64 * 100.0 / self.total_bytes as f64 }
    }
}

pub struct TorrentEngine {
    session: Arc<Session>,
}

impl TorrentEngine {
    /// Start a session. Must be called from inside a Tokio runtime.
    pub async fn start(config: EngineConfig) -> Result<Self, TorrentError> {
        let listen = config.listen_port.map(|port| ListenerOptions {
            mode: ListenerMode::TcpOnly,
            listen_addr: SocketAddr::from(([0, 0, 0, 0], port)),
            ..Default::default()
        });
        let options = SessionOptions {
            dht: if config.dht { Default::default() } else { None },
            disable_trackers: !config.trackers,
            persistence: None,
            listen,
            ..Default::default()
        };
        let session = Session::new_with_opts(config.output_dir, options).await?;
        Ok(Self { session })
    }

    /// The TCP address peers can connect to, if listening.
    pub fn listen_addr(&self) -> Option<SocketAddr> {
        self.session.listen_addr()
    }

    /// Add by magnet link. A magnet has no file list, so the transfer is only
    /// registered once peers have sent the metadata. Waits up to `wait` for that.
    /// If it hasn't arrived, returns [`TorrentError::MetadataPending`]; the
    /// attempt goes on in the background and the transfer appears in `list`
    /// when the metadata arrives.
    pub async fn add_magnet(&self, magnet: &str, paused: bool, wait: std::time::Duration) -> Result<usize, TorrentError> {
        // `add_torrent` does not return until the file list has arrived from peers,
        // which can take minutes. So run it in the background and return as soon
        // as the transfer appears in the session. The metadata wait goes on in the
        // background task.
        let before: std::collections::HashSet<usize> = self.ids().into_iter().collect();
        let options = AddTorrentOptions { paused, ..Default::default() };
        let session = self.session.clone();
        let url = magnet.to_string();
        let task = tokio::spawn(async move { session.add_torrent(AddTorrent::from_url(url), Some(options)).await.map(|_| ()) });
        let deadline = std::time::Instant::now() + wait;
        loop {
            if let Some(id) = self.ids().into_iter().find(|id| !before.contains(id)) {
                return Ok(id);
            }
            if std::time::Instant::now() >= deadline {
                return Err(TorrentError::MetadataPending);
            }
            if task.is_finished() {
                return match task.await {
                    Ok(Ok(())) => Err(TorrentError::Engine("the magnet link was not added".into())),
                    Ok(Err(e)) => Err(e.into()),
                    Err(e) => Err(TorrentError::Engine(e.to_string())),
                };
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }

    fn ids(&self) -> Vec<usize> {
        self.session.with_torrents(|torrents| torrents.map(|(id, _)| id).collect())
    }

    /// Add a `.torrent` file's bytes. `peers` are contacted directly, which is
    /// how transfers run on a LAN without trackers.
    pub async fn add_torrent_bytes(&self, bytes: Vec<u8>, paused: bool, peers: Vec<SocketAddr>) -> Result<usize, TorrentError> {
        let options = AddTorrentOptions {
            paused,
            overwrite: true,
            initial_peers: (!peers.is_empty()).then_some(peers),
            ..Default::default()
        };
        let response = self.session.add_torrent(AddTorrent::from_bytes(bytes), Some(options)).await?;
        let handle = response.into_handle().ok_or_else(|| TorrentError::Engine("nothing to add".into()))?;
        Ok(handle.id())
    }

    /// Every transfer the session knows about.
    pub fn list(&self) -> Vec<TorrentStatus> {
        let mut out: Vec<TorrentStatus> = self.session.with_torrents(|torrents| {
            torrents.map(|(id, handle)| status_of(id, handle)).collect()
        });
        out.sort_by_key(|s| s.id);
        out
    }

    pub fn status(&self, id: usize) -> Result<TorrentStatus, TorrentError> {
        self.list().into_iter().find(|s| s.id == id).ok_or(TorrentError::NotFound(id))
    }

    pub async fn pause(&self, id: usize) -> Result<(), TorrentError> {
        let handle = self.handle(id)?;
        self.session.pause(&handle).await?;
        Ok(())
    }

    pub async fn resume(&self, id: usize) -> Result<(), TorrentError> {
        let handle = self.handle(id)?;
        self.session.unpause(&handle).await?;
        Ok(())
    }

    /// Stop and forget a transfer. `delete_files` also removes its data.
    pub async fn remove(&self, id: usize, delete_files: bool) -> Result<(), TorrentError> {
        self.session.delete(librqbit::api::TorrentIdOrHash::Id(id), delete_files).await?;
        Ok(())
    }

    /// Build a `.torrent` file for a file or folder, with the given name and piece size.
    pub async fn create_file_torrent(path: &Path, name: Option<&str>) -> Result<Vec<u8>, TorrentError> {
        let spawner = BlockingSpawner::new(4);
        let options = CreateTorrentOptions { name, ..Default::default() };
        let result = create_torrent(path, options, &spawner).await?;
        Ok(result.as_bytes()?.to_vec())
    }

    pub async fn shutdown(&self) {
        self.session.stop().await;
    }

    fn handle(&self, id: usize) -> Result<Arc<librqbit::ManagedTorrent>, TorrentError> {
        self.session.get(librqbit::api::TorrentIdOrHash::Id(id)).ok_or(TorrentError::NotFound(id))
    }
}

fn status_of(id: usize, handle: &librqbit::ManagedTorrent) -> TorrentStatus {
    let stats = handle.stats();
    let (state, error) = match &stats.state {
        TorrentStatsState::Initializing { .. } => (TorrentState::Initializing, None),
        TorrentStatsState::Live => (TorrentState::Live, None),
        TorrentStatsState::Paused => (TorrentState::Paused, None),
        TorrentStatsState::Error => (TorrentState::Error, stats.error.clone()),
    };
    let live = stats.live.as_ref();
    TorrentStatus {
        id,
        info_hash: handle.info_hash().as_string(),
        name: handle.name(),
        state,
        finished: stats.finished,
        progress_bytes: stats.progress_bytes,
        total_bytes: stats.total_bytes,
        download_mbps: live.map(|l| l.download_speed.mbps).unwrap_or(0.0),
        upload_mbps: live.map(|l| l.upload_speed.mbps).unwrap_or(0.0),
        uploaded_bytes: stats.uploaded_bytes,
        live_peers: live.map(|l| l.snapshot.peer_stats.live).unwrap_or(0),
        error,
    }
}

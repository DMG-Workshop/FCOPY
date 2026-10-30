//! A small, fcopy-shaped wrapper over `librqbit`: add a magnet link or a
//! `.torrent` file, list transfers, pause, resume, remove, and create a
//! `.torrent` file from a folder.
//!
//! The engine is async and holds no thread of its own: callers run it on the
//! runtime they already have. DHT and tracker announces are off by default, so
//! a session on a LAN or in tests never reaches the public internet. Peers
//! come from the `.torrent` file's trackers when they are enabled, or from
//! peers given explicitly.
//!
//! A magnet link has no file list until peers send one, so `librqbit` does not
//! register it until then. The engine keeps those magnets itself: they show up
//! in [`TorrentEngine::list`] at once, with [`TorrentPhase::WaitingForMetadata`],
//! and become ordinary transfers when the list arrives. With a `state_dir`, both
//! the transfers and the waiting magnets survive a restart.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use librqbit::spawn_utils::BlockingSpawner;
use librqbit::{
    AddTorrent, AddTorrentOptions, CreateTorrentOptions, ListenerMode, ListenerOptions, Session, SessionOptions,
    SessionPersistenceConfig, TorrentStatsState, create_torrent,
};
use serde::{Deserialize, Serialize};

/// Ids at or above this belong to magnets still waiting for metadata. The
/// session's own ids are small counters, so the two ranges never meet.
pub const PENDING_ID_BASE: usize = 1 << 48;

/// Waiting magnets are saved here, inside the state directory.
const PENDING_FILE: &str = "pending-magnets.json";

#[derive(Debug, thiserror::Error)]
pub enum TorrentError {
    #[error("torrent engine: {0}")]
    Engine(String),
    #[error("no transfer with id {0}")]
    NotFound(usize),
}

impl From<anyhow::Error> for TorrentError {
    fn from(e: anyhow::Error) -> Self {
        TorrentError::Engine(format!("{e:#}"))
    }
}

impl From<std::io::Error> for TorrentError {
    fn from(e: std::io::Error) -> Self {
        TorrentError::Engine(e.to_string())
    }
}

/// Settings for [`TorrentEngine::start`].
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Where downloads are written, and where seeds are read from.
    pub output_dir: PathBuf,
    /// Where transfers and waiting magnets are kept between runs. `None` keeps
    /// nothing: the engine starts empty and forgets everything on exit.
    pub state_dir: Option<PathBuf>,
    /// TCP port for incoming peers. `None` listens for nothing: outgoing only.
    pub listen_port: Option<u16>,
    /// Use the DHT to find peers. Off by default.
    pub dht: bool,
    /// Announce to the trackers listed in a torrent. Off by default.
    pub trackers: bool,
}

impl EngineConfig {
    pub fn new(output_dir: impl Into<PathBuf>) -> Self {
        Self { output_dir: output_dir.into(), state_dir: None, listen_port: None, dht: false, trackers: false }
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

/// The one word a UI shows for a transfer's situation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TorrentPhase {
    /// A magnet link, waiting for peers to send its file list.
    WaitingForMetadata,
    /// Checking the data already on disk against the torrent.
    Checking,
    Downloading,
    /// Every piece is here; the transfer keeps uploading until it is removed.
    Seeding,
    Paused,
    Failed,
}

/// One transfer, as the UI shows it.
#[derive(Debug, Clone, PartialEq)]
pub struct TorrentStatus {
    pub id: usize,
    pub info_hash: String,
    pub name: Option<String>,
    pub state: TorrentState,
    /// True while a magnet link is still waiting for its file list.
    pub metadata_pending: bool,
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

    pub fn phase(&self) -> TorrentPhase {
        match self.state {
            TorrentState::Error => TorrentPhase::Failed,
            TorrentState::Paused => TorrentPhase::Paused,
            TorrentState::Initializing if self.metadata_pending => TorrentPhase::WaitingForMetadata,
            TorrentState::Initializing => TorrentPhase::Checking,
            TorrentState::Live if self.finished => TorrentPhase::Seeding,
            TorrentState::Live => TorrentPhase::Downloading,
        }
    }
}

/// Per-transfer choices for an add.
#[derive(Debug, Clone, Default)]
pub struct AddOptions {
    /// Start paused.
    pub paused: bool,
    /// Contacted directly, for the file list or the data.
    pub peers: Vec<SocketAddr>,
    /// Where this transfer's data goes. `None` uses the engine's output folder.
    pub output: Option<PathBuf>,
}

/// A magnet link still waiting for its file list.
struct Pending {
    id: usize,
    magnet: String,
    paused: bool,
    peers: Vec<SocketAddr>,
    output: Option<PathBuf>,
    /// The task that waits for metadata. `None` once it has finished.
    task: Option<tokio::task::JoinHandle<()>>,
    error: Option<String>,
    /// The session's id, once the file list has arrived. The transfer keeps
    /// this entry's id, so a row in the UI does not change identity.
    resolved: Option<usize>,
}

/// What is written to disk for a waiting magnet.
#[derive(Serialize, Deserialize)]
struct SavedMagnet {
    magnet: String,
    paused: bool,
    peers: Vec<SocketAddr>,
    /// Missing in files saved before folders were chosen per transfer.
    #[serde(default)]
    output: Option<PathBuf>,
}

type PendingList = Arc<Mutex<Vec<Pending>>>;

pub struct TorrentEngine {
    session: Arc<Session>,
    pending: PendingList,
    next_pending: AtomicUsize,
    pending_file: Option<PathBuf>,
}

impl TorrentEngine {
    /// Start a session. Must be called from inside a Tokio runtime. With a
    /// `state_dir`, transfers and waiting magnets saved by an earlier run come back.
    pub async fn start(config: EngineConfig) -> Result<Self, TorrentError> {
        let listen = config.listen_port.map(|port| ListenerOptions {
            mode: ListenerMode::TcpOnly,
            listen_addr: SocketAddr::from(([0, 0, 0, 0], port)),
            ..Default::default()
        });
        let persistence = config.state_dir.clone().map(|folder| SessionPersistenceConfig::Json { folder: Some(folder) });
        let options = SessionOptions {
            dht: if config.dht { Default::default() } else { None },
            disable_trackers: !config.trackers,
            persistence,
            listen,
            ..Default::default()
        };
        let session = Session::new_with_opts(config.output_dir, options).await?;
        let engine = Self {
            session,
            pending: Arc::new(Mutex::new(Vec::new())),
            next_pending: AtomicUsize::new(PENDING_ID_BASE),
            pending_file: config.state_dir.map(|d| d.join(PENDING_FILE)),
        };
        for saved in load_saved(engine.pending_file.as_deref())? {
            let opts = AddOptions { paused: saved.paused, peers: saved.peers, output: saved.output };
            engine.spawn_magnet(&saved.magnet, opts)?;
        }
        Ok(engine)
    }

    /// The TCP address peers can connect to, if listening.
    pub fn listen_addr(&self) -> Option<SocketAddr> {
        self.session.listen_addr()
    }

    /// Add a magnet link and return its id at once. Until its file list has
    /// arrived from peers the transfer is listed as waiting for metadata; `peers`
    /// are contacted for that list. Waiting magnets are saved with the state
    /// directory, so they are still waiting after a restart.
    pub async fn add_magnet(&self, magnet: &str, paused: bool, peers: Vec<SocketAddr>) -> Result<usize, TorrentError> {
        self.add_magnet_with(magnet, AddOptions { paused, peers, output: None }).await
    }

    /// [`add_magnet`](Self::add_magnet) with the output folder chosen too.
    pub async fn add_magnet_with(&self, magnet: &str, opts: AddOptions) -> Result<usize, TorrentError> {
        self.spawn_magnet(magnet, opts)
    }

    /// Start waiting for one magnet's metadata in the background, and list it.
    fn spawn_magnet(&self, magnet: &str, opts: AddOptions) -> Result<usize, TorrentError> {
        if !magnet.starts_with("magnet:?") || magnet_param(magnet, "xt").and_then(|x| x.strip_prefix("urn:btih:").map(str::to_string)).is_none() {
            return Err(TorrentError::Engine("not a magnet link with an info hash".into()));
        }
        let id = PENDING_ID_BASE + self.next_pending.fetch_add(1, Ordering::Relaxed);
        let mut list = lock(&self.pending);
        let session = self.session.clone();
        let shared = self.pending.clone();
        let file = self.pending_file.clone();
        let url = magnet.to_string();
        let options = AddTorrentOptions {
            paused: opts.paused,
            initial_peers: (!opts.peers.is_empty()).then(|| opts.peers.clone()),
            output_folder: folder_string(opts.output.as_deref()),
            ..Default::default()
        };
        // The task locks the list before it reports anything, so the entry below is
        // always in place by the time it does.
        let task = tokio::spawn(async move {
            let result = session.add_torrent(AddTorrent::from_url(url), Some(options)).await;
            let mut list = lock(&shared);
            if let Some(p) = list.iter_mut().find(|p| p.id == id) {
                p.task = None;
                match result {
                    Ok(response) => match response.into_handle() {
                        Some(handle) => p.resolved = Some(handle.id()),
                        None => p.error = Some("the magnet link is already in the session".into()),
                    },
                    Err(e) => p.error = Some(format!("{e:#}")),
                }
            }
            // Every change rewrites the whole file. A failed write is retried by the next change.
            let _ = save_pending(file.as_deref(), &list);
        });
        list.push(Pending {
            id,
            magnet: magnet.to_string(),
            paused: opts.paused,
            peers: opts.peers,
            output: opts.output,
            task: Some(task),
            error: None,
            resolved: None,
        });
        save_pending(self.pending_file.as_deref(), &list)?;
        Ok(id)
    }

    /// Every transfer the session knows about, waiting magnets included. A magnet
    /// that has resolved is listed under the id it had while waiting.
    pub fn list(&self) -> Vec<TorrentStatus> {
        let pending = lock(&self.pending);
        let mut out: Vec<TorrentStatus> = self.session.with_torrents(|torrents| {
            torrents
                .map(|(real, handle)| {
                    let id = pending.iter().find(|p| p.resolved == Some(real)).map_or(real, |p| p.id);
                    status_of(id, handle)
                })
                .collect()
        });
        out.extend(pending.iter().filter(|p| p.resolved.is_none()).map(pending_status));
        out.sort_by_key(|s| s.id);
        out
    }

    pub fn status(&self, id: usize) -> Result<TorrentStatus, TorrentError> {
        self.list().into_iter().find(|s| s.id == id).ok_or(TorrentError::NotFound(id))
    }

    /// Add a `.torrent` file's bytes. `peers` are contacted directly, which is
    /// how transfers run on a LAN without trackers.
    pub async fn add_torrent_bytes(&self, bytes: Vec<u8>, paused: bool, peers: Vec<SocketAddr>) -> Result<usize, TorrentError> {
        self.add_torrent_bytes_with(bytes, AddOptions { paused, peers, output: None }).await
    }

    /// [`add_torrent_bytes`](Self::add_torrent_bytes) with the output folder chosen too.
    pub async fn add_torrent_bytes_with(&self, bytes: Vec<u8>, opts: AddOptions) -> Result<usize, TorrentError> {
        let options = AddTorrentOptions {
            paused: opts.paused,
            overwrite: true,
            initial_peers: (!opts.peers.is_empty()).then_some(opts.peers),
            output_folder: folder_string(opts.output.as_deref()),
            ..Default::default()
        };
        let response = self.session.add_torrent(AddTorrent::from_bytes(bytes), Some(options)).await?;
        let handle = response.into_handle().ok_or_else(|| TorrentError::Engine("nothing to add".into()))?;
        Ok(handle.id())
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

    /// Stop and forget a transfer. `delete_files` also removes its data. A
    /// magnet still waiting has no data yet, so it is only forgotten.
    pub async fn remove(&self, id: usize, delete_files: bool) -> Result<(), TorrentError> {
        // The list is locked only inside `forget`, never across the await below.
        if let Some(real) = self.forget(id)? {
            self.session.delete(librqbit::api::TorrentIdOrHash::Id(real), delete_files).await?;
        }
        Ok(())
    }

    /// Drop the bookkeeping for `id`. Returns the session id to delete, if any:
    /// none for a magnet that never resolved.
    fn forget(&self, id: usize) -> Result<Option<usize>, TorrentError> {
        let mut list = lock(&self.pending);
        if id < PENDING_ID_BASE {
            list.retain(|p| p.resolved != Some(id));
            return Ok(Some(id));
        }
        let index = list.iter().position(|p| p.id == id).ok_or(TorrentError::NotFound(id))?;
        let removed = list.remove(index);
        if let Some(task) = removed.task {
            task.abort();
        }
        save_pending(self.pending_file.as_deref(), &list)?;
        Ok(removed.resolved)
    }

    /// Build a `.torrent` file for a file or folder, with the given name and piece size.
    pub async fn create_file_torrent(path: &Path, name: Option<&str>) -> Result<Vec<u8>, TorrentError> {
        let spawner = BlockingSpawner::new(4);
        let options = CreateTorrentOptions { name, ..Default::default() };
        let result = create_torrent(path, options, &spawner).await?;
        Ok(result.as_bytes()?.to_vec())
    }

    /// Stop the session. Waiting magnets stop waiting, but stay saved.
    pub async fn shutdown(&self) {
        for p in lock(&self.pending).iter_mut() {
            if let Some(task) = p.task.take() {
                task.abort();
            }
        }
        self.session.stop().await;
    }

    /// The session's id behind a listed id. A magnet that is still waiting has none yet.
    fn real_id(&self, id: usize) -> Result<usize, TorrentError> {
        if id < PENDING_ID_BASE {
            return Ok(id);
        }
        let list = lock(&self.pending);
        let p = list.iter().find(|p| p.id == id).ok_or(TorrentError::NotFound(id))?;
        p.resolved.ok_or_else(|| TorrentError::Engine("a magnet link cannot be paused until its file list arrives".into()))
    }

    fn handle(&self, id: usize) -> Result<Arc<librqbit::ManagedTorrent>, TorrentError> {
        let real = self.real_id(id)?;
        self.session.get(librqbit::api::TorrentIdOrHash::Id(real)).ok_or(TorrentError::NotFound(id))
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn load_saved(path: Option<&Path>) -> Result<Vec<SavedMagnet>, TorrentError> {
    let Some(path) = path else { return Ok(vec![]) };
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| TorrentError::Engine(format!("{}: {e}", path.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(vec![]),
        Err(e) => Err(e.into()),
    }
}

/// Write the magnets still waiting, with a temporary file and a rename so a
/// crash never leaves half a file behind. Magnets that failed or have resolved
/// are not kept: a resolved one is a session transfer, saved by librqbit.
fn save_pending(path: Option<&Path>, list: &[Pending]) -> Result<(), TorrentError> {
    let Some(path) = path else { return Ok(()) };
    let saved: Vec<SavedMagnet> = list
        .iter()
        .filter(|p| p.error.is_none() && p.resolved.is_none())
        .map(|p| SavedMagnet { magnet: p.magnet.clone(), paused: p.paused, peers: p.peers.clone(), output: p.output.clone() })
        .collect();
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(&saved).map_err(|e| TorrentError::Engine(e.to_string()))?;
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// The output folder as librqbit takes it.
fn folder_string(path: Option<&Path>) -> Option<String> {
    path.map(|p| p.to_string_lossy().into_owned())
}

/// One query parameter of a magnet link, as written (not decoded).
fn magnet_param(magnet: &str, key: &str) -> Option<String> {
    let query = magnet.strip_prefix("magnet:?")?;
    query.split('&').find_map(|pair| pair.strip_prefix(key)?.strip_prefix('=').map(str::to_string))
}

/// Decode `%XX` escapes and `+` in a magnet's display name.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                match std::str::from_utf8(&bytes[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(byte) => {
                        out.push(byte);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn pending_status(p: &Pending) -> TorrentStatus {
    let magnet_hash = magnet_param(&p.magnet, "xt")
        .and_then(|x| x.strip_prefix("urn:btih:").map(str::to_string))
        .unwrap_or_default();
    let name = magnet_param(&p.magnet, "dn").map(|n| percent_decode(&n)).filter(|n| !n.is_empty());
    TorrentStatus {
        id: p.id,
        info_hash: magnet_hash,
        name,
        state: if p.error.is_some() { TorrentState::Error } else { TorrentState::Initializing },
        metadata_pending: p.task.is_some(),
        finished: false,
        progress_bytes: 0,
        total_bytes: 0,
        download_mbps: 0.0,
        upload_mbps: 0.0,
        uploaded_bytes: 0,
        live_peers: 0,
        error: p.error.clone(),
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
        metadata_pending: false,
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

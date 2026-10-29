//! The sending side: connect to an agent, pin its identity, authenticate, and
//! push files over per-file QUIC streams.
//!
//! ```text
//! Session::open
//!   resolve ─▶ QUIC connect (PinnedVerifier) ─▶ unknown host? ask *before* auth
//!       │
//!       ▼  control stream
//!   Hello → AuthResume{token}            (reconnect; falls back to the password)
//!         → AuthStart → AuthChallenge → Argon2id (blocking pool) → AuthProof → AuthOk
//!       │                                 (server signature checked: mutual auth)
//!       ├─ control reader task  (server Pause/Resume → remote_paused)
//!       └─ control writer task  (local Pause/Resume, Bye)
//!
//! Session::push_file
//!   FileOffer ─▶ FileAccept{resume_from} ─▶ hash local prefix
//!   ├─ send: gate (cancel/skip/pause/limit) → DATA … → FileEnd{digest}
//!   └─ recv: Ack … → FileDone{digest}        (run concurrently)
//! ```
//!
//! Errors say whether a retry can help: [`ClientError::is_retryable`] is true
//! only for connection-level failures, which the backend answers by
//! reconnecting (token first) and re-offering the file, which resumes.

use std::fmt;
use std::io::{self, Read};
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use sha2::{Digest as _, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use fc_engine::{Digest, Hasher, JobControl, LinkStats, RateLimiter};

use crate::crypto::{auth_message, random, ClientKeys, EXPORTER_LABEL};
use crate::protocol::*;
use crate::server::to_engine;
use crate::tls::{client_config, PinnedVerifier};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);
/// How often a paused transfer, or a stalled stream, re-checks its controls.
const POLL: Duration = Duration::from_millis(50);
const ABORT_POLL: Duration = Duration::from_millis(100);
/// SNI / certificate name. The pin, not the name, authenticates the server.
const SERVER_NAME: &str = "fcopy-agent";

/// KDF limits the client will run for a server (a hostile one could ask for
/// absurd memory). 1 GiB, 10 passes, 8 lanes.
const MAX_KDF_M_KIB: u32 = 1024 * 1024;
const MAX_KDF_T: u32 = 10;
const MAX_KDF_P: u32 = 8;

// ---------------------------------------------------------------------------
// Remote target syntax
// ---------------------------------------------------------------------------

/// A parsed `[user@]host[:port]:/share/path` target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteSpec {
    pub user: Option<String>,
    /// Hostname or IP, without brackets.
    pub host: String,
    pub port: u16,
    /// `share/dir…`, no leading or trailing `/`. Never empty.
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid remote target '{input}': {reason}")]
pub struct SpecError {
    pub input: String,
    pub reason: &'static str,
}

impl RemoteSpec {
    /// Parse `[user@]host[:port]:/share/path`; IPv6 hosts go in brackets
    /// (`[fe80::1]:/share`). A one-letter host is refused because it is a
    /// Windows drive (`C:/Users`), not a machine.
    pub fn parse(input: &str) -> Result<Self, SpecError> {
        let err = |reason| SpecError { input: input.to_string(), reason };

        // The user part ends at an '@' that comes before any ':' '/' or '['.
        let (user, rest) = match input.find('@') {
            Some(at) if !input[..at].contains([':', '/', '[']) => {
                if at == 0 {
                    return Err(err("empty user name"));
                }
                (Some(input[..at].to_string()), &input[at + 1..])
            }
            _ => (None, input),
        };

        let (host, after_host) = if let Some(bracketed) = rest.strip_prefix('[') {
            let close = bracketed.find(']').ok_or_else(|| err("missing ']' after IPv6 address"))?;
            let host = &bracketed[..close];
            if host.parse::<std::net::Ipv6Addr>().is_err() {
                return Err(err("bracketed host is not an IPv6 address"));
            }
            let after = bracketed[close + 1..].strip_prefix(':').ok_or_else(|| err("expected ':' after ']'"))?;
            (host, after)
        } else {
            let colon = rest.find(':').ok_or_else(|| err("expected host:/share/path"))?;
            let host = &rest[..colon];
            if host.len() == 1 && host.chars().all(|c| c.is_ascii_alphabetic()) {
                return Err(err("looks like a Windows drive letter, not a host"));
            }
            (host, &rest[colon + 1..])
        };
        if host.is_empty() {
            return Err(err("empty host"));
        }
        if host.contains(['/', '\\', '@']) {
            return Err(err("illegal character in host"));
        }

        let (port, path) = if after_host.starts_with('/') {
            (DEFAULT_PORT, after_host)
        } else {
            let (p, path) = after_host.split_once(':').ok_or_else(|| err("expected ':/share/path' after the port"))?;
            let port = p.parse::<u16>().ok().filter(|&p| p != 0).ok_or_else(|| err("invalid port"))?;
            (port, path)
        };
        if !path.starts_with('/') {
            return Err(err("remote path must start with '/'"));
        }

        let mut parts = Vec::new();
        for c in path.split('/').filter(|c| !c.is_empty()) {
            if c == "." || c == ".." {
                return Err(err("'.' and '..' are not allowed in remote paths"));
            }
            if c.contains(['\\', '\0']) {
                return Err(err("illegal character in remote path"));
            }
            parts.push(c);
        }
        if parts.is_empty() {
            return Err(err("missing share name"));
        }
        Ok(Self { user, host: host.to_string(), port, path: parts.join("/") })
    }

    /// `host:port` (IPv6 bracketed): the known-hosts key.
    pub fn host_port(&self) -> String {
        if self.host.contains(':') { format!("[{}]:{}", self.host, self.port) } else { format!("{}:{}", self.host, self.port) }
    }

    /// The share this target writes into.
    pub fn share(&self) -> &str {
        self.path.split('/').next().unwrap_or(&self.path)
    }

    /// Remote path for a file at `rel` below this target (`/`-joined).
    /// Only normal components are kept; plans never contain anything else.
    pub fn join(&self, rel: &Path) -> String {
        let mut out = self.path.clone();
        for c in rel.components() {
            if let Component::Normal(s) = c {
                out.push('/');
                out.push_str(&s.to_string_lossy());
            }
        }
        out
    }
}

impl fmt::Display for RemoteSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(u) = &self.user {
            write!(f, "{u}@")?;
        }
        write!(f, "{}:/{}", self.host_port(), self.path)
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("cannot connect: {0}")]
    Connect(String),
    #[error("connection lost: {0}")]
    Connection(String),
    #[error("host identity {fingerprint} is not trusted")]
    HostKeyNotTrusted { fingerprint: String },
    #[error(
        "HOST IDENTITY CHANGED: expected {expected}, server presented {presented}. \
         This could be an attack, or the agent was reinstalled."
    )]
    HostKeyChanged { expected: String, presented: String },
    #[error("authentication failed: {reason}")]
    AuthFailed { reason: String, retry_after: Duration },
    #[error("no password available and the session token was refused")]
    NoCredentials,
    #[error("the server could not prove it knows the account's verifier (possible impostor)")]
    ServerNotAuthenticated,
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("rejected by server ({reason:?}): {message}")]
    Rejected { reason: RejectReason, message: String },
    #[error("server could not finish the file: {0}")]
    Remote(String),
    #[error("{path}: {source}")]
    Local { path: PathBuf, source: io::Error },
    #[error("checksum mismatch: sent {expected}, server stored {actual}")]
    Mismatch { expected: String, actual: String },
    #[error("cancelled")]
    Cancelled,
    #[error("skipped")]
    Skipped,
}

impl ClientError {
    /// Worth reconnecting and trying again (the transfer resumes).
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::Connect(_) | Self::Connection(_))
    }

    fn local(path: &Path, source: io::Error) -> Self {
        Self::Local { path: path.to_path_buf(), source }
    }
}

// ---------------------------------------------------------------------------
// Connector: everything needed to (re)open a session
// ---------------------------------------------------------------------------

/// Asked with `(host:port, fingerprint)` the first time a host is seen.
/// Return `true` to trust it. Runs on the blocking pool, so it may prompt.
pub type TrustFn = Arc<dyn Fn(&str, &str) -> bool + Send + Sync>;

struct Password(String);

impl fmt::Debug for Password {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Password(<redacted>)")
    }
}

/// How to reach and log in to one agent. Reused across reconnects: once a
/// fingerprint is trusted it stays pinned here, so a reconnect never re-asks.
pub struct Connector {
    spec: RemoteSpec,
    user: String,
    password: Option<Password>,
    client_name: String,
    pinned: Mutex<Option<String>>,
    trust: Option<TrustFn>,
}

impl fmt::Debug for Connector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Connector")
            .field("spec", &self.spec)
            .field("user", &self.user)
            .field("pinned", &self.pinned_fingerprint())
            .finish_non_exhaustive()
    }
}

impl Connector {
    /// The user defaults to the local login name.
    pub fn new(spec: RemoteSpec) -> Self {
        let user = spec.user.clone().unwrap_or_else(local_user);
        Self { spec, user, password: None, client_name: local_hostname(), pinned: Mutex::new(None), trust: None }
    }

    /// Password or unattended access key.
    pub fn password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(Password(password.into()));
        self
    }

    /// Fingerprint from known_hosts (or `--accept-fingerprint`).
    pub fn pinned(self, fingerprint: Option<String>) -> Self {
        *self.pinned.lock().unwrap_or_else(|e| e.into_inner()) = fingerprint;
        self
    }

    /// Ask the user about hosts with no pin. Without this, unknown hosts fail
    /// with [`ClientError::HostKeyNotTrusted`].
    pub fn on_unknown_host(mut self, trust: TrustFn) -> Self {
        self.trust = Some(trust);
        self
    }

    pub fn client_name(mut self, name: impl Into<String>) -> Self {
        self.client_name = name.into();
        self
    }

    pub fn spec(&self) -> &RemoteSpec {
        &self.spec
    }

    pub fn user(&self) -> &str {
        &self.user
    }

    pub fn pinned_fingerprint(&self) -> Option<String> {
        self.pinned.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn pin(&self, fingerprint: String) {
        *self.pinned.lock().unwrap_or_else(|e| e.into_inner()) = Some(fingerprint);
    }
}

fn local_user() -> String {
    ["USER", "USERNAME", "LOGNAME"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "fcopy".into())
}

fn local_hostname() -> String {
    ["COMPUTERNAME", "HOSTNAME"]
        .iter()
        .find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| "fcopy".into())
}

// ---------------------------------------------------------------------------
// Session
// ---------------------------------------------------------------------------

enum Auth<'a> {
    Token([u8; 32]),
    Password(&'a Password),
}

/// An authenticated connection to one agent.
pub struct Session {
    endpoint: quinn::Endpoint,
    conn: quinn::Connection,
    peer: String,
    server_name: String,
    fingerprint: String,
    newly_trusted: bool,
    token: Mutex<[u8; 32]>,
    token_ttl: Duration,
    shares: Vec<ShareInfo>,
    remote_paused: Arc<AtomicBool>,
    /// Last pause state told to the server, so each change is sent once.
    local_paused_sent: AtomicBool,
    ctl_tx: mpsc::Sender<Message>,
    tasks: Vec<JoinHandle<()>>,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session").field("peer", &self.peer).field("fingerprint", &self.fingerprint).finish_non_exhaustive()
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        for t in &self.tasks {
            t.abort();
        }
    }
}

impl Session {
    /// Connect and log in: with `token` first if given (fast reconnect), then
    /// with the connector's password if the token is refused.
    pub async fn open(connector: &Connector, token: Option<[u8; 32]>) -> Result<Self, ClientError> {
        if let Some(t) = token {
            match Self::open_with(connector, Auth::Token(t)).await {
                Ok(s) => return Ok(s),
                // Expired or already used: the password still works.
                Err(ClientError::AuthFailed { .. }) if connector.password.is_some() => {}
                Err(e) => return Err(e),
            }
        }
        let password = connector.password.as_ref().ok_or(ClientError::NoCredentials)?;
        Self::open_with(connector, Auth::Password(password)).await
    }

    async fn open_with(c: &Connector, auth: Auth<'_>) -> Result<Self, ClientError> {
        let addr = resolve(&c.spec).await?;
        let bind = if addr.is_ipv6() { SocketAddr::from(([0u16; 8], 0)) } else { SocketAddr::from(([0, 0, 0, 0], 0)) };
        let endpoint = quinn::Endpoint::client(bind).map_err(|e| ClientError::Connect(e.to_string()))?;

        let pinned = c.pinned_fingerprint();
        let verifier = PinnedVerifier::new(pinned.clone());
        let config = client_config(verifier.clone()).map_err(|e| ClientError::Connect(e.to_string()))?;
        let connecting = endpoint.connect_with(config, addr, SERVER_NAME).map_err(|e| ClientError::Connect(e.to_string()))?;
        let conn = match connecting.await {
            Ok(conn) => conn,
            Err(e) => {
                return Err(match (pinned, verifier.seen()) {
                    (Some(expected), Some(presented)) if expected != presented => ClientError::HostKeyChanged { expected, presented },
                    _ => ClientError::Connect(e.to_string()),
                });
            }
        };
        let fingerprint = verifier.seen().ok_or_else(|| ClientError::Protocol("server presented no certificate".into()))?;

        // Trust on first use, decided before any credential leaves this machine.
        let mut newly_trusted = false;
        if c.pinned_fingerprint().is_none() {
            let accepted = match &c.trust {
                Some(trust) => {
                    let (trust, host, fp) = (trust.clone(), c.spec.host_port(), fingerprint.clone());
                    tokio::task::spawn_blocking(move || trust(&host, &fp)).await.unwrap_or(false)
                }
                None => false,
            };
            if !accepted {
                conn.close(0u32.into(), b"host identity not trusted");
                return Err(ClientError::HostKeyNotTrusted { fingerprint });
            }
            c.pin(fingerprint.clone());
            newly_trusted = true;
        }

        let (mut send, mut recv) = conn.open_bi().await.map_err(|e| ClientError::Connect(e.to_string()))?;
        let handshake = handshake(c, &conn, auth, &mut send, &mut recv);
        let (server_name, token, token_ttl, shares) = match tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake).await {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                conn.close(0u32.into(), b"handshake failed");
                return Err(e);
            }
            Err(_) => {
                conn.close(0u32.into(), b"handshake timeout");
                return Err(ClientError::Connection("handshake timed out".into()));
            }
        };

        // Control stream tasks.
        let remote_paused = Arc::new(AtomicBool::new(false));
        let (ctl_tx, mut ctl_rx) = mpsc::channel::<Message>(16);
        let writer = tokio::spawn(async move {
            while let Some(m) = ctl_rx.recv().await {
                let bye = m == Message::Bye;
                if write_msg(&mut send, &m).await.is_err() || bye {
                    let _ = send.finish();
                    break;
                }
            }
        });
        let reader = {
            let remote_paused = remote_paused.clone();
            tokio::spawn(async move {
                let mut buf = Vec::new();
                while let Ok(m) = read_msg(&mut recv, &mut buf).await {
                    match m {
                        Message::Pause => remote_paused.store(true, Ordering::Relaxed),
                        Message::Resume => remote_paused.store(false, Ordering::Relaxed),
                        _ => {}
                    }
                }
            })
        };

        let peer = format!("{}@{}", c.user, c.spec.host_port());
        Ok(Self {
            endpoint,
            conn,
            peer,
            server_name,
            fingerprint,
            newly_trusted,
            token: Mutex::new(token),
            token_ttl,
            shares,
            remote_paused,
            local_paused_sent: AtomicBool::new(false),
            ctl_tx,
            tasks: vec![writer, reader],
        })
    }

    /// The server's certificate fingerprint (`SHA256:…`).
    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    /// The user accepted this host during this open: save it to known_hosts.
    pub fn newly_trusted(&self) -> bool {
        self.newly_trusted
    }

    /// The agent's configured name (from `HelloAck`).
    pub fn server_name(&self) -> &str {
        &self.server_name
    }

    /// Single-use token for the next reconnect.
    pub fn token(&self) -> [u8; 32] {
        *self.token.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn token_ttl(&self) -> Duration {
        self.token_ttl
    }

    pub fn shares(&self) -> &[ShareInfo] {
        &self.shares
    }

    /// The agent asked senders to pause.
    pub fn remote_paused(&self) -> bool {
        self.remote_paused.load(Ordering::Relaxed)
    }

    pub fn is_connected(&self) -> bool {
        self.conn.close_reason().is_none()
    }

    /// Link telemetry from QUIC. Sent/acked bytes and reconnects belong to
    /// the job, so the backend fills those in.
    pub fn link_stats(&self) -> LinkStats {
        let s = self.conn.stats();
        LinkStats {
            peer: self.peer.clone(),
            connected: self.is_connected(),
            rtt: s.path.rtt,
            lost_packets: s.path.lost_packets,
            cwnd: s.path.cwnd,
            remote_paused: self.remote_paused(),
            ..LinkStats::default()
        }
    }

    /// Say goodbye and close the connection cleanly.
    pub async fn close(self) {
        let _ = self.ctl_tx.send(Message::Bye).await;
        self.conn.close(0u32.into(), b"bye");
        let _ = tokio::time::timeout(Duration::from_secs(1), self.endpoint.wait_idle()).await;
    }

    /// Tell the server about a local pause change (once per change).
    fn note_local_pause(&self, paused: bool) {
        if self.local_paused_sent.swap(paused, Ordering::Relaxed) != paused {
            let _ = self.ctl_tx.try_send(if paused { Message::Pause } else { Message::Resume });
        }
    }

    /// Classify a stream error: a dead connection is retryable, anything else
    /// is the peer misbehaving.
    fn wire_error(&self, e: impl fmt::Display) -> ClientError {
        match self.conn.close_reason() {
            Some(reason) => ClientError::Connection(reason.to_string()),
            None => ClientError::Protocol(e.to_string()),
        }
    }

    async fn open_stream(&self) -> Result<(quinn::SendStream, quinn::RecvStream), ClientError> {
        self.conn.open_bi().await.map_err(|e| ClientError::Connection(e.to_string()))
    }

    /// Create `remote` (`share/dir…`) and its parents.
    pub async fn make_dir(&self, remote: &str) -> Result<(), ClientError> {
        let (mut send, mut recv) = self.open_stream().await?;
        let offer = Message::FileOffer {
            kind: EntryKind::Dir,
            path: remote.to_string(),
            size: 0,
            mtime: (0, 0),
            mode: 0,
            algo: HashAlgo::Xxh3,
            source_id: [0; 16],
            overwrite: false,
        };
        write_msg(&mut send, &offer).await.map_err(|e| self.wire_error(e))?;
        let _ = send.finish();
        let mut buf = Vec::new();
        match read_msg(&mut recv, &mut buf).await.map_err(|e| self.wire_error(e))? {
            Message::FileDone { ok: true, .. } => Ok(()),
            Message::FileDone { message, .. } => Err(ClientError::Remote(message)),
            Message::FileReject { reason, message } => Err(ClientError::Rejected { reason, message }),
            other => Err(ClientError::Protocol(format!("expected FileDone, got {other:?}"))),
        }
    }

    /// Upload one file, resuming a partial upload of the same source version.
    pub async fn push_file(
        &self,
        req: &PushRequest<'_>,
        ctl: &PushControl<'_>,
        observer: &dyn PushObserver,
    ) -> Result<PushOutcome, ClientError> {
        let local = req.local;
        let meta = tokio::fs::metadata(local).await.map_err(|e| ClientError::local(local, e))?;
        if !meta.is_file() {
            return Err(ClientError::local(local, io::Error::new(io::ErrorKind::InvalidInput, "not a regular file")));
        }
        let size = meta.len();
        let mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| (d.as_secs() as i64, d.subsec_nanos()))
            .unwrap_or((-1, 0));
        let source_id = req.source_id.unwrap_or_else(|| source_id(local, size, mtime));

        let (mut send, mut recv) = self.open_stream().await?;
        let offer = Message::FileOffer {
            kind: EntryKind::File,
            path: req.remote.to_string(),
            size,
            mtime,
            mode: unix_mode(&meta),
            algo: req.algo,
            source_id,
            overwrite: req.overwrite,
        };
        write_msg(&mut send, &offer).await.map_err(|e| self.wire_error(e))?;
        let mut buf = Vec::new();
        let resume_from = match read_msg(&mut recv, &mut buf).await.map_err(|e| self.wire_error(e))? {
            Message::FileAccept { resume_from } if resume_from <= size => resume_from,
            Message::FileAccept { resume_from } => {
                return Err(ClientError::Protocol(format!("server resumes at {resume_from}, past the end ({size})")));
            }
            Message::FileReject { reason, message } => return Err(ClientError::Rejected { reason, message }),
            other => return Err(ClientError::Protocol(format!("expected FileAccept, got {other:?}"))),
        };
        if resume_from > 0 {
            observer.acked(resume_from);
        }

        // The digest covers the whole file, so the part the server already
        // holds is hashed locally (not re-sent).
        let hasher = {
            let (path, algo) = (local.to_path_buf(), req.algo);
            tokio::task::spawn_blocking(move || hash_prefix(&path, algo, resume_from))
                .await
                .map_err(|e| ClientError::local(local, io::Error::other(e)))?
                .map_err(|e| ClientError::local(local, e))?
        };

        let sender = async {
            let mut hasher = hasher;
            let mut file = tokio::fs::File::open(local).await.map_err(|e| ClientError::local(local, e))?;
            file.seek(io::SeekFrom::Start(resume_from)).await.map_err(|e| ClientError::local(local, e))?;
            let mut chunk = vec![0u8; CHUNK];
            let mut offset = resume_from;
            while offset < size {
                self.gate(ctl).await?;
                let want = (size - offset).min(CHUNK as u64) as usize;
                let n = read_full(&mut file, &mut chunk[..want]).await.map_err(|e| ClientError::local(local, e))?;
                if n < want {
                    let e = io::Error::new(io::ErrorKind::UnexpectedEof, "file shrank while it was being sent");
                    return Err(ClientError::local(local, e));
                }
                if let Some(limiter) = ctl.limiter {
                    let wait = limiter.reserve(n as u64);
                    if !wait.is_zero() {
                        tokio::time::sleep(wait).await;
                    }
                }
                hasher.update(&chunk[..n]);
                write_data(&mut send, offset, &chunk[..n]).await.map_err(|e| self.wire_error(e))?;
                offset += n as u64;
                observer.sent(offset);
            }
            let digest = hasher.finalize();
            write_msg(&mut send, &Message::FileEnd { size, digest: digest.as_bytes().to_vec() })
                .await
                .map_err(|e| self.wire_error(e))?;
            Ok(digest)
        };
        let receiver = async {
            loop {
                match read_msg(&mut recv, &mut buf).await.map_err(|e| self.wire_error(e))? {
                    Message::Ack { offset } => observer.acked(offset),
                    Message::FileDone { ok, digest, message } => return Ok((ok, digest, message)),
                    other => return Err(ClientError::Protocol(format!("expected Ack or FileDone, got {other:?}"))),
                }
            }
        };
        // Cancel and skip must also work while a send is stalled by flow
        // control (e.g. the server paused), where the gate is never reached.
        let abort = async {
            loop {
                tokio::time::sleep(ABORT_POLL).await;
                if ctl.control.is_cancelled() {
                    return ClientError::Cancelled;
                }
                if ctl.control.take_skip() {
                    return ClientError::Skipped;
                }
            }
        };

        let result = tokio::select! {
            r = async { tokio::try_join!(sender, receiver) } => r,
            e = abort => Err(e),
        };
        let (digest, (ok, remote_digest, message)) = match result {
            Ok(v) => v,
            Err(e) => {
                // Abandon the stream, not the session. The server keeps the
                // partial file, so a later offer of the same version resumes.
                let _ = send.reset(0u32.into());
                let _ = recv.stop(0u32.into());
                return Err(e);
            }
        };
        let _ = send.finish();

        let hex = |d: &[u8]| Digest::new(digest.algorithm(), d.to_vec()).to_hex();
        if !remote_digest.is_empty() && remote_digest != digest.as_bytes() {
            return Err(ClientError::Mismatch { expected: digest.to_hex(), actual: hex(&remote_digest) });
        }
        if !ok {
            return Err(ClientError::Remote(message));
        }
        observer.acked(size);
        Ok(PushOutcome { size, resumed_from: resume_from, digest })
    }

    /// Before each chunk: cancel, skip, local or remote pause.
    async fn gate(&self, ctl: &PushControl<'_>) -> Result<(), ClientError> {
        loop {
            if ctl.control.is_cancelled() {
                return Err(ClientError::Cancelled);
            }
            if ctl.control.take_skip() {
                return Err(ClientError::Skipped);
            }
            let local = ctl.control.is_paused();
            self.note_local_pause(local);
            if !local && !self.remote_paused() {
                return Ok(());
            }
            if let Some(reason) = self.conn.close_reason() {
                return Err(ClientError::Connection(reason.to_string()));
            }
            tokio::time::sleep(POLL).await;
        }
    }
}

async fn resolve(spec: &RemoteSpec) -> Result<SocketAddr, ClientError> {
    let mut addrs = tokio::net::lookup_host((spec.host.as_str(), spec.port))
        .await
        .map_err(|e| ClientError::Connect(format!("cannot resolve {}: {e}", spec.host)))?;
    addrs.next().ok_or_else(|| ClientError::Connect(format!("{} has no addresses", spec.host)))
}

/// `Hello`, then token or SCRAM. Returns (server name, token, token TTL, shares).
async fn handshake(
    c: &Connector,
    conn: &quinn::Connection,
    auth: Auth<'_>,
    send: &mut quinn::SendStream,
    recv: &mut quinn::RecvStream,
) -> Result<(String, [u8; 32], Duration, Vec<ShareInfo>), ClientError> {
    let proto = |e: ProtoError| match conn.close_reason() {
        Some(r) => ClientError::Connection(r.to_string()),
        None => ClientError::Protocol(e.to_string()),
    };
    let failed = |reason: String, retry_after_ms: u64| ClientError::AuthFailed { reason, retry_after: Duration::from_millis(retry_after_ms) };
    let mut buf = Vec::new();

    write_msg(send, &Message::Hello { protocol: PROTOCOL_VERSION, name: c.client_name.clone() }).await.map_err(proto)?;
    let server_name = match read_msg(recv, &mut buf).await.map_err(proto)? {
        Message::HelloAck { protocol, name } if protocol == PROTOCOL_VERSION => name,
        Message::HelloAck { protocol, .. } => {
            return Err(ClientError::Protocol(format!("server speaks protocol {protocol}, this client {PROTOCOL_VERSION}")));
        }
        Message::AuthFailed { reason, retry_after_ms } => return Err(failed(reason, retry_after_ms)),
        other => return Err(ClientError::Protocol(format!("expected HelloAck, got {other:?}"))),
    };

    match auth {
        Auth::Token(token) => {
            write_msg(send, &Message::AuthResume { token }).await.map_err(proto)?;
            match read_msg(recv, &mut buf).await.map_err(proto)? {
                // No server signature on this path: the pinned TLS identity
                // already authenticates the server.
                Message::AuthOk { token, token_ttl_secs, shares, .. } => {
                    Ok((server_name, token, Duration::from_secs(token_ttl_secs), shares))
                }
                Message::AuthFailed { reason, retry_after_ms } => Err(failed(reason, retry_after_ms)),
                other => Err(ClientError::Protocol(format!("expected AuthOk, got {other:?}"))),
            }
        }
        Auth::Password(password) => {
            let client_nonce = random::<32>();
            write_msg(send, &Message::AuthStart { user: c.user.clone(), client_nonce }).await.map_err(proto)?;
            let (salt, kdf, server_nonce) = match read_msg(recv, &mut buf).await.map_err(proto)? {
                Message::AuthChallenge { salt, kdf, server_nonce } => (salt, kdf, server_nonce),
                Message::AuthFailed { reason, retry_after_ms } => return Err(failed(reason, retry_after_ms)),
                other => return Err(ClientError::Protocol(format!("expected AuthChallenge, got {other:?}"))),
            };
            if kdf.m_cost > MAX_KDF_M_KIB || kdf.t_cost > MAX_KDF_T || kdf.p_cost > MAX_KDF_P {
                return Err(ClientError::Protocol(format!("server asked for an unreasonable KDF cost {kdf:?}")));
            }
            let keys = {
                let (pw, salt) = (password.0.clone(), salt.clone());
                tokio::task::spawn_blocking(move || ClientKeys::derive(&pw, &salt, kdf))
                    .await
                    .map_err(|e| ClientError::Protocol(e.to_string()))?
                    .map_err(|e| ClientError::Protocol(e.to_string()))?
            };
            let msg = auth_message(&c.user, &client_nonce, &server_nonce, &salt, kdf, &channel_binding(conn)?);
            write_msg(send, &Message::AuthProof { proof: keys.proof(&msg) }).await.map_err(proto)?;
            match read_msg(recv, &mut buf).await.map_err(proto)? {
                Message::AuthOk { server_signature, token, token_ttl_secs, shares } => {
                    if !keys.verify_server(&msg, &server_signature) {
                        return Err(ClientError::ServerNotAuthenticated);
                    }
                    Ok((server_name, token, Duration::from_secs(token_ttl_secs), shares))
                }
                Message::AuthFailed { reason, retry_after_ms } => Err(failed(reason, retry_after_ms)),
                other => Err(ClientError::Protocol(format!("expected AuthOk, got {other:?}"))),
            }
        }
    }
}

fn channel_binding(conn: &quinn::Connection) -> Result<[u8; 32], ClientError> {
    let mut cb = [0u8; 32];
    conn.export_keying_material(&mut cb, EXPORTER_LABEL, b"")
        .map_err(|_| ClientError::Protocol("TLS exporter unavailable".into()))?;
    Ok(cb)
}

// ---------------------------------------------------------------------------
// Push requests and helpers
// ---------------------------------------------------------------------------

/// One file to upload.
#[derive(Debug, Clone)]
pub struct PushRequest<'a> {
    pub local: &'a Path,
    /// `share/dir/name`.
    pub remote: &'a str,
    pub algo: HashAlgo,
    pub overwrite: bool,
    /// `None` = derived from path, size and mtime (so a re-offer resumes).
    /// Pass [`fresh_source_id`] to force a full re-send after a mismatch.
    pub source_id: Option<[u8; 16]>,
}

/// Job controls a push obeys.
#[derive(Debug, Clone, Copy)]
pub struct PushControl<'a> {
    pub control: &'a JobControl,
    pub limiter: Option<&'a RateLimiter>,
}

#[derive(Debug, Clone)]
pub struct PushOutcome {
    pub size: u64,
    /// Bytes the server already held from an earlier attempt.
    pub resumed_from: u64,
    pub digest: Digest,
}

/// Progress callbacks with *absolute* file offsets. After a resume the
/// offsets restart at `resume_from`, which can be below an earlier ack, so
/// observers should keep the maximum rather than add deltas.
pub trait PushObserver: Send + Sync {
    /// Bytes up to `offset` are on the wire.
    fn sent(&self, _offset: u64) {}
    /// The server confirmed bytes up to `offset` as written.
    fn acked(&self, _offset: u64) {}
}

impl PushObserver for () {}

/// Identifies one version of a source file: `sha256(path | size | mtime)[..16]`.
pub fn source_id(path: &Path, size: u64, mtime: (i64, u32)) -> [u8; 16] {
    let mut h = Sha256::new();
    h.update(path.to_string_lossy().as_bytes());
    h.update([0]);
    h.update(size.to_le_bytes());
    h.update(mtime.0.to_le_bytes());
    h.update(mtime.1.to_le_bytes());
    let mut id = [0u8; 16];
    id.copy_from_slice(&h.finalize()[..16]);
    id
}

/// A random source id: the server will not resume any earlier partial.
pub fn fresh_source_id() -> [u8; 16] {
    random()
}

fn unix_mode(meta: &std::fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o7777
    }
    #[cfg(not(unix))]
    {
        let _ = meta;
        0
    }
}

fn hash_prefix(path: &Path, algo: HashAlgo, len: u64) -> io::Result<Hasher> {
    let mut hasher = Hasher::new(to_engine(algo));
    if len == 0 {
        return Ok(hasher);
    }
    let mut file = std::fs::File::open(path)?.take(len);
    let mut buf = vec![0u8; CHUNK];
    let mut total = 0u64;
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    if total < len {
        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "file shrank before the resume point"));
    }
    Ok(hasher)
}

async fn read_full(file: &mut tokio::fs::File, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..]).await? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(s: &str) -> RemoteSpec {
        RemoteSpec::parse(s).unwrap_or_else(|e| panic!("{e}"))
    }

    #[test]
    fn parses_user_host_port_and_path() {
        let s = spec("alice@nas:/backup/photos/");
        assert_eq!((s.user.as_deref(), s.host.as_str(), s.port, s.path.as_str()), (Some("alice"), "nas", DEFAULT_PORT, "backup/photos"));
        assert_eq!(s.share(), "backup");

        let s = spec("nas.local:47801:/backup");
        assert_eq!((s.user, s.host.as_str(), s.port, s.path.as_str()), (None, "nas.local", 47801, "backup"));

        let s = spec("192.168.1.50:/b//x");
        assert_eq!((s.host.as_str(), s.path.as_str()), ("192.168.1.50", "b/x"));
    }

    #[test]
    fn parses_bracketed_ipv6() {
        let s = spec("bob@[fe80::1]:9000:/s/d");
        assert_eq!((s.user.as_deref(), s.host.as_str(), s.port, s.path.as_str()), (Some("bob"), "fe80::1", 9000, "s/d"));
        assert_eq!(s.host_port(), "[fe80::1]:9000");
        assert_eq!(spec("[::1]:/s").port, DEFAULT_PORT);
        assert_eq!(spec("[::1]:/s").to_string(), "[::1]:47800:/s");
    }

    #[test]
    fn rejects_drive_letters_and_malformed_targets() {
        for bad in [
            "C:/Users/me",
            "c:/x",
            "nas:backup",
            "nas:/",
            "nas:/b/../c",
            "nas:/b/./c",
            "@nas:/b",
            "nas:99999:/b",
            "nas:0:/b",
            "[::1:/b",
            "[nas]:/b",
            ":/b",
            "nas",
            "/local/path",
            "nas:/a\\b",
        ] {
            assert!(RemoteSpec::parse(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn display_round_trips() {
        for s in ["alice@nas:47800:/backup/x", "nas:1234:/s"] {
            assert_eq!(spec(s).to_string(), s);
            assert_eq!(spec(&spec(s).to_string()), spec(s));
        }
    }

    #[test]
    fn join_keeps_only_normal_components() {
        let s = spec("nas:/backup/2026");
        assert_eq!(s.join(Path::new("photos/a.jpg")), "backup/2026/photos/a.jpg");
        assert_eq!(s.join(Path::new("./x/../y")), "backup/2026/x/y");
        assert_eq!(s.join(Path::new("")), "backup/2026");
    }

    #[test]
    fn source_id_tracks_size_and_mtime() {
        let p = Path::new("/data/a.iso");
        let a = source_id(p, 10, (1_700_000_000, 0));
        assert_eq!(a, source_id(p, 10, (1_700_000_000, 0)));
        assert_ne!(a, source_id(p, 11, (1_700_000_000, 0)));
        assert_ne!(a, source_id(p, 10, (1_700_000_000, 1)));
        assert_ne!(a, source_id(Path::new("/data/b.iso"), 10, (1_700_000_000, 0)));
        assert_ne!(fresh_source_id(), fresh_source_id());
    }

    #[test]
    fn prefix_hash_matches_a_full_hash_of_the_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f");
        let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7) as u8).collect();
        std::fs::write(&p, &data).unwrap();
        let mut whole = Hasher::new(fc_engine::Algorithm::Sha256);
        whole.update(&data[..200_000]);
        assert_eq!(hash_prefix(&p, HashAlgo::Sha256, 200_000).unwrap().finalize(), whole.finalize());
        assert!(hash_prefix(&p, HashAlgo::Sha256, 400_000).is_err(), "prefix longer than the file");
    }
}

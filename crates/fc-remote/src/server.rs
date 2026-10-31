//! The receiving agent: accepts QUIC connections, authenticates, writes files
//! into configured shares, verifies them, and resumes interrupted uploads.
//!
//! ```text
//! Endpoint::accept ─▶ IP allowlist + lockout check (refuse before TLS work)
//!       │
//!       ▼  per connection (task)
//!  control stream: Hello → SCRAM / token resume → AuthOk
//!       │
//!       ├─ control reader task  (client Pause/Resume/Ping/Bye)
//!       ├─ control writer task  (server-side Pause/Resume broadcast)
//!       └─ accept_bi loop ─▶ per-entry task: offer → resolve path → write .fcpart
//!                                            → ack → fsync → re-read + hash → rename
//! ```

use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, watch};

use fc_engine::{Algorithm, Hasher};

use crate::access::{LoginLimiter, SessionTokens};
use crate::config::{Account, AgentConfig, Share};
use crate::crypto::{auth_message, random, Verifier, EXPORTER_LABEL};
use crate::protocol::*;
use crate::tls::{server_config, Identity};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);
const ACK_EVERY: u64 = 4 * 1024 * 1024;
/// Partial uploads resume from a multiple of this (the tail may be torn).
pub(crate) const RESUME_ALIGN: u64 = 64 * 1024;
pub(crate) const TEMP_SUFFIX: &str = ".fcpart";
pub(crate) const ID_SUFFIX: &str = ".fcpart.id";

/// A connected, authenticated client (for status displays).
#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub id: u64,
    pub account: String,
    pub peer: SocketAddr,
    pub paused_by_client: bool,
    pub bytes_received: u64,
    pub files_completed: u64,
}

/// Test-only fault injection.
#[doc(hidden)]
#[derive(Debug, Default)]
pub struct TestHooks {
    /// Abruptly close the connection once this many file bytes have arrived (once).
    pub drop_after_bytes: AtomicU64,
    /// Close the next sufficiently large download at this file offset, once.
    pub drop_pull_after_bytes: AtomicU64,
}

struct State {
    config: AgentConfig,
    fingerprint: String,
    limiter: LoginLimiter,
    tokens: SessionTokens,
    paused: watch::Sender<bool>,
    sessions: Mutex<Vec<SessionInfo>>,
    next_session: AtomicU64,
    hooks: TestHooks,
}

/// Control surface for a running server (status, pause, shutdown).
#[derive(Clone)]
pub struct ServerHandle {
    state: Arc<State>,
    endpoint: quinn::Endpoint,
}

impl ServerHandle {
    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        self.endpoint.local_addr()
    }
    pub fn fingerprint(&self) -> &str {
        &self.state.fingerprint
    }
    /// Pause every incoming transfer. Clients are told, and the server also
    /// stops reading, so flow control holds senders that ignore the message.
    pub fn pause_all(&self) {
        // `send_replace` stores the value even with no session connected yet.
        self.state.paused.send_replace(true);
    }
    pub fn resume_all(&self) {
        self.state.paused.send_replace(false);
    }
    pub fn sessions(&self) -> Vec<SessionInfo> {
        self.state.sessions.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn shutdown(&self) {
        self.endpoint.close(0u32.into(), b"server shutting down");
    }
    #[doc(hidden)]
    pub fn hooks(&self) -> &TestHooks {
        &self.state.hooks
    }
}

pub struct Server {
    handle: ServerHandle,
}

impl Server {
    /// Bind the UDP socket. Must be called inside a Tokio runtime.
    pub fn bind(config: AgentConfig, identity: &Identity) -> io::Result<Self> {
        config.validate()?;
        let endpoint = quinn::Endpoint::server(server_config(identity)?, config.listen)?;
        let state = Arc::new(State {
            limiter: LoginLimiter::new(config.limits.clone()),
            tokens: SessionTokens::new(Duration::from_secs(config.limits.session_ttl_secs), config.limits.max_sessions),
            fingerprint: identity.fingerprint(),
            paused: watch::channel(false).0,
            sessions: Mutex::new(Vec::new()),
            next_session: AtomicU64::new(1),
            hooks: TestHooks::default(),
            config,
        });
        Ok(Self { handle: ServerHandle { state, endpoint } })
    }

    pub fn handle(&self) -> ServerHandle {
        self.handle.clone()
    }

    /// Accept connections until [`ServerHandle::shutdown`].
    pub async fn run(self) {
        let ServerHandle { state, endpoint } = self.handle;
        while let Some(incoming) = endpoint.accept().await {
            let ip = incoming.remote_address().ip().to_canonical();
            // Cheapest possible rejection: before any TLS work.
            if !state.config.allows(ip) {
                tracing::warn!(%ip, "connection refused: not in allowlist");
                incoming.refuse();
                continue;
            }
            if state.limiter.check(ip, None).is_err() {
                tracing::warn!(%ip, "connection refused: locked out after failed logins");
                incoming.refuse();
                continue;
            }
            let state = state.clone();
            tokio::spawn(async move {
                match incoming.await {
                    Ok(conn) => {
                        if let Err(e) = handle_connection(state, conn, ip).await {
                            tracing::debug!(%ip, "connection ended: {e}");
                        }
                    }
                    Err(e) => tracing::debug!(%ip, "handshake failed: {e}"),
                }
            });
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum ConnError {
    #[error(transparent)]
    Proto(#[from] ProtoError),
    #[error(transparent)]
    Quic(#[from] quinn::ConnectionError),
    #[error("authentication failed")]
    AuthFailed,
    #[error("timed out")]
    Timeout,
}

fn channel_binding(conn: &quinn::Connection) -> [u8; 32] {
    let mut cb = [0u8; 32];
    conn.export_keying_material(&mut cb, EXPORTER_LABEL, b"").expect("TLS 1.3 exporter available");
    cb
}

async fn handle_connection(state: Arc<State>, conn: quinn::Connection, ip: IpAddr) -> Result<(), ConnError> {
    let (mut send, mut recv) = tokio::time::timeout(HANDSHAKE_TIMEOUT, conn.accept_bi())
        .await
        .map_err(|_| ConnError::Timeout)??;
    let account = match tokio::time::timeout(HANDSHAKE_TIMEOUT, authenticate(&state, &conn, ip, &mut send, &mut recv)).await {
        Ok(Ok(a)) => a,
        Ok(Err(e)) => {
            close_after_flush(&mut send, &conn, b"authentication failed").await;
            return Err(e);
        }
        Err(_) => {
            close_after_flush(&mut send, &conn, b"handshake timeout").await;
            return Err(ConnError::Timeout);
        }
    };

    let session_id = state.next_session.fetch_add(1, Ordering::Relaxed);
    state.sessions.lock().unwrap_or_else(|e| e.into_inner()).push(SessionInfo {
        id: session_id,
        account: account.name.clone(),
        peer: conn.remote_address(),
        paused_by_client: false,
        bytes_received: 0,
        files_completed: 0,
    });
    tracing::info!(peer = %conn.remote_address(), account = %account.name, "session started");

    // Control writer: forwards server-side pause/resume to the client.
    let (ctl_tx, mut ctl_rx) = mpsc::channel::<Message>(16);
    let writer = tokio::spawn(async move {
        while let Some(m) = ctl_rx.recv().await {
            if write_msg(&mut send, &m).await.is_err() {
                break;
            }
        }
    });
    let mut paused_rx = state.paused.subscribe();
    let pause_fwd = {
        let tx = ctl_tx.clone();
        tokio::spawn(async move {
            if *paused_rx.borrow_and_update() {
                let _ = tx.send(Message::Pause).await;
            }
            while paused_rx.changed().await.is_ok() {
                let m = if *paused_rx.borrow_and_update() { Message::Pause } else { Message::Resume };
                if tx.send(m).await.is_err() {
                    break;
                }
            }
        })
    };
    // Control reader: client-side pause notices, pings.
    let reader = {
        let (state, tx) = (state.clone(), ctl_tx.clone());
        tokio::spawn(async move {
            let mut buf = Vec::new();
            while let Ok(m) = read_msg(&mut recv, &mut buf).await {
                match m {
                    Message::Pause | Message::Resume => update_session(&state, session_id, |s| s.paused_by_client = m == Message::Pause),
                    Message::Ping { nonce } => {
                        let _ = tx.send(Message::Pong { nonce }).await;
                    }
                    Message::Bye => break,
                    _ => {}
                }
            }
        })
    };

    // Entry streams until the connection ends.
    let account = Arc::new(account);
    let result = loop {
        match conn.accept_bi().await {
            Ok((s, r)) => {
                let (state, account, conn) = (state.clone(), account.clone(), conn.clone());
                tokio::spawn(async move {
                    if let Err(e) = handle_entry(&state, &account, &conn, session_id, s, r).await {
                        tracing::debug!("entry stream ended: {e}");
                    }
                });
            }
            Err(quinn::ConnectionError::ApplicationClosed(_)) | Err(quinn::ConnectionError::LocallyClosed) => break Ok(()),
            Err(e) => break Err(ConnError::Quic(e)),
        }
    };
    for t in [writer, pause_fwd, reader] {
        t.abort();
    }
    state.sessions.lock().unwrap_or_else(|e| e.into_inner()).retain(|s| s.id != session_id);
    tracing::info!(peer = %conn.remote_address(), "session ended");
    result
}

/// Close the connection only after the queued reply (e.g. `AuthFailed`) has
/// been acknowledged. Closing first discards unsent stream data, and the peer
/// would then see only "connection closed" with no reason.
async fn close_after_flush(send: &mut quinn::SendStream, conn: &quinn::Connection, reason: &'static [u8]) {
    let _ = send.finish();
    // `stopped` completes once the peer has read the stream to its end (or
    // stopped it); the timeout keeps a silent peer from holding us open.
    let _ = tokio::time::timeout(Duration::from_secs(1), send.stopped()).await;
    conn.close(1u32.into(), reason);
}

fn update_session(state: &State, id: u64, f: impl FnOnce(&mut SessionInfo)) {
    if let Some(s) = state.sessions.lock().unwrap_or_else(|e| e.into_inner()).iter_mut().find(|s| s.id == id) {
        f(s);
    }
}

/// Hello + (SCRAM | token resume). Returns the authenticated account.
async fn authenticate(
    state: &State,
    conn: &quinn::Connection,
    ip: IpAddr,
    send: &mut quinn::SendStream,
    recv: &mut quinn::RecvStream,
) -> Result<Account, ConnError> {
    let mut buf = Vec::new();
    match read_msg(recv, &mut buf).await? {
        Message::Hello { protocol, .. } if protocol == PROTOCOL_VERSION => {}
        Message::Hello { protocol, .. } => {
            write_msg(send, &Message::AuthFailed { reason: format!("protocol {protocol} not supported (server speaks {PROTOCOL_VERSION})"), retry_after_ms: 0 }).await?;
            return Err(ConnError::AuthFailed);
        }
        other => return Err(ProtoError::Unexpected { expected: "Hello", got: Box::new(other) }.into()),
    }
    write_msg(send, &Message::HelloAck { protocol: PROTOCOL_VERSION, name: state.config.name.clone() }).await?;

    let shares_for = |a: &Account| {
        state.config.shares_for(a).map(|s| ShareInfo { name: s.name.clone(), writable: s.writable }).collect::<Vec<_>>()
    };

    match read_msg(recv, &mut buf).await? {
        // ----- reconnect with a session token -----
        Message::AuthResume { token } => {
            let account = state.tokens.redeem(&token).and_then(|name| state.config.account(&name).cloned());
            match account {
                Some(a) => {
                    let token = state.tokens.issue(&a.name);
                    write_msg(send, &Message::AuthOk {
                        server_signature: [0; 32], // not meaningful for token auth; TLS pin authenticates the server
                        token,
                        token_ttl_secs: state.tokens.ttl().as_secs(),
                        shares: shares_for(&a),
                    })
                    .await?;
                    Ok(a)
                }
                None => {
                    let retry = state.limiter.failure(ip, None).unwrap_or_default();
                    write_msg(send, &Message::AuthFailed { reason: "session expired".into(), retry_after_ms: retry.as_millis() as u64 }).await?;
                    Err(ConnError::AuthFailed)
                }
            }
        }
        // ----- full SCRAM exchange -----
        Message::AuthStart { user, client_nonce } => {
            if let Err(wait) = state.limiter.check(ip, Some(&user)) {
                write_msg(send, &Message::AuthFailed { reason: "too many failed attempts".into(), retry_after_ms: wait.as_millis() as u64 }).await?;
                return Err(ConnError::AuthFailed);
            }
            let account = state.config.account(&user).cloned();
            // Unknown users get an indistinguishable challenge.
            let verifier = account.as_ref().map(|a| a.verifier.clone()).unwrap_or_else(|| Verifier::decoy(&state.config.server_secret, &user));
            let server_nonce = random::<32>();
            write_msg(send, &Message::AuthChallenge { salt: verifier.salt.clone(), kdf: verifier.kdf, server_nonce }).await?;
            let proof = match read_msg(recv, &mut buf).await? {
                Message::AuthProof { proof } => proof,
                other => return Err(ProtoError::Unexpected { expected: "AuthProof", got: Box::new(other) }.into()),
            };
            let msg = auth_message(&user, &client_nonce, &server_nonce, &verifier.salt, verifier.kdf, &channel_binding(conn));
            let ok = verifier.check_proof(&msg, &proof) && account.is_some();
            match (ok, account) {
                (true, Some(a)) => {
                    state.limiter.success(ip, &a.name);
                    let token = state.tokens.issue(&a.name);
                    write_msg(send, &Message::AuthOk {
                        server_signature: verifier.server_signature(&msg),
                        token,
                        token_ttl_secs: state.tokens.ttl().as_secs(),
                        shares: shares_for(&a),
                    })
                    .await?;
                    Ok(a)
                }
                _ => {
                    let lock = state.limiter.failure(ip, Some(&user));
                    tracing::warn!(%ip, %user, locked = lock.is_some(), "failed login");
                    // Uniform delay: failures are slow, successes are not.
                    tokio::time::sleep(Duration::from_millis(400)).await;
                    write_msg(send, &Message::AuthFailed {
                        reason: "invalid username or password".into(),
                        retry_after_ms: lock.map(|d| d.as_millis() as u64).unwrap_or(0),
                    })
                    .await?;
                    Err(ConnError::AuthFailed)
                }
            }
        }
        other => Err(ProtoError::Unexpected { expected: "AuthStart or AuthResume", got: Box::new(other) }.into()),
    }
}

// ---------------------------------------------------------------------------
// Path resolution (the security boundary)
// ---------------------------------------------------------------------------

type Reject = (RejectReason, String);

/// Validate `share/rel/path` for `account`; returns the share and the joined path.
/// Rejects anything that could escape the share root.
pub fn resolve<'c>(config: &'c AgentConfig, account: &Account, remote: &str, write: bool) -> Result<(&'c Share, PathBuf), Reject> {
    let bad = |m: &str| (RejectReason::BadPath, m.to_string());
    let remote = remote.trim_start_matches('/');
    let (share_name, rel) = remote.split_once('/').unwrap_or((remote, ""));
    let share = config.share(share_name).ok_or_else(|| bad("no such share"))?;
    if !config.shares_for(account).any(|s| s.name == share.name) {
        return Err((RejectReason::Denied, format!("account may not use share '{share_name}'")));
    }
    if write && !share.writable {
        return Err((RejectReason::Denied, format!("share '{share_name}' is read-only")));
    }
    let mut path = share.path.clone();
    for c in rel.split('/').filter(|c| !c.is_empty()) {
        if c == "." || c == ".." {
            return Err(bad("'.' and '..' are not allowed"));
        }
        if c.contains(['\\', '\0']) {
            return Err(bad("illegal character in path"));
        }
        // On Windows a ':' would address an alternate data stream or a drive.
        if cfg!(windows) && (c.contains(':') || fc_engine::platform::names::sanitize_for_windows(c) != c) {
            return Err(bad("name not allowed on this server's filesystem"));
        }
        path.push(c);
    }
    Ok((share, path))
}

/// After creating directories: make sure no symlink led us outside the share.
fn ensure_inside(share_root: &Path, dir: &Path) -> Result<(), Reject> {
    let root = fs::canonicalize(share_root).map_err(|e| (RejectReason::Io, e.to_string()))?;
    let real = fs::canonicalize(dir).map_err(|e| (RejectReason::Io, e.to_string()))?;
    if real.starts_with(&root) { Ok(()) } else { Err((RejectReason::BadPath, "path escapes the share via a symlink".into())) }
}

pub(crate) fn sidecar(path: &Path, suffix: &str) -> PathBuf {
    let mut s = path.as_os_str().to_os_string();
    s.push(suffix);
    PathBuf::from(s)
}

/// Open (or reopen) the temp file without following symlinks.
pub(crate) fn open_temp(tmp: &Path) -> io::Result<fs::File> {
    if fs::symlink_metadata(tmp).is_ok_and(|m| m.file_type().is_symlink()) {
        fs::remove_file(tmp)?;
    }
    let mut o = fs::OpenOptions::new();
    o.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.custom_flags(libc::O_NOFOLLOW);
    }
    o.open(tmp)
}

struct Prepared {
    file: fs::File,
    tmp: PathBuf,
    dest: PathBuf,
    resume_from: u64,
    overwrite: bool,
}

#[allow(clippy::too_many_arguments)]
fn prepare_file(share: &Share, dest: PathBuf, size: u64, source_id: [u8; 16], overwrite: bool) -> Result<Prepared, Reject> {
    let io_err = |e: io::Error| (RejectReason::Io, e.to_string());
    let parent = dest.parent().ok_or((RejectReason::BadPath, "no parent".into()))?.to_path_buf();
    fs::create_dir_all(&parent).map_err(io_err)?;
    ensure_inside(&share.path, &parent)?;
    if let Ok(m) = fs::symlink_metadata(&dest) {
        if m.is_dir() {
            return Err((RejectReason::Exists, "a folder with that name exists".into()));
        }
        if !overwrite {
            return Err((RejectReason::Exists, "destination exists".into()));
        }
    }
    let tmp = sidecar(&dest, TEMP_SUFFIX);
    let id_path = sidecar(&dest, ID_SUFFIX);

    // Resume only a partial from the *same* source version.
    let same_source = fs::read(&id_path).is_ok_and(|id| id == source_id);
    let existing = if same_source { fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0) } else { 0 };
    let resume_from = (existing.min(size) / RESUME_ALIGN) * RESUME_ALIGN;

    let available = fs4::available_space(&parent).unwrap_or(u64::MAX);
    if available < size.saturating_sub(resume_from) {
        return Err((RejectReason::NoSpace, format!("need {} bytes, {available} available", size - resume_from)));
    }

    let mut file = open_temp(&tmp).map_err(io_err)?;
    file.set_len(resume_from).map_err(io_err)?;
    file.seek(SeekFrom::Start(resume_from)).map_err(io_err)?;
    fs::write(&id_path, source_id).map_err(io_err)?;
    Ok(Prepared { file, tmp, dest, resume_from, overwrite })
}

/// fsync, re-read from disk (cache dropped where possible), hash, compare, commit.
fn finalize_file(p: &Prepared, algo: HashAlgo, expected: &[u8], mtime: (i64, u32), mode: u32) -> io::Result<Result<Vec<u8>, Vec<u8>>> {
    p.file.sync_all()?;
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: plain advisory syscall on a valid fd.
        unsafe { libc::posix_fadvise(p.file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
    }
    let mut hasher = Hasher::new(to_engine(algo));
    let mut f = fs::File::open(&p.tmp)?;
    let mut buf = vec![0u8; 1024 * 1024];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let digest = hasher.finalize().as_bytes().to_vec();
    if digest != expected {
        let _ = fs::remove_file(&p.tmp);
        let _ = fs::remove_file(sidecar(&p.dest, ID_SUFFIX));
        return Ok(Err(digest));
    }
    if mtime.0 >= 0 {
        let t = std::time::UNIX_EPOCH + Duration::new(mtime.0 as u64, mtime.1);
        let _ = p.file.set_times(fs::FileTimes::new().set_modified(t));
    }
    #[cfg(unix)]
    if mode != 0 {
        use std::os::unix::fs::PermissionsExt;
        let _ = p.file.set_permissions(fs::Permissions::from_mode(mode & 0o777));
    }
    #[cfg(not(unix))]
    let _ = mode;
    fc_engine::conflict::commit_file(&p.tmp, &p.dest, p.overwrite)?;
    let _ = fs::remove_file(sidecar(&p.dest, ID_SUFFIX));
    Ok(Ok(digest))
}

pub(crate) fn to_engine(a: HashAlgo) -> Algorithm {
    match a {
        HashAlgo::Xxh3 => Algorithm::Xxh3,
        HashAlgo::Sha256 => Algorithm::Sha256,
    }
}

async fn handle_entry(
    state: &Arc<State>,
    account: &Account,
    conn: &quinn::Connection,
    session_id: u64,
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
) -> Result<(), ProtoError> {
    let mut buf = Vec::with_capacity(MAX_FRAME);
    let (kind, path, size, mtime, mode, algo, source_id, overwrite) = match read_msg(&mut recv, &mut buf).await? {
        Message::FileOffer { kind, path, size, mtime, mode, algo, source_id, overwrite } => (kind, path, size, mtime, mode, algo, source_id, overwrite),
        Message::FileRequest { path, algo } => return handle_pull(state, account, conn, send, recv, path, algo).await,
        Message::ListDir { path } => return handle_list(state, account, send, path).await,
        other => return Err(ProtoError::Unexpected { expected: "FileOffer, FileRequest or ListDir", got: Box::new(other) }),
    };
    let reject = |(reason, message): Reject| Message::FileReject { reason, message };

    let (share, dest) = match resolve(&state.config, account, &path, true) {
        Ok((s, d)) => (s.clone(), d),
        Err(r) => {
            write_msg(&mut send, &reject(r)).await?;
            return Ok(());
        }
    };

    if kind == EntryKind::Dir {
        let res = tokio::task::spawn_blocking(move || {
            fs::create_dir_all(&dest).map_err(|e| (RejectReason::Io, e.to_string()))?;
            ensure_inside(&share.path, &dest)
        })
        .await
        .expect("mkdir task");
        let m = match res {
            Ok(()) => Message::FileDone { ok: true, digest: vec![], message: String::new() },
            Err(r) => reject(r),
        };
        write_msg(&mut send, &m).await?;
        let _ = send.finish();
        return Ok(());
    }

    let prepared = tokio::task::spawn_blocking(move || prepare_file(&share, dest, size, source_id, overwrite)).await.expect("prepare task");
    let prepared = match prepared {
        Ok(p) => p,
        Err(r) => {
            write_msg(&mut send, &reject(r)).await?;
            return Ok(());
        }
    };
    write_msg(&mut send, &Message::FileAccept { resume_from: prepared.resume_from }).await?;

    let mut file = tokio::fs::File::from_std(prepared.file.try_clone().map_err(ProtoError::Io)?);
    let mut expected = prepared.resume_from;
    let mut last_ack = expected;
    let mut paused = state.paused.subscribe();

    let end = loop {
        // Server-side pause: stop reading; QUIC flow control stalls the sender.
        while *paused.borrow_and_update() {
            if paused.changed().await.is_err() {
                break;
            }
        }
        match read_frame(&mut recv, &mut buf).await? {
            Some(Frame::Data { offset, bytes }) => {
                if offset != expected || expected + bytes.len() as u64 > size {
                    return Err(ProtoError::Malformed(format!("data at {offset}, expected {expected}")));
                }
                file.write_all(bytes).await?;
                expected += bytes.len() as u64;
                update_session(state, session_id, |s| s.bytes_received += bytes.len() as u64);

                let drop_at = state.hooks.drop_after_bytes.load(Ordering::Relaxed);
                if drop_at > 0 && expected >= drop_at {
                    state.hooks.drop_after_bytes.store(0, Ordering::Relaxed);
                    file.flush().await?;
                    conn.close(0u32.into(), b"test: simulated network drop");
                    return Ok(());
                }
                if expected - last_ack >= ACK_EVERY {
                    write_msg(&mut send, &Message::Ack { offset: expected }).await?;
                    last_ack = expected;
                }
            }
            Some(Frame::Msg(Message::FileEnd { size: end_size, digest })) => break (end_size, digest),
            Some(Frame::Msg(other)) => return Err(ProtoError::Unexpected { expected: "Data or FileEnd", got: Box::new(other) }),
            None => return Err(ProtoError::Closed), // sender went away: keep the partial for resume
        }
    };
    file.flush().await?;
    drop(file);
    write_msg(&mut send, &Message::Ack { offset: expected }).await?;

    let (end_size, digest) = end;
    let done = if end_size != size || expected != size {
        Message::FileDone { ok: false, digest: vec![], message: format!("size mismatch: got {expected} of {size} bytes") }
    } else {
        let result = tokio::task::spawn_blocking(move || finalize_file(&prepared, algo, &digest, mtime, mode)).await.expect("finalize task");
        match result {
            Ok(Ok(d)) => {
                update_session(state, session_id, |s| s.files_completed += 1);
                Message::FileDone { ok: true, digest: d, message: String::new() }
            }
            Ok(Err(d)) => Message::FileDone { ok: false, digest: d, message: "checksum mismatch after writing; partial removed".into() },
            Err(e) => Message::FileDone { ok: false, digest: vec![], message: format!("finalize failed: {e}") },
        }
    };
    write_msg(&mut send, &done).await?;
    let _ = send.finish();
    Ok(())
}

// ---------------------------------------------------------------------------
// Download: read-only access to a share
// ---------------------------------------------------------------------------

/// Canonicalise `path` and make sure it is still inside the share, so a
/// symlink can't lead a reader out of it. Returns the real path and its metadata.
fn stat_inside(share: &Share, path: &Path) -> Result<(PathBuf, fs::Metadata), Reject> {
    let io_err = |e: io::Error| (RejectReason::Io, e.to_string());
    let real = fs::canonicalize(path).map_err(|e| match e.kind() {
        io::ErrorKind::NotFound => (RejectReason::NotFound, "no such file or directory".to_string()),
        _ => io_err(e),
    })?;
    let root = fs::canonicalize(&share.path).map_err(io_err)?;
    if !real.starts_with(&root) {
        return Err((RejectReason::BadPath, "path escapes the share via a symlink".into()));
    }
    let meta = fs::metadata(&real).map_err(io_err)?;
    Ok((real, meta))
}

fn mtime_of(meta: &fs::Metadata) -> (i64, u32) {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| (d.as_secs() as i64, d.subsec_nanos()))
        .unwrap_or((-1, 0))
}

fn mode_of(meta: &fs::Metadata) -> u32 {
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

struct PullFile {
    file: fs::File,
    size: u64,
    mtime: (i64, u32),
    mode: u32,
}

fn open_for_read(share: &Share, path: &Path) -> Result<PullFile, Reject> {
    let io_err = |e: io::Error| (RejectReason::Io, e.to_string());
    let (real, meta) = stat_inside(share, path)?;
    if meta.is_dir() {
        return Err((RejectReason::IsDirectory, "that path is a directory".into()));
    }
    // Opening a FIFO or device would block or misbehave.
    if !meta.is_file() {
        return Err((RejectReason::BadPath, "not a regular file".into()));
    }
    let file = fs::File::open(&real).map_err(io_err)?;
    let meta = file.metadata().map_err(io_err)?;
    Ok(PullFile { file, size: meta.len(), mtime: mtime_of(&meta), mode: mode_of(&meta) })
}

fn list_dir(share: &Share, dir: &Path) -> Result<Vec<DirEntry>, Reject> {
    let io_err = |e: io::Error| (RejectReason::Io, e.to_string());
    let (real, meta) = stat_inside(share, dir)?;
    if !meta.is_dir() {
        return Err((RejectReason::NotDirectory, "that path is not a directory".into()));
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(&real).map_err(io_err)? {
        let Ok(entry) = entry else { continue };
        let Some(name) = entry.file_name().to_str().map(str::to_string) else { continue };
        // Our own partial-upload files, and anything that isn't a plain name.
        if name.ends_with(TEMP_SUFFIX) || name.ends_with(ID_SUFFIX) || name.contains(['/', '\\', '\0']) {
            continue;
        }
        // Entries that resolve outside the share, or are neither file nor folder, are not listed.
        let Ok((_, m)) = stat_inside(share, &entry.path()) else { continue };
        let kind = if m.is_dir() {
            EntryKind::Dir
        } else if m.is_file() {
            EntryKind::File
        } else {
            continue;
        };
        out.push(DirEntry { name, kind, size: if m.is_dir() { 0 } else { m.len() }, mtime: mtime_of(&m), mode: mode_of(&m) });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

async fn handle_list(state: &Arc<State>, account: &Account, mut send: quinn::SendStream, path: String) -> Result<(), ProtoError> {
    let reject = |(reason, message): Reject| Message::FileReject { reason, message };
    let listed = match resolve(&state.config, account, &path, false) {
        Ok((share, dir)) => {
            let share = share.clone();
            tokio::task::spawn_blocking(move || list_dir(&share, &dir)).await.expect("list task")
        }
        Err(r) => Err(r),
    };
    match listed {
        Err(r) => write_msg(&mut send, &reject(r)).await?,
        Ok(entries) => {
            let mut pages: Vec<&[DirEntry]> = entries.chunks(LISTING_PAGE).collect();
            if pages.is_empty() {
                pages.push(&[]);
            }
            let last = pages.len() - 1;
            for (i, page) in pages.iter().enumerate() {
                write_msg(&mut send, &Message::DirListing { entries: page.to_vec(), more: i < last }).await?;
            }
        }
    }
    let _ = send.finish();
    Ok(())
}

async fn handle_pull(
    state: &Arc<State>,
    account: &Account,
    conn: &quinn::Connection,
    mut send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    path: String,
    algo: HashAlgo,
) -> Result<(), ProtoError> {
    let reject = |(reason, message): Reject| Message::FileReject { reason, message };
    let opened = match resolve(&state.config, account, &path, false) {
        Ok((share, src)) => {
            let share = share.clone();
            tokio::task::spawn_blocking(move || open_for_read(&share, &src)).await.expect("open task")
        }
        Err(r) => Err(r),
    };
    let pf = match opened {
        Ok(pf) => pf,
        Err(r) => {
            write_msg(&mut send, &reject(r)).await?;
            let _ = send.finish();
            return Ok(());
        }
    };
    let (size, mtime) = (pf.size, pf.mtime);
    let source_id = crate::client::source_id(Path::new(&path), size, mtime);
    write_msg(&mut send, &Message::FileMeta { size, mtime, mode: pf.mode, source_id }).await?;

    let mut buf = Vec::new();
    let resume_from = match read_msg(&mut recv, &mut buf).await? {
        Message::PullStart { resume_from } if resume_from <= size => resume_from,
        Message::PullStart { resume_from } => {
            return Err(ProtoError::Malformed(format!("resume point {resume_from} is past the end ({size})")));
        }
        other => return Err(ProtoError::Unexpected { expected: "PullStart", got: Box::new(other) }),
    };

    // Claim the fault before streaming: an earlier cancelled stream must not
    // consume a hook installed for the next download.
    let candidate = state.hooks.drop_pull_after_bytes.load(Ordering::Relaxed);
    let drop_at = if candidate > 0 && size >= candidate
        && state.hooks.drop_pull_after_bytes.compare_exchange(candidate, 0, Ordering::Relaxed, Ordering::Relaxed).is_ok()
    {
        candidate
    } else { 0 };
    let mut file = tokio::fs::File::from_std(pf.file);
    let mut hasher = Hasher::new(to_engine(algo));
    let mut chunk = vec![0u8; CHUNK];
    let mut offset = 0u64;
    let failed = |message: &str| Message::FileDone { ok: false, digest: vec![], message: message.to_string() };

    // The digest covers the whole file; the part the client already holds is only hashed.
    while offset < resume_from {
        let want = (resume_from - offset).min(CHUNK as u64) as usize;
        let n = crate::client::read_full(&mut file, &mut chunk[..want]).await?;
        if n < want {
            write_msg(&mut send, &failed("file shrank while it was being read")).await?;
            let _ = send.finish();
            return Ok(());
        }
        hasher.update(&chunk[..n]);
        offset += n as u64;
    }

    let mut paused = state.paused.subscribe();
    while offset < size {
        // Server-side pause: stop sending and let the client wait.
        while *paused.borrow_and_update() {
            if paused.changed().await.is_err() {
                break;
            }
        }
        let want = (size - offset).min(CHUNK as u64) as usize;
        let n = crate::client::read_full(&mut file, &mut chunk[..want]).await?;
        if n < want {
            write_msg(&mut send, &failed("file shrank while it was being sent")).await?;
            let _ = send.finish();
            return Ok(());
        }
        hasher.update(&chunk[..n]);
        write_data(&mut send, offset, &chunk[..n]).await?;
        offset += n as u64;

        if drop_at > 0 && offset >= drop_at {
            conn.close(0u32.into(), b"test: simulated network drop");
            return Ok(());
        }
    }

    let after = file.metadata().await?;
    if after.len() != size || mtime_of(&after) != mtime {
        write_msg(&mut send, &failed("file changed while it was being sent")).await?;
        let _ = send.finish();
        return Ok(());
    }
    write_msg(&mut send, &Message::FileEnd { size, digest: hasher.finalize().as_bytes().to_vec() }).await?;
    // The client's verdict arrives after it has written and compared the file.
    if let Ok(Message::FileDone { ok: false, message, .. }) = read_msg(&mut recv, &mut buf).await {
        tracing::warn!(%path, "client rejected the download: {message}");
    }
    let _ = send.finish();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AccountKind, Share};
    use crate::crypto::TEST_KDF;

    fn cfg(root: &Path) -> AgentConfig {
        let mut c = AgentConfig::new("t");
        c.kdf = TEST_KDF;
        c.shares.push(Share { name: "backup".into(), path: root.to_path_buf(), writable: true });
        c.shares.push(Share { name: "ro".into(), path: root.to_path_buf(), writable: false });
        c.set_account("alice", AccountKind::Interactive, "password1", Some(vec!["backup".into(), "ro".into()])).unwrap();
        c.set_account("bob", AccountKind::Interactive, "password1", Some(vec!["ro".into()])).unwrap();
        c
    }

    #[test]
    fn resolve_accepts_normal_paths() {
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(dir.path());
        let (s, p) = resolve(&c, c.account("alice").unwrap(), "/backup/photos/2026/a.jpg", true).unwrap();
        assert_eq!(s.name, "backup");
        assert_eq!(p, dir.path().join("photos/2026/a.jpg"));
    }

    #[test]
    fn resolve_blocks_traversal_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let c = cfg(dir.path());
        let alice = c.account("alice").unwrap();
        for evil in ["backup/../etc/passwd", "backup/./x", "backup/a\\..\\b", "nope/x", "backup/a\0b"] {
            assert!(resolve(&c, alice, evil, true).is_err(), "{evil}");
        }
        assert_eq!(resolve(&c, alice, "ro/x", true).unwrap_err().0, RejectReason::Denied, "read-only share");
        assert_eq!(resolve(&c, c.account("bob").unwrap(), "backup/x", true).unwrap_err().0, RejectReason::Denied, "share not granted");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_detected() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        let share = Share { name: "s".into(), path: dir.path().into(), writable: true };
        let r = prepare_file(&share, dir.path().join("link/evil.txt"), 1, [0; 16], false);
        assert!(matches!(r, Err((RejectReason::BadPath, _))));
        assert!(!outside.path().join("evil.txt.fcpart").exists());
    }

    #[cfg(unix)]
    #[test]
    fn planted_temp_symlink_is_not_followed() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim");
        fs::write(&victim, b"precious").unwrap();
        std::os::unix::fs::symlink(&victim, dir.path().join("f.fcpart")).unwrap();
        let share = Share { name: "s".into(), path: dir.path().into(), writable: true };
        let p = prepare_file(&share, dir.path().join("f"), 10, [1; 16], false).unwrap();
        drop(p);
        assert_eq!(fs::read(&victim).unwrap(), b"precious", "symlink target untouched");
    }

    #[test]
    fn resume_requires_matching_source_id() {
        let dir = tempfile::tempdir().unwrap();
        let share = Share { name: "s".into(), path: dir.path().into(), writable: true };
        let dest = dir.path().join("big");
        fs::write(sidecar(&dest, TEMP_SUFFIX), vec![7u8; 200_000]).unwrap();
        fs::write(sidecar(&dest, ID_SUFFIX), [9u8; 16]).unwrap();
        let p = prepare_file(&share, dest.clone(), 1_000_000, [9; 16], false).unwrap();
        assert_eq!(p.resume_from, 196_608, "rounded down to 64 KiB");
        drop(p);
        let p = prepare_file(&share, dest, 1_000_000, [8; 16], false).unwrap();
        assert_eq!(p.resume_from, 0, "different source version starts over");
    }
}

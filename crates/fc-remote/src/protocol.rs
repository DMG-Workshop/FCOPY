//! The fcopy wire protocol, version 1.
//!
//! # Transport
//!
//! QUIC (RFC 9000) with TLS 1.3, ALPN `fcopy/1`. One QUIC connection per
//! session; inside it:
//!
//! | Stream | Opened by | Carries |
//! |--------|-----------|---------|
//! | first bidirectional stream | client | **control**: `Hello`, authentication, `Pause`/`Resume`, `Ping`, `Bye` |
//! | one bidirectional stream **per file or directory** | client | `FileOffer` → `FileAccept` → `Data`… → `FileEnd` → `FileDone`, with `Ack`s flowing back |
//!
//! Per-file streams are what QUIC buys us: a lost packet on one file never
//! stalls another (no head-of-line blocking), many small files fly in parallel
//! over one handshake, and a file can be abandoned with a stream reset without
//! tearing the session down.
//!
//! # Framing
//!
//! ```text
//!  0      4      5                                     len+4
//!  +------+------+--------------------------------------+
//!  | len  | kind | payload                              |   len: u32 LE = 1 + payload length
//!  +------+------+--------------------------------------+
//!
//!  kind 0x01  MSG   payload = postcard(Message)       compact, versioned control messages
//!  kind 0x02  DATA  payload = offset: u64 LE ‖ bytes  file bytes, no serde copy
//! ```
//!
//! Frames are capped at [`MAX_FRAME`]; a peer sending more is disconnected.
//! `DATA` offsets must be contiguous: the receiver rejects gaps or overlaps,
//! so a confused or malicious sender can't corrupt the middle of a file.
//!
//! # Message flow
//!
//! ```text
//! client                                    server
//!   │── Hello{protocol, name} ───────────────▶│   (control stream)
//!   │◀────────────── HelloAck{protocol, name}─│
//!   │── AuthStart{user, client_nonce} ───────▶│
//!   │◀──── AuthChallenge{salt, kdf, s_nonce} ─│
//!   │── AuthProof{proof} ────────────────────▶│   SCRAM, bound to the TLS session
//!   │◀──── AuthOk{server_sig, token, shares} ─│   (or AuthFailed{retry_after})
//!   │                                          │
//!   │══ new stream ═══════════════════════════│
//!   │── FileOffer{path, size, id, …} ────────▶│
//!   │◀─────────────── FileAccept{resume_from} ─│   resume_from > 0 after a dropout
//!   │── DATA(offset, bytes) … ───────────────▶│
//!   │◀──────────────────────── Ack{offset} … ──│   bytes durably written
//!   │── FileEnd{size, digest} ───────────────▶│
//!   │◀──────────── FileDone{ok, digest, msg} ──│   server re-read the file from disk
//! ```
//!
//! On reconnect the client sends `AuthResume{token}` instead of the SCRAM
//! exchange and re-offers the interrupted file; the server answers with the
//! offset it already holds.

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Bumped on any incompatible change. Peers must match exactly.
pub const PROTOCOL_VERSION: u16 = 1;
/// TLS ALPN identifier.
pub const ALPN: &[u8] = b"fcopy/1";
/// Default UDP port.
pub const DEFAULT_PORT: u16 = 47_800;
/// Data payload per frame.
pub const CHUNK: usize = 1024 * 1024;
/// Largest frame accepted (1 MiB data + header + slack for messages).
pub const MAX_FRAME: usize = CHUNK + 64 * 1024;

const KIND_MSG: u8 = 0x01;
const KIND_DATA: u8 = 0x02;

/// Argon2id parameters, sent to the client so it can derive the same key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct KdfParams {
    /// Memory in KiB.
    pub m_cost: u32,
    pub t_cost: u32,
    pub p_cost: u32,
}

impl Default for KdfParams {
    /// OWASP 2024 recommendation for Argon2id: 19 MiB, 2 passes, 1 lane.
    fn default() -> Self {
        Self { m_cost: 19 * 1024, t_cost: 2, p_cost: 1 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShareInfo {
    pub name: String,
    pub writable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EntryKind {
    File,
    Dir,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HashAlgo {
    Xxh3,
    Sha256,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RejectReason {
    /// Path escapes the share, has illegal components, or names an unknown share.
    BadPath,
    /// Account may not write here, or the share is read-only.
    Denied,
    /// Destination exists and the offer did not ask to overwrite.
    Exists,
    /// Not enough free space for the file.
    NoSpace,
    /// Server-side I/O error.
    Io,
}

/// Control and per-file messages (`kind 0x01` frames).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Message {
    // ----- handshake (control stream) -----
    Hello { protocol: u16, name: String },
    HelloAck { protocol: u16, name: String },
    AuthStart { user: String, client_nonce: [u8; 32] },
    AuthChallenge { salt: Vec<u8>, kdf: KdfParams, server_nonce: [u8; 32] },
    AuthProof { proof: [u8; 32] },
    /// Fast re-authentication after a dropout, using the token from `AuthOk`.
    AuthResume { token: [u8; 32] },
    AuthOk { server_signature: [u8; 32], token: [u8; 32], token_ttl_secs: u64, shares: Vec<ShareInfo> },
    AuthFailed { reason: String, retry_after_ms: u64 },

    // ----- session control (either direction) -----
    Pause,
    Resume,
    Ping { nonce: u64 },
    Pong { nonce: u64 },
    Bye,

    // ----- per-entry stream -----
    FileOffer {
        kind: EntryKind,
        /// `share/relative/path`, `/`-separated, validated by the server.
        path: String,
        size: u64,
        /// Seconds + nanoseconds since the Unix epoch.
        mtime: (i64, u32),
        /// Unix `rwx` bits (0 if unknown, e.g. from Windows).
        mode: u32,
        algo: HashAlgo,
        /// Identifies *this version of this source file* so a partial upload
        /// is only resumed for the same data (hash of path, size, mtime).
        source_id: [u8; 16],
        overwrite: bool,
    },
    FileAccept { resume_from: u64 },
    FileReject { reason: RejectReason, message: String },
    Ack { offset: u64 },
    FileEnd { size: u64, digest: Vec<u8> },
    FileDone { ok: bool, digest: Vec<u8>, message: String },
}

/// One decoded frame. `Data` borrows from the caller's buffer.
#[derive(Debug, PartialEq, Eq)]
pub enum Frame<'a> {
    Msg(Message),
    Data { offset: u64, bytes: &'a [u8] },
}

#[derive(Debug, thiserror::Error)]
pub enum ProtoError {
    #[error("connection I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("frame of {0} bytes exceeds the {MAX_FRAME}-byte limit")]
    TooLarge(usize),
    #[error("malformed frame: {0}")]
    Malformed(String),
    #[error("unexpected message: expected {expected}, got {got:?}")]
    Unexpected { expected: &'static str, got: Box<Message> },
    #[error("peer closed the stream")]
    Closed,
}

/// Write one control message.
pub async fn write_msg<W: AsyncWrite + Unpin>(w: &mut W, msg: &Message) -> Result<(), ProtoError> {
    let body = postcard::to_allocvec(msg).map_err(|e| ProtoError::Malformed(e.to_string()))?;
    let len = u32::try_from(body.len() + 1).map_err(|_| ProtoError::TooLarge(body.len()))?;
    let mut head = [0u8; 5];
    head[..4].copy_from_slice(&len.to_le_bytes());
    head[4] = KIND_MSG;
    w.write_all(&head).await?;
    w.write_all(&body).await?;
    Ok(())
}

/// Write one data frame without copying the payload.
pub async fn write_data<W: AsyncWrite + Unpin>(w: &mut W, offset: u64, bytes: &[u8]) -> Result<(), ProtoError> {
    if bytes.len() > CHUNK {
        return Err(ProtoError::TooLarge(bytes.len()));
    }
    let len = (1 + 8 + bytes.len()) as u32;
    let mut head = [0u8; 13];
    head[..4].copy_from_slice(&len.to_le_bytes());
    head[4] = KIND_DATA;
    head[5..].copy_from_slice(&offset.to_le_bytes());
    w.write_all(&head).await?;
    w.write_all(bytes).await?;
    Ok(())
}

/// Read one frame into `buf` (reused across calls). `Ok(None)` = clean end of stream.
pub async fn read_frame<'b, R: AsyncRead + Unpin>(r: &mut R, buf: &'b mut Vec<u8>) -> Result<Option<Frame<'b>>, ProtoError> {
    let mut len_bytes = [0u8; 4];
    // Distinguish "stream ended between frames" (clean) from "ended mid-frame" (error).
    let mut got = 0;
    while got < 4 {
        match r.read(&mut len_bytes[got..]).await? {
            0 if got == 0 => return Ok(None),
            0 => return Err(ProtoError::Malformed("truncated frame header".into())),
            n => got += n,
        }
    }
    let len = u32::from_le_bytes(len_bytes) as usize;
    if len == 0 {
        return Err(ProtoError::Malformed("empty frame".into()));
    }
    if len > MAX_FRAME {
        return Err(ProtoError::TooLarge(len));
    }
    buf.resize(len, 0);
    r.read_exact(buf).await?;
    match buf[0] {
        KIND_MSG => {
            let msg = postcard::from_bytes(&buf[1..]).map_err(|e| ProtoError::Malformed(format!("bad message: {e}")))?;
            Ok(Some(Frame::Msg(msg)))
        }
        KIND_DATA => {
            if len < 9 {
                return Err(ProtoError::Malformed("short data frame".into()));
            }
            let offset = u64::from_le_bytes(buf[1..9].try_into().expect("8 bytes"));
            Ok(Some(Frame::Data { offset, bytes: &buf[9..] }))
        }
        k => Err(ProtoError::Malformed(format!("unknown frame kind {k:#04x}"))),
    }
}

/// Read a frame that must be a control message.
pub async fn read_msg<R: AsyncRead + Unpin>(r: &mut R, buf: &mut Vec<u8>) -> Result<Message, ProtoError> {
    match read_frame(r, buf).await? {
        Some(Frame::Msg(m)) => Ok(m),
        Some(Frame::Data { .. }) => Err(ProtoError::Malformed("data frame where a message was expected".into())),
        None => Err(ProtoError::Closed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn messages_and_data_round_trip() {
        let (mut a, mut b) = tokio::io::duplex(8 * 1024 * 1024);
        let offer = Message::FileOffer {
            kind: EntryKind::File,
            path: "backup/photos/a.jpg".into(),
            size: 123,
            mtime: (1_700_000_000, 5),
            mode: 0o644,
            algo: HashAlgo::Xxh3,
            source_id: [7; 16],
            overwrite: false,
        };
        write_msg(&mut a, &offer).await.unwrap();
        let payload: Vec<u8> = (0..CHUNK).map(|i| i as u8).collect();
        write_data(&mut a, 42, &payload).await.unwrap();
        write_msg(&mut a, &Message::Bye).await.unwrap();
        drop(a);

        let mut buf = Vec::new();
        assert_eq!(read_frame(&mut b, &mut buf).await.unwrap(), Some(Frame::Msg(offer)));
        match read_frame(&mut b, &mut buf).await.unwrap() {
            Some(Frame::Data { offset, bytes }) => {
                assert_eq!(offset, 42);
                assert_eq!(bytes, &payload[..]);
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(read_msg(&mut b, &mut buf).await.unwrap(), Message::Bye);
        assert!(read_frame(&mut b, &mut buf).await.unwrap().is_none(), "clean EOF");
    }

    #[tokio::test]
    async fn oversized_and_garbage_frames_are_rejected() {
        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&(MAX_FRAME as u32 + 1).to_le_bytes()).await.unwrap();
        let mut buf = Vec::new();
        assert!(matches!(read_frame(&mut b, &mut buf).await, Err(ProtoError::TooLarge(_))));

        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&[3, 0, 0, 0, 0x7f, 1, 2]).await.unwrap();
        assert!(matches!(read_frame(&mut b, &mut buf).await, Err(ProtoError::Malformed(_))));

        let (mut a, mut b) = tokio::io::duplex(64);
        a.write_all(&[9, 0]).await.unwrap(); // truncated header
        drop(a);
        assert!(matches!(read_frame(&mut b, &mut buf).await, Err(ProtoError::Malformed(_))));
    }

    #[tokio::test]
    async fn data_frames_refuse_oversized_payloads() {
        let (mut a, _b) = tokio::io::duplex(64);
        let big = vec![0u8; CHUNK + 1];
        assert!(matches!(write_data(&mut a, 0, &big).await, Err(ProtoError::TooLarge(_))));
    }
}

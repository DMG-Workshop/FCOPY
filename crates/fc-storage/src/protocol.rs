//! Wire format between the desktop app and the privileged worker.
//!
//! The app seals a plan with an HMAC-SHA256 under a key generated for that one
//! operation. The key travels to the worker over the same pipe, never on the
//! command line, so it is not visible in process listings. The worker verifies
//! the MAC in constant time before it parses anything, then checks expiry.
//!
//! Each operation runs in its own worker process, so a nonce only has to be
//! unique per operation. Replaying an old envelope fails, because its MAC was
//! made with a key that no longer exists.

use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use crate::orchestrate::{Outcome, Request};
use crate::wipe::Progress;

type HmacSha256 = Hmac<Sha256>;

/// Length of the per-operation key and of the MAC.
pub const KEY_LEN: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ProtocolError {
    #[error("the plan signature does not verify")]
    BadSignature,
    #[error("the plan has expired")]
    Expired,
    #[error("the plan body is malformed")]
    Malformed,
}

/// What the MAC covers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanBody {
    pub expires_at_unix: u64,
    /// Hex, 16 random bytes, unique per operation.
    pub nonce: String,
    pub request: Request,
}

/// A sealed plan. `body` is the exact JSON that was signed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Envelope {
    pub body: Vec<u8>,
    pub mac: [u8; KEY_LEN],
}

fn mac_over(key: &[u8; KEY_LEN], bytes: &[u8]) -> [u8; KEY_LEN] {
    // HMAC accepts keys of any length, so this cannot fail for a 32-byte key.
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(bytes);
    mac.finalize().into_bytes().into()
}

pub fn seal(key: &[u8; KEY_LEN], body: &PlanBody) -> Result<Envelope, ProtocolError> {
    let bytes = serde_json::to_vec(body).map_err(|_| ProtocolError::Malformed)?;
    let mac = mac_over(key, &bytes);
    Ok(Envelope { body: bytes, mac })
}

/// Verifies the MAC, then parses, then checks expiry. The MAC comparison is constant time.
pub fn open(key: &[u8; KEY_LEN], env: &Envelope, now_unix: u64) -> Result<PlanBody, ProtocolError> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(&env.body);
    mac.verify_slice(&env.mac)
        .map_err(|_| ProtocolError::BadSignature)?;
    let body: PlanBody = serde_json::from_slice(&env.body).map_err(|_| ProtocolError::Malformed)?;
    if now_unix >= body.expires_at_unix {
        return Err(ProtocolError::Expired);
    }
    Ok(body)
}

/// A fresh per-operation key from the operating system's random source.
pub fn new_key() -> Result<[u8; KEY_LEN], getrandom::Error> {
    let mut key = [0u8; KEY_LEN];
    getrandom::fill(&mut key)?;
    Ok(key)
}

/// A fresh nonce, as lowercase hex.
pub fn new_nonce() -> Result<String, getrandom::Error> {
    let mut raw = [0u8; 16];
    getrandom::fill(&mut raw)?;
    Ok(raw.iter().map(|b| format!("{b:02x}")).collect())
}

/// Events the worker streams back, one JSON object per line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum WorkerEvent {
    Progress(Progress),
    Finished(Outcome),
}

pub fn encode_event(event: &WorkerEvent) -> Result<String, ProtocolError> {
    serde_json::to_string(event).map_err(|_| ProtocolError::Malformed)
}

pub fn decode_event(line: &str) -> Result<WorkerEvent, ProtocolError> {
    serde_json::from_str(line.trim_end()).map_err(|_| ProtocolError::Malformed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{DiskId, DiskIdentity};
    use crate::orchestrate::WipeScope;
    use crate::safety::Policy;
    use crate::wipe::{Phase, WipeMethod};

    const KEY: [u8; KEY_LEN] = [9u8; KEY_LEN];

    fn body(expires: u64) -> PlanBody {
        PlanBody {
            expires_at_unix: expires,
            nonce: "00112233445566778899aabbccddeeff".into(),
            request: Request {
                disk: DiskId::derive(
                    crate::model::Transport::Usb,
                    &DiskIdentity {
                        serial: Some("SN-1".into()),
                        model: "Drive".into(),
                        size_bytes: 1 << 30,
                    },
                ),
                confirmed: DiskIdentity {
                    serial: Some("SN-1".into()),
                    model: "Drive".into(),
                    size_bytes: 1 << 30,
                },
                typed_serial: "SN-1".into(),
                policy: Policy::default(),
                protected_paths: Vec::new(),
                scope: WipeScope::WholeDevice,
                method: WipeMethod::Clear,
                table: None,
            },
        }
    }

    #[test]
    fn sealed_plan_opens_with_the_same_key() {
        let env = seal(&KEY, &body(2_000)).unwrap();
        assert_eq!(open(&KEY, &env, 1_000).unwrap(), body(2_000));
    }

    #[test]
    fn wrong_key_is_rejected() {
        let env = seal(&KEY, &body(2_000)).unwrap();
        assert_eq!(
            open(&[1u8; KEY_LEN], &env, 1_000),
            Err(ProtocolError::BadSignature)
        );
    }

    #[test]
    fn a_single_changed_byte_is_rejected() {
        let mut env = seal(&KEY, &body(2_000)).unwrap();
        let i = env
            .body
            .windows(4)
            .position(|w| w == b"SN-1")
            .expect("serial is in the body");
        env.body[i + 3] = b'2';
        assert_eq!(open(&KEY, &env, 1_000), Err(ProtocolError::BadSignature));
    }

    #[test]
    fn an_expired_plan_is_rejected_even_when_signed() {
        let env = seal(&KEY, &body(2_000)).unwrap();
        assert_eq!(open(&KEY, &env, 2_000), Err(ProtocolError::Expired));
        assert_eq!(open(&KEY, &env, 3_000), Err(ProtocolError::Expired));
    }

    #[test]
    fn a_validly_signed_but_malformed_body_is_rejected() {
        let bytes = b"not json".to_vec();
        let env = Envelope {
            mac: mac_over(&KEY, &bytes),
            body: bytes,
        };
        assert_eq!(open(&KEY, &env, 1_000), Err(ProtocolError::Malformed));
    }

    #[test]
    fn nonces_are_hex_and_differ() {
        let a = new_nonce().unwrap();
        let b = new_nonce().unwrap();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn worker_events_round_trip_as_one_line() {
        let events = [
            WorkerEvent::Progress(Progress {
                pass: 1,
                passes: 1,
                done: 512,
                total: 1024,
                phase: Phase::Write,
            }),
            WorkerEvent::Finished(Outcome::Completed),
            WorkerEvent::Finished(Outcome::Failed("format failed".into())),
        ];
        for e in events {
            let line = encode_event(&e).unwrap();
            assert!(!line.contains('\n'));
            assert_eq!(decode_event(&line).unwrap(), e);
        }
    }
}

//! Password authentication: SCRAM (RFC 5802/7677) with **Argon2id** as the
//! key-derivation function and **TLS channel binding**.
//!
//! # Why this construction
//!
//! * **The password never crosses the wire**, not even inside TLS. The
//!   client proves knowledge of a key derived from it.
//! * **The server never stores the password or a password-equivalent.** It
//!   stores `StoredKey = SHA-256(ClientKey)` and `ServerKey`. A stolen config
//!   file does not let an attacker log in; it only allows an *offline* guessing
//!   attack, and every guess costs a full Argon2id evaluation (19 MiB, 2 passes).
//! * **Mutual authentication.** The server must prove it holds `ServerKey`
//!   (`server_signature`), so a fake server can't pretend the login succeeded.
//! * **Channel binding.** The signed transcript includes a value exported from
//!   *this* TLS session (RFC 5705 exporter). A man-in-the-middle with its own
//!   TLS session can't relay the proof to the real server.
//!
//! ```text
//! SaltedPassword = Argon2id(password, salt, m, t, p)        (client side only)
//! ClientKey      = HMAC(SaltedPassword, "Client Key")
//! ServerKey      = HMAC(SaltedPassword, "Server Key")
//! StoredKey      = SHA-256(ClientKey)                        ← server stores StoredKey, ServerKey, salt
//! AuthMessage    = lp(user) ‖ lp(c_nonce) ‖ lp(s_nonce) ‖ lp(salt) ‖ kdf ‖ lp(tls_exporter)
//! ClientProof    = ClientKey ⊕ HMAC(StoredKey, AuthMessage)
//! server checks  SHA-256(ClientProof ⊕ HMAC(StoredKey, AuthMessage)) == StoredKey
//! ServerSig      = HMAC(ServerKey, AuthMessage)              ← client checks this
//! ```
//! (`lp` = 4-byte length prefix, so field boundaries can't be shifted.)
//!
//! Unknown users get a deterministic fake salt derived from a server secret,
//! so the challenge looks identical and usernames can't be enumerated.

use argon2::{Argon2, Params};
use base64::Engine as _;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::protocol::KdfParams;

type HmacSha256 = Hmac<Sha256>;

/// TLS exporter label for channel binding.
pub const EXPORTER_LABEL: &[u8] = b"EXPORTER-fcopy-channel-binding-v1";

#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    #[error("invalid KDF parameters: {0}")]
    Kdf(String),
    #[error("system random generator failed")]
    Random,
}

/// Cryptographically secure random bytes (from the TLS provider's CSPRNG).
pub fn random<const N: usize>() -> [u8; N] {
    let mut out = [0u8; N];
    rustls::crypto::ring::default_provider()
        .secure_random
        .fill(&mut out)
        .expect("OS random number generator unavailable");
    out
}

fn hmac(key: &[u8], data: &[u8]) -> [u8; 32] {
    let mut mac = <HmacSha256 as KeyInit>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    let mut out = [0u8; 32];
    out.copy_from_slice(&mac.finalize().into_bytes());
    out
}

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&Sha256::digest(data));
    out
}

fn xor(a: &[u8; 32], b: &[u8; 32]) -> [u8; 32] {
    std::array::from_fn(|i| a[i] ^ b[i])
}

/// `SHA256:<base64>`, the format OpenSSH uses for host keys.
pub fn fingerprint(cert_der: &[u8]) -> String {
    format!("SHA256:{}", base64::engine::general_purpose::STANDARD_NO_PAD.encode(sha256(cert_der)))
}

/// Keys the *client* derives from the password. Never sent anywhere.
#[derive(Clone)]
pub struct ClientKeys {
    client_key: [u8; 32],
    server_key: [u8; 32],
}

impl std::fmt::Debug for ClientKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ClientKeys(<redacted>)")
    }
}

impl ClientKeys {
    /// Run Argon2id. Deliberately slow (~tens of ms).
    pub fn derive(password: &str, salt: &[u8], kdf: KdfParams) -> Result<Self, CryptoError> {
        let params = Params::new(kdf.m_cost, kdf.t_cost, kdf.p_cost, Some(32)).map_err(|e| CryptoError::Kdf(e.to_string()))?;
        let argon = Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
        let mut salted = [0u8; 32];
        argon
            .hash_password_into(password.as_bytes(), salt, &mut salted)
            .map_err(|e| CryptoError::Kdf(e.to_string()))?;
        let keys = Self { client_key: hmac(&salted, b"Client Key"), server_key: hmac(&salted, b"Server Key") };
        salted.fill(0);
        Ok(keys)
    }

    fn stored_key(&self) -> [u8; 32] {
        sha256(&self.client_key)
    }

    /// The proof sent in `AuthProof`.
    pub fn proof(&self, auth_message: &[u8]) -> [u8; 32] {
        xor(&self.client_key, &hmac(&self.stored_key(), auth_message))
    }

    /// Check the server's signature from `AuthOk` (mutual authentication).
    pub fn verify_server(&self, auth_message: &[u8], signature: &[u8; 32]) -> bool {
        hmac(&self.server_key, auth_message).ct_eq(signature).into()
    }
}

/// What the server stores per account. Contains no password-equivalent.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verifier {
    #[serde(with = "b64")]
    pub salt: Vec<u8>,
    pub kdf: KdfParams,
    #[serde(with = "b64_32")]
    pub stored_key: [u8; 32],
    #[serde(with = "b64_32")]
    pub server_key: [u8; 32],
}

impl std::fmt::Debug for Verifier {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Verifier").field("kdf", &self.kdf).finish_non_exhaustive()
    }
}

impl Verifier {
    /// Create a verifier for a new password, with a fresh random salt.
    pub fn new(password: &str, kdf: KdfParams) -> Result<Self, CryptoError> {
        let salt = random::<16>().to_vec();
        let keys = ClientKeys::derive(password, &salt, kdf)?;
        Ok(Self { stored_key: keys.stored_key(), server_key: keys.server_key, salt, kdf })
    }

    /// A verifier that can never succeed, for unknown users. The salt is a
    /// stable function of the username so repeated probes see the same challenge.
    pub fn decoy(server_secret: &[u8; 32], user: &str) -> Self {
        let salt = hmac(server_secret, format!("decoy-salt:{user}").as_bytes())[..16].to_vec();
        Self { salt, kdf: KdfParams::default(), stored_key: random(), server_key: random() }
    }

    /// Constant-time check of a client proof.
    pub fn check_proof(&self, auth_message: &[u8], proof: &[u8; 32]) -> bool {
        let client_key = xor(proof, &hmac(&self.stored_key, auth_message));
        sha256(&client_key).ct_eq(&self.stored_key).into()
    }

    pub fn server_signature(&self, auth_message: &[u8]) -> [u8; 32] {
        hmac(&self.server_key, auth_message)
    }
}

/// The transcript both sides sign. Every field is length-prefixed.
pub fn auth_message(
    user: &str,
    client_nonce: &[u8; 32],
    server_nonce: &[u8; 32],
    salt: &[u8],
    kdf: KdfParams,
    channel_binding: &[u8; 32],
) -> Vec<u8> {
    let mut m = Vec::with_capacity(256);
    let mut lp = |b: &[u8]| {
        m.extend_from_slice(&(b.len() as u32).to_le_bytes());
        m.extend_from_slice(b);
    };
    lp(b"fcopy-scram-argon2id-v1");
    lp(user.as_bytes());
    lp(client_nonce);
    lp(server_nonce);
    lp(salt);
    lp(&kdf.m_cost.to_le_bytes());
    lp(&kdf.t_cost.to_le_bytes());
    lp(&kdf.p_cost.to_le_bytes());
    lp(channel_binding);
    m
}

/// Generate a random unattended-access key (shown to the admin once).
pub fn generate_access_key() -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random::<24>())
}

mod b64 {
    use base64::Engine as _;
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(v))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        base64::engine::general_purpose::STANDARD.decode(s).map_err(serde::de::Error::custom)
    }
}

mod b64_32 {
    use serde::{Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        super::b64::serialize(v, s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let v = super::b64::deserialize(d)?;
        v.try_into().map_err(|_| serde::de::Error::custom("expected 32 bytes"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cheap parameters so tests run fast; production uses `KdfParams::default()`.
    pub const TEST_KDF: KdfParams = KdfParams { m_cost: 256, t_cost: 1, p_cost: 1 };

    fn transcript(v: &Verifier, cb: &[u8; 32]) -> Vec<u8> {
        auth_message("alice", &[1; 32], &[2; 32], &v.salt, v.kdf, cb)
    }

    #[test]
    fn correct_password_authenticates_both_ways() {
        let v = Verifier::new("correct horse battery staple", TEST_KDF).unwrap();
        let keys = ClientKeys::derive("correct horse battery staple", &v.salt, v.kdf).unwrap();
        let msg = transcript(&v, &[9; 32]);
        assert!(v.check_proof(&msg, &keys.proof(&msg)));
        assert!(keys.verify_server(&msg, &v.server_signature(&msg)));
    }

    #[test]
    fn wrong_password_fails() {
        let v = Verifier::new("right", TEST_KDF).unwrap();
        let keys = ClientKeys::derive("wrong", &v.salt, v.kdf).unwrap();
        let msg = transcript(&v, &[9; 32]);
        assert!(!v.check_proof(&msg, &keys.proof(&msg)));
    }

    #[test]
    fn proof_is_bound_to_the_tls_session() {
        let v = Verifier::new("pw", TEST_KDF).unwrap();
        let keys = ClientKeys::derive("pw", &v.salt, v.kdf).unwrap();
        // A MITM relays a proof made over *its* TLS session to the real server.
        let mitm_session = transcript(&v, &[1; 32]);
        let real_session = transcript(&v, &[2; 32]);
        assert!(!v.check_proof(&real_session, &keys.proof(&mitm_session)));
    }

    #[test]
    fn fake_server_cannot_forge_success() {
        let real = Verifier::new("pw", TEST_KDF).unwrap();
        let keys = ClientKeys::derive("pw", &real.salt, real.kdf).unwrap();
        let msg = transcript(&real, &[3; 32]);
        let fake = Verifier { server_key: random(), ..real.clone() };
        assert!(!keys.verify_server(&msg, &fake.server_signature(&msg)));
    }

    #[test]
    fn stored_verifier_is_not_a_login_credential() {
        // Knowing stored_key/server_key (a leaked config) is not enough to make a proof:
        // the proof needs ClientKey, and only its hash is stored.
        let v = Verifier::new("pw", TEST_KDF).unwrap();
        let msg = transcript(&v, &[4; 32]);
        let forged = xor(&v.stored_key, &hmac(&v.stored_key, &msg));
        assert!(!v.check_proof(&msg, &forged));
    }

    #[test]
    fn decoys_are_stable_per_user_and_never_verify() {
        let secret = [5u8; 32];
        let a1 = Verifier::decoy(&secret, "mallory");
        let a2 = Verifier::decoy(&secret, "mallory");
        let b = Verifier::decoy(&secret, "eve");
        assert_eq!(a1.salt, a2.salt);
        assert_ne!(a1.salt, b.salt);
        let keys = ClientKeys::derive("anything", &a1.salt, TEST_KDF).unwrap();
        let msg = transcript(&a1, &[0; 32]);
        assert!(!a1.check_proof(&msg, &keys.proof(&msg)));
    }

    #[test]
    fn transcript_fields_cannot_be_shifted() {
        let a = auth_message("ab", &[0; 32], &[0; 32], b"c", KdfParams::default(), &[0; 32]);
        let b = auth_message("a", &[0; 32], &[0; 32], b"bc", KdfParams::default(), &[0; 32]);
        assert_ne!(a, b);
    }

    #[test]
    fn verifier_serializes_without_secrets_in_debug() {
        let v = Verifier::new("pw", TEST_KDF).unwrap();
        let toml = toml::to_string(&v).unwrap();
        let back: Verifier = toml::from_str(&toml).unwrap();
        assert_eq!(back, v);
        assert!(!format!("{v:?}").contains(&base64::engine::general_purpose::STANDARD.encode(v.stored_key)));
        assert!(fingerprint(b"cert").starts_with("SHA256:"));
        assert_eq!(generate_access_key().len(), 32);
    }
}

#[cfg(test)]
pub(crate) use tests::TEST_KDF;

//! Web accounts, sessions and API keys.
//!
//! * **Passwords** are stored as `$fc-argon2id$v1$m$t$p$salt$hash` (Argon2id,
//!   base64 without padding). Login always runs one Argon2 evaluation, even for
//!   an unknown user, so response time doesn't reveal which accounts exist.
//! * **Sessions** are random 256-bit tokens in an `HttpOnly` cookie. The server
//!   keeps only `SHA-256(token)`, expires entries, and logout deletes them.
//!   Opaque tokens rather than JWTs: a single server can revoke them instantly.
//! * **API keys** are for unattended scripts (`Authorization: Bearer …`). Like
//!   sessions, only the SHA-256 is stored. Keys are high-entropy, so a slow hash
//!   would add nothing.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::RwLock;
use std::time::{Duration, Instant};

use argon2::{Algorithm, Argon2, Params, Version};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use fc_remote::crypto::random;

/// Cookie that carries the session token.
pub const SESSION_COOKIE: &str = "fc_session";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Manages accounts, pairing and settings, and may act on any job.
    Admin,
    /// Sees and manages only the jobs it created.
    User,
}

/// Who a request is acting as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub name: String,
    pub role: Role,
}

impl Principal {
    pub fn is_admin(&self) -> bool {
        self.role == Role::Admin
    }
}

/// Argon2id cost. Production uses [`Params::default`]-equivalent values; tests
/// pass something cheap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HashCost {
    pub m_kib: u32,
    pub t: u32,
    pub p: u32,
}

impl Default for HashCost {
    /// OWASP 2024 Argon2id guidance: 19 MiB, 2 passes, 1 lane.
    fn default() -> Self {
        Self { m_kib: 19 * 1024, t: 2, p: 1 }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("invalid account name")]
    BadName,
    #[error("password must be at least 8 characters")]
    WeakPassword,
    #[error("account already exists")]
    Exists,
    #[error("password hashing failed: {0}")]
    Hash(String),
    #[error("account storage failed: {0}")]
    Storage(#[from] std::io::Error),
}

fn derive(password: &[u8], salt: &[u8], cost: HashCost) -> Result<[u8; 32], AuthError> {
    let params = Params::new(cost.m_kib, cost.t, cost.p, Some(32)).map_err(|e| AuthError::Hash(e.to_string()))?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);
    let mut out = [0u8; 32];
    argon.hash_password_into(password, salt, &mut out).map_err(|e| AuthError::Hash(e.to_string()))?;
    Ok(out)
}

/// Encode a new password hash.
pub fn hash_password(password: &str, cost: HashCost) -> Result<String, AuthError> {
    let salt = random::<16>();
    let hash = derive(password.as_bytes(), &salt, cost)?;
    Ok(format!(
        "$fc-argon2id$v1${}${}${}${}${}",
        cost.m_kib,
        cost.t,
        cost.p,
        STANDARD_NO_PAD.encode(salt),
        STANDARD_NO_PAD.encode(hash)
    ))
}

fn decode_hash(stored: &str) -> Option<(HashCost, Vec<u8>, Vec<u8>)> {
    let parts: Vec<&str> = stored.split('$').collect();
    let ["", scheme, version, m, t, p, salt, hash] = parts[..] else { return None };
    if scheme != "fc-argon2id" || version != "v1" {
        return None;
    }
    let (Ok(m), Ok(t), Ok(p)) = (m.parse::<u32>(), t.parse::<u32>(), p.parse::<u32>()) else { return None };
    if !(1..=8).contains(&p) || !(1..=10).contains(&t) || !(8 * p..=1024 * 1024).contains(&m) {
        return None;
    }
    let (Ok(salt), Ok(expected)) = (STANDARD_NO_PAD.decode(salt), STANDARD_NO_PAD.decode(hash)) else { return None };
    if salt.len() != 16 || expected.len() != 32 { return None; }
    Some((HashCost { m_kib: m, t, p }, salt, expected))
}

/// Check a password against a stored hash. Malformed hashes never match.
pub fn verify_password(password: &str, stored: &str) -> bool {
    let Some((cost, salt, expected)) = decode_hash(stored) else { return false; };
    match derive(password.as_bytes(), &salt, cost) {
        Ok(got) => got.ct_eq(expected.as_slice()).into(),
        Err(_) => false,
    }
}

fn sha256(data: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&Sha256::digest(data));
    out
}

/// A stored account.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
    pub name: String,
    pub role: Role,
    hash: String,
}

/// All web accounts, keyed by name.
pub struct UserStore {
    accounts: RwLock<HashMap<String, Account>>,
    cost: HashCost,
    /// Verified against when the user is unknown, so timing is the same.
    decoy: String,
    path: Option<PathBuf>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountsFile {
    version: u8,
    accounts: HashMap<String, Account>,
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

impl UserStore {
    pub fn new(cost: HashCost) -> Result<Self, AuthError> {
        Self::open(cost, None)
    }

    pub fn open(cost: HashCost, path: Option<PathBuf>) -> Result<Self, AuthError> {
        let decoy = hash_password("decoy-password", cost)?;
        let accounts = if let Some(path) = &path {
            match std::fs::read(path) {
                Ok(bytes) => {
                    let file: AccountsFile = serde_json::from_slice(&bytes)
                        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
                    if file.version != 1 || file.accounts.iter().any(|(name, account)| {
                        !valid_name(name) || name != &account.name || decode_hash(&account.hash).is_none()
                    }) {
                        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "invalid saved account data").into());
                    }
                    file.accounts
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
                Err(error) => return Err(error.into()),
            }
        } else { HashMap::new() };
        Ok(Self { accounts: RwLock::new(accounts), cost, decoy, path })
    }

    fn save(&self, accounts: &HashMap<String, Account>) -> Result<(), AuthError> {
        if let Some(path) = &self.path {
            if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
            let bytes = serde_json::to_vec_pretty(&AccountsFile { version: 1, accounts: accounts.clone() })
                .map_err(std::io::Error::other)?;
            fc_remote::config::write_private(path, &bytes)?;
        }
        Ok(())
    }

    pub fn add(&self, name: &str, password: &str, role: Role) -> Result<(), AuthError> {
        if !valid_name(name) {
            return Err(AuthError::BadName);
        }
        if password.chars().count() < 8 {
            return Err(AuthError::WeakPassword);
        }
        let hash = hash_password(password, self.cost)?;
        let mut accounts = self.accounts.write().unwrap_or_else(|e| e.into_inner());
        if accounts.contains_key(name) {
            return Err(AuthError::Exists);
        }
        let mut next = accounts.clone();
        next.insert(name.to_string(), Account { name: name.to_string(), role, hash });
        self.save(&next)?;
        *accounts = next;
        Ok(())
    }

    /// Returns the account if `password` is correct. Always does one Argon2
    /// evaluation, so an unknown name costs the same as a wrong password.
    pub fn authenticate(&self, name: &str, password: &str) -> Option<Account> {
        let account = self.accounts.read().unwrap_or_else(|e| e.into_inner()).get(name).cloned();
        let stored = account.as_ref().map(|a| a.hash.as_str()).unwrap_or(&self.decoy);
        let ok = verify_password(password, stored);
        account.filter(|_| ok)
    }

    /// All accounts, sorted by name, without password hashes.
    pub fn list(&self) -> Vec<(String, Role)> {
        let mut out: Vec<(String, Role)> = self
            .accounts
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .map(|a| (a.name.clone(), a.role))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    pub fn remove(&self, name: &str) -> Result<bool, AuthError> {
        let mut accounts = self.accounts.write().unwrap_or_else(|e| e.into_inner());
        let mut next = accounts.clone();
        if next.remove(name).is_none() { return Ok(false); }
        self.save(&next)?;
        *accounts = next;
        Ok(true)
    }

    pub fn len(&self) -> usize {
        self.accounts.read().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

struct SessionEntry {
    principal: Principal,
    expires: Instant,
}

/// Server-side login sessions.
pub struct SessionStore {
    sessions: RwLock<HashMap<[u8; 32], SessionEntry>>,
    ttl: Duration,
}

impl SessionStore {
    pub fn new(ttl: Duration) -> Self {
        Self { sessions: RwLock::new(HashMap::new()), ttl }
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }

    /// Start a session; returns the token to put in the cookie.
    pub fn issue(&self, principal: Principal) -> String {
        let token = random::<32>();
        let mut sessions = self.sessions.write().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        sessions.retain(|_, s| s.expires > now);
        sessions.insert(sha256(&token), SessionEntry { principal, expires: now + self.ttl });
        URL_SAFE_NO_PAD.encode(token)
    }

    pub fn lookup(&self, token: &str) -> Option<Principal> {
        let raw = URL_SAFE_NO_PAD.decode(token).ok()?;
        let key = sha256(&raw);
        let sessions = self.sessions.read().unwrap_or_else(|e| e.into_inner());
        let entry = sessions.get(&key)?;
        (entry.expires > Instant::now()).then(|| entry.principal.clone())
    }

    /// End every session of `name` (used when an account is deleted).
    pub fn revoke_user(&self, name: &str) {
        self.sessions.write().unwrap_or_else(|e| e.into_inner()).retain(|_, s| s.principal.name != name);
    }

    pub fn revoke(&self, token: &str) {
        if let Ok(raw) = URL_SAFE_NO_PAD.decode(token) {
            self.sessions.write().unwrap_or_else(|e| e.into_inner()).remove(&sha256(&raw));
        }
    }
}

/// Long-lived keys for scripts.
#[derive(Default)]
pub struct ApiKeys {
    keys: RwLock<HashMap<[u8; 32], Principal>>,
}

impl ApiKeys {
    /// Create a key for `principal`. The returned string is shown once.
    pub fn create(&self, principal: Principal) -> String {
        let key = URL_SAFE_NO_PAD.encode(random::<32>());
        self.keys.write().unwrap_or_else(|e| e.into_inner()).insert(sha256(key.as_bytes()), principal);
        key
    }

    pub fn lookup(&self, key: &str) -> Option<Principal> {
        self.keys.read().unwrap_or_else(|e| e.into_inner()).get(&sha256(key.as_bytes())).cloned()
    }

    pub fn revoke(&self, key: &str) {
        self.keys.write().unwrap_or_else(|e| e.into_inner()).remove(&sha256(key.as_bytes()));
    }
}

/// Pull the session token out of a `Cookie` header.
pub fn cookie_value<'a>(header: &'a str, name: &str) -> Option<&'a str> {
    header.split(';').map(str::trim).find_map(|pair| pair.strip_prefix(name)?.strip_prefix('='))
}

/// `Set-Cookie` value for a new session.
pub fn session_cookie(token: &str, max_age: Duration, secure: bool) -> String {
    let mut c = format!("{SESSION_COOKIE}={token}; HttpOnly; SameSite=Strict; Path=/; Max-Age={}", max_age.as_secs());
    if secure {
        c.push_str("; Secure");
    }
    c
}

/// `Set-Cookie` value that clears the session.
pub fn clear_session_cookie(secure: bool) -> String {
    let mut c = format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0");
    if secure {
        c.push_str("; Secure");
    }
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHEAP: HashCost = HashCost { m_kib: 256, t: 1, p: 1 };

    #[test]
    fn account_hashes_and_deletions_persist_without_plaintext() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("users.json");
        let users = UserStore::open(CHEAP, Some(path.clone())).unwrap();
        users.add("alice", "long-password", Role::Admin).unwrap();
        users.add("bob", "another-password", Role::User).unwrap();
        assert!(users.remove("bob").unwrap());
        drop(users);
        let users = UserStore::open(CHEAP, Some(path.clone())).unwrap();
        assert!(users.authenticate("alice", "long-password").is_some());
        assert!(users.authenticate("bob", "another-password").is_none());
        let bytes = std::fs::read_to_string(path).unwrap();
        assert!(!bytes.contains("long-password"));
        assert!(!bytes.contains("another-password"));
    }

    #[test]
    fn damaged_account_files_and_failed_saves_never_become_empty_success() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("users.json");
        std::fs::write(&path, "broken").unwrap();
        assert!(UserStore::open(CHEAP, Some(path.clone())).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "broken");
        std::fs::remove_file(&path).unwrap();
        let users = UserStore::open(CHEAP, Some(path.clone())).unwrap();
        users.add("alice", "long-password", Role::Admin).unwrap();
        std::fs::create_dir(path.with_extension("tmp")).unwrap();
        assert!(users.add("bob", "another-password", Role::User).is_err());
        assert!(users.authenticate("bob", "another-password").is_none());
        assert!(users.remove("alice").is_err());
        assert!(users.authenticate("alice", "long-password").is_some());
    }

    #[test]
    fn password_hash_round_trips_and_never_stores_the_password() {
        let h = hash_password("correct horse", CHEAP).unwrap();
        assert!(!h.contains("correct horse"));
        assert!(verify_password("correct horse", &h));
        assert!(!verify_password("wrong horse", &h));
    }

    #[test]
    fn each_hash_gets_its_own_salt() {
        assert_ne!(hash_password("same-password", CHEAP).unwrap(), hash_password("same-password", CHEAP).unwrap());
    }

    #[test]
    fn malformed_stored_hashes_never_verify() {
        for bad in ["", "plain", "$fc-argon2id$v1$x$1$1$a$b", "$other$v1$256$1$1$YQ$YQ", "$fc-argon2id$v2$256$1$1$YQ$YQ"] {
            assert!(!verify_password("anything", bad), "{bad}");
        }
    }

    #[test]
    fn users_are_validated_and_authenticated() {
        let users = UserStore::new(CHEAP).unwrap();
        users.add("alice", "password1", Role::Admin).unwrap();
        assert_eq!(users.add("alice", "password2", Role::User).unwrap_err().to_string(), "account already exists");
        assert!(matches!(users.add("../evil", "password1", Role::User), Err(AuthError::BadName)));
        assert!(matches!(users.add("bob", "short", Role::User), Err(AuthError::WeakPassword)));

        let a = users.authenticate("alice", "password1").unwrap();
        assert_eq!(a.role, Role::Admin);
        assert!(users.authenticate("alice", "nope-nope").is_none());
        assert!(users.authenticate("ghost", "password1").is_none());
    }

    #[test]
    fn sessions_resolve_until_revoked_or_expired() {
        let store = SessionStore::new(Duration::from_secs(60));
        let p = Principal { name: "alice".into(), role: Role::Admin };
        let t = store.issue(p.clone());
        assert_eq!(store.lookup(&t), Some(p));
        assert_eq!(store.lookup("not-a-token"), None);
        store.revoke(&t);
        assert_eq!(store.lookup(&t), None);

        let short = SessionStore::new(Duration::from_millis(1));
        let t = short.issue(Principal { name: "bob".into(), role: Role::User });
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(short.lookup(&t), None, "expired session");
    }

    #[test]
    fn api_keys_resolve_and_revoke() {
        let keys = ApiKeys::default();
        let k = keys.create(Principal { name: "script".into(), role: Role::User });
        assert_eq!(keys.lookup(&k).unwrap().name, "script");
        keys.revoke(&k);
        assert!(keys.lookup(&k).is_none());
    }

    #[test]
    fn cookie_parsing_and_attributes() {
        assert_eq!(cookie_value("a=1; fc_session=tok; b=2", SESSION_COOKIE), Some("tok"));
        assert_eq!(cookie_value("fc_session_x=tok", SESSION_COOKIE), None, "prefix must match a whole name");
        let c = session_cookie("tok", Duration::from_secs(3600), true);
        assert!(c.contains("HttpOnly") && c.contains("SameSite=Strict") && c.contains("Secure"));
    }
}

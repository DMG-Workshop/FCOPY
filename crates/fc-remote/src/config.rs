//! Agent configuration (`agent.toml`) and the client's known-hosts file.
//!
//! ```toml
//! name = "nas"
//! listen = "0.0.0.0:47800"
//! discovery = true
//! # CIDRs allowed to connect. Default: loopback + private LAN ranges only.
//! allow = ["127.0.0.0/8", "::1/128", "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "fc00::/7", "fe80::/10"]
//!
//! [[shares]]
//! name = "backup"
//! path = "/srv/backup"
//! writable = true
//!
//! [[accounts]]
//! name = "alice"
//! kind = "interactive"            # a person typing a password in the client
//! shares = ["backup"]             # omit = all shares
//! [accounts.verifier]             # SCRAM verifier: no password stored
//! salt = "…"; stored_key = "…"; server_key = "…"
//! kdf = { m_cost = 19456, t_cost = 2, p_cost = 1 }
//!
//! [[accounts]]
//! name = "unattended"
//! kind = "unattended"             # pre-shared access key for scripts/daemons
//! ```
//!
//! The file holds verifiers and a server secret, so it is written with mode
//! 0600 on Unix. On Windows it lives under `%ProgramData%\fcopy-agent`, whose
//! ACL should be restricted to Administrators and SYSTEM (the installer's job).

use std::io::{self, Write};
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};

use ipnet::IpNet;
use serde::{Deserialize, Serialize};

use crate::crypto::{random, Verifier};
use crate::protocol::{KdfParams, DEFAULT_PORT};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccountKind {
    /// A person who types a username and password in the client.
    Interactive,
    /// A long random access key used by scripts or another agent; no human present.
    Unattended,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub name: String,
    pub kind: AccountKind,
    pub verifier: Verifier,
    /// Shares this account may use. `None` = all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shares: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Share {
    pub name: String,
    pub path: PathBuf,
    #[serde(default)]
    pub writable: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Limits {
    /// Failed logins from one IP within `window_secs` before it is locked out.
    pub max_failures: u32,
    pub window_secs: u64,
    pub lockout_secs: u64,
    /// Lifetime of a session token (used for reconnects).
    pub session_ttl_secs: u64,
    pub max_sessions: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self { max_failures: 5, window_secs: 600, lockout_secs: 900, session_ttl_secs: 12 * 3600, max_sessions: 32 }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub name: String,
    pub listen: SocketAddr,
    #[serde(default = "default_true")]
    pub discovery: bool,
    #[serde(default = "default_allow")]
    pub allow: Vec<String>,
    #[serde(default)]
    pub shares: Vec<Share>,
    #[serde(default)]
    pub accounts: Vec<Account>,
    #[serde(default)]
    pub limits: Limits,
    /// KDF cost for newly created verifiers.
    #[serde(default)]
    pub kdf: KdfParams,
    /// Keys decoy salts for unknown users. Never leaves this file.
    #[serde(with = "secret_b64")]
    pub server_secret: [u8; 32],
}

fn default_true() -> bool {
    true
}

/// Loopback and private ranges: a LAN tool should not be reachable from the
/// internet unless the admin explicitly says so.
pub fn default_allow() -> Vec<String> {
    ["127.0.0.0/8", "::1/128", "10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "100.64.0.0/10", "fc00::/7", "fe80::/10"]
        .map(String::from)
        .to_vec()
}

impl AgentConfig {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            listen: SocketAddr::from(([0, 0, 0, 0], DEFAULT_PORT)),
            discovery: true,
            allow: default_allow(),
            shares: vec![],
            accounts: vec![],
            limits: Limits::default(),
            kdf: KdfParams::default(),
            server_secret: random(),
        }
    }

    pub fn load(path: &Path) -> io::Result<Self> {
        let text = std::fs::read_to_string(path)?;
        let cfg: Self = toml::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        self.validate()?;
        let text = toml::to_string_pretty(self).map_err(io::Error::other)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        write_private(path, text.as_bytes())
    }

    /// Reject configs that would silently do the wrong thing.
    pub fn validate(&self) -> io::Result<()> {
        let bad = |m: String| Err(io::Error::new(io::ErrorKind::InvalidInput, m));
        for a in &self.allow {
            if parse_net(a).is_none() {
                return bad(format!("allow: '{a}' is not an IP or CIDR"));
            }
        }
        for s in &self.shares {
            if s.name.is_empty() || s.name.contains(['/', '\\']) || s.name == "." || s.name == ".." {
                return bad(format!("share name '{}' is invalid", s.name));
            }
            if !s.path.is_absolute() {
                return bad(format!("share '{}': path must be absolute", s.name));
            }
        }
        let mut names: Vec<_> = self.accounts.iter().map(|a| &a.name).collect();
        names.sort();
        if names.windows(2).any(|w| w[0] == w[1]) {
            return bad("duplicate account name".into());
        }
        Ok(())
    }

    pub fn account(&self, name: &str) -> Option<&Account> {
        self.accounts.iter().find(|a| a.name == name)
    }

    pub fn share(&self, name: &str) -> Option<&Share> {
        self.shares.iter().find(|s| s.name == name)
    }

    /// Shares visible to `account`.
    pub fn shares_for<'a>(&'a self, account: &'a Account) -> impl Iterator<Item = &'a Share> + 'a {
        self.shares
            .iter()
            .filter(move |s| account.shares.as_ref().is_none_or(|allowed| allowed.iter().any(|n| *n == s.name)))
    }

    /// Is this peer address allowed to connect at all?
    pub fn allows(&self, ip: IpAddr) -> bool {
        let ip = ip.to_canonical(); // ::ffff:192.168.1.5 → 192.168.1.5
        self.allow.iter().filter_map(|a| parse_net(a)).any(|net| net.contains(&ip))
    }

    /// Add or replace an account with a fresh verifier for `secret`.
    pub fn set_account(&mut self, name: &str, kind: AccountKind, secret: &str, shares: Option<Vec<String>>) -> io::Result<()> {
        if kind == AccountKind::Interactive && secret.chars().count() < 8 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "password must be at least 8 characters"));
        }
        let verifier = Verifier::new(secret, self.kdf).map_err(io::Error::other)?;
        self.accounts.retain(|a| a.name != name);
        self.accounts.push(Account { name: name.to_string(), kind, verifier, shares });
        Ok(())
    }
}

fn parse_net(s: &str) -> Option<IpNet> {
    s.parse::<IpNet>().ok().or_else(|| s.parse::<IpAddr>().ok().map(IpNet::from))
}

/// Default location for the agent's config and identity.
///
/// `FCOPY_AGENT_DIR` overrides. Otherwise: Linux root → `/var/lib/fcopy-agent`
/// (systemd `StateDirectory`), Linux user → `~/.config/fcopy-agent`, macOS →
/// `~/Library/Application Support/fcopy-agent` (or `/Library/…` for the
/// launchd daemon), Windows → `%ProgramData%\fcopy-agent`.
pub fn default_agent_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("FCOPY_AGENT_DIR") {
        return PathBuf::from(d);
    }
    #[cfg(windows)]
    {
        let base = std::env::var_os("ProgramData").unwrap_or_else(|| "C:\\ProgramData".into());
        PathBuf::from(base).join("fcopy-agent")
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| "/Library".into());
        home.join("Library/Application Support/fcopy-agent")
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // SAFETY: geteuid has no preconditions.
        if unsafe { libc_geteuid() } == 0 {
            return PathBuf::from("/var/lib/fcopy-agent");
        }
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_else(|| PathBuf::from("."));
        base.join("fcopy-agent")
    }
    #[cfg(not(any(unix, windows)))]
    {
        PathBuf::from("fcopy-agent")
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
unsafe extern "C" {
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
}

/// Client-side config dir (known hosts, saved profiles).
pub fn default_client_dir() -> PathBuf {
    if let Some(d) = std::env::var_os("FCOPY_CONFIG_DIR") {
        return PathBuf::from(d);
    }
    #[cfg(windows)]
    {
        let base = std::env::var_os("APPDATA").unwrap_or_else(|| ".".into());
        PathBuf::from(base).join("fcopy")
    }
    #[cfg(target_os = "macos")]
    {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        home.join("Library/Application Support/fcopy")
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
            .unwrap_or_default()
            .join("fcopy")
    }
}

/// Write a file readable only by its owner (0600 on Unix), atomically.
pub fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)
}

/// `host:port fingerprint` lines, like SSH's `known_hosts`.
#[derive(Debug, Default, Clone)]
pub struct KnownHosts {
    entries: Vec<(String, String)>,
}

impl KnownHosts {
    pub fn load(path: &Path) -> Self {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        let entries = text
            .lines()
            .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
            .filter_map(|l| l.split_once(' ').map(|(h, f)| (h.to_string(), f.trim().to_string())))
            .collect();
        Self { entries }
    }

    pub fn get(&self, host_port: &str) -> Option<&str> {
        self.entries.iter().find(|(h, _)| h == host_port).map(|(_, f)| f.as_str())
    }

    pub fn set(&mut self, host_port: &str, fingerprint: &str) {
        self.entries.retain(|(h, _)| h != host_port);
        self.entries.push((host_port.to_string(), fingerprint.to_string()));
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let text: String = self.entries.iter().map(|(h, f)| format!("{h} {f}\n")).collect();
        write_private(path, text.as_bytes())
    }
}

mod secret_b64 {
    use base64::Engine as _;
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(v: &[u8; 32], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&base64::engine::general_purpose::STANDARD.encode(v))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 32], D::Error> {
        let s = String::deserialize(d)?;
        let v = base64::engine::general_purpose::STANDARD.decode(s).map_err(serde::de::Error::custom)?;
        v.try_into().map_err(|_| serde::de::Error::custom("expected 32 bytes"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::TEST_KDF;

    #[test]
    fn default_allowlist_is_lan_only() {
        let c = AgentConfig::new("t");
        for ok in ["127.0.0.1", "192.168.1.50", "10.1.2.3", "172.20.0.1", "::1", "fe80::1", "::ffff:192.168.1.5", "100.100.1.1"] {
            assert!(c.allows(ok.parse().unwrap()), "{ok}");
        }
        for bad in ["8.8.8.8", "172.32.0.1", "2001:db8::1", "1.1.1.1"] {
            assert!(!c.allows(bad.parse().unwrap()), "{bad}");
        }
    }

    #[test]
    fn round_trip_and_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agent.toml");
        let mut c = AgentConfig::new("nas");
        c.kdf = TEST_KDF;
        c.shares.push(Share { name: "backup".into(), path: dir.path().to_path_buf(), writable: true });
        c.set_account("alice", AccountKind::Interactive, "correct horse", Some(vec!["backup".into()])).unwrap();
        c.save(&path).unwrap();
        let back = AgentConfig::load(&path).unwrap();
        assert_eq!(back.account("alice").unwrap().verifier, c.account("alice").unwrap().verifier);
        assert_eq!(back.server_secret, c.server_secret);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("correct horse"), "no plaintext password in the config");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
    }

    #[test]
    fn validation() {
        let mut c = AgentConfig::new("x");
        c.allow.push("not-an-ip".into());
        assert!(c.validate().is_err());
        let mut c = AgentConfig::new("x");
        c.shares.push(Share { name: "../etc".into(), path: "/etc".into(), writable: false });
        assert!(c.validate().is_err());
        let mut c = AgentConfig::new("x");
        c.kdf = TEST_KDF;
        assert!(c.set_account("bob", AccountKind::Interactive, "short", None).is_err());
        assert!(c.set_account("bot", AccountKind::Unattended, "k", None).is_ok());
    }

    #[test]
    fn share_visibility_per_account() {
        let mut c = AgentConfig::new("x");
        c.kdf = TEST_KDF;
        c.shares.push(Share { name: "a".into(), path: "/a".into(), writable: true });
        c.shares.push(Share { name: "b".into(), path: "/b".into(), writable: true });
        c.set_account("only-a", AccountKind::Interactive, "password1", Some(vec!["a".into()])).unwrap();
        c.set_account("all", AccountKind::Interactive, "password1", None).unwrap();
        let names = |n: &str| c.shares_for(c.account(n).unwrap()).map(|s| s.name.clone()).collect::<Vec<_>>();
        assert_eq!(names("only-a"), vec!["a"]);
        assert_eq!(names("all"), vec!["a", "b"]);
    }

    #[test]
    fn known_hosts_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("known_hosts");
        let mut k = KnownHosts::default();
        k.set("nas:47800", "SHA256:abc");
        k.set("nas:47800", "SHA256:def");
        k.save(&p).unwrap();
        assert_eq!(KnownHosts::load(&p).get("nas:47800"), Some("SHA256:def"));
    }
}

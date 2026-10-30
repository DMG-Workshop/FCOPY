//! Remote destinations for the window: the form's address becomes a backend,
//! and discovery runs off the UI thread so the window never waits on it.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use fc_engine::TransferBackend;
use fc_remote::backend::RemoteBackend;
use fc_remote::client::{Connector, RemoteSpec};
use fc_remote::config::{KnownHosts, default_client_dir};
use fc_remote::discovery::{Peer, browse};

/// The remote part of the new-transfer form.
#[derive(Default, Clone)]
pub struct RemoteForm {
    pub enabled: bool,
    /// `user@host:port`, or `host:port`.
    pub address: String,
    /// A folder on the PC, below its share, for example `backup/photos`.
    pub folder: String,
    /// Used for this job only. Cleared as soon as the job starts.
    pub password: String,
    /// The fingerprint from discovery, or typed. Not trusted until checked.
    pub fingerprint: String,
    /// The user has compared `fingerprint` with the PC's own fingerprint.
    pub fingerprint_checked: bool,
}

pub fn known_hosts_path() -> PathBuf {
    default_client_dir().join("known_hosts")
}

/// The fingerprint already trusted for this address, if any.
pub fn trusted_fingerprint(address: &str) -> Option<String> {
    let spec = RemoteSpec::parse(&format!("{}:/x", address.trim())).ok()?;
    KnownHosts::load(&known_hosts_path())
        .get(&spec.host_port())
        .map(str::to_string)
}

/// Build the backend a job will use. The error says what is missing, and is
/// shown in the form.
pub fn backend_for(form: &RemoteForm) -> Result<Arc<dyn TransferBackend>, String> {
    let folder = form.folder.trim().trim_matches('/');
    let spec = RemoteSpec::parse(&format!("{}:/{}", form.address.trim(), folder))
        .map_err(|e| e.to_string())?;
    if form.password.is_empty() {
        return Err("enter the PC's password or access key".into());
    }
    let host_port = spec.host_port();
    let known = KnownHosts::load(&known_hosts_path());
    let pinned = match known.get(&host_port) {
        Some(fp) => fp.to_string(),
        None if form.fingerprint_checked && !form.fingerprint.is_empty() => {
            // The user has confirmed this fingerprint, so trust it and remember it.
            let mut known = known;
            known.set(&host_port, &form.fingerprint);
            let _ = known.save(&known_hosts_path());
            form.fingerprint.clone()
        }
        None => {
            return Err(
                "compare the PC's fingerprint with the one it shows, then tick the box".into(),
            );
        }
    };
    let connector = Connector::new(spec)
        .password(form.password.clone())
        .pinned(Some(pinned));
    let backend = RemoteBackend::new(connector).map_err(|e| e.to_string())?;
    Ok(Arc::new(backend))
}

/// One discovery search, run on its own thread.
#[derive(Default)]
pub struct PeerSearch {
    rx: Option<Receiver<Result<Vec<Peer>, String>>>,
}

impl PeerSearch {
    pub fn start(&mut self) {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(browse(Duration::from_secs(3)).map_err(|e| e.to_string()));
        });
        self.rx = Some(rx);
    }

    pub fn busy(&self) -> bool {
        self.rx.is_some()
    }

    /// The finished search, once. `None` while it is still running.
    pub fn poll(&mut self) -> Option<Result<Vec<Peer>, String>> {
        let result = self.rx.as_ref()?.try_recv().ok()?;
        self.rx = None;
        Some(result)
    }
}

/// `user@host:port` for a discovered peer, keeping any user already typed.
pub fn address_for(current: &str, peer: &Peer) -> String {
    let user = current
        .split_once('@')
        .map(|(u, _)| format!("{u}@"))
        .unwrap_or_default();
    let host = peer
        .addresses
        .first()
        .map(ToString::to_string)
        .unwrap_or_else(|| peer.host.clone());
    format!("{user}{host}:{}", peer.port)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form() -> RemoteForm {
        RemoteForm {
            enabled: true,
            address: "alice@127.0.0.1:47800".into(),
            folder: "backup/photos".into(),
            password: "agent-pass-1".into(),
            fingerprint: "SHA256:abc".into(),
            fingerprint_checked: false,
        }
    }

    #[test]
    fn a_fingerprint_must_be_checked_before_a_copy_starts() {
        let mut f = form();
        f.folder = "backup/gui-test-unchecked".into();
        let err = backend_for(&f).err().expect("refused");
        assert!(err.contains("fingerprint"), "{err}");
    }

    #[test]
    fn a_password_is_required() {
        let mut f = form();
        f.password.clear();
        assert!(backend_for(&f).err().unwrap().contains("password"));
    }

    #[test]
    fn a_bad_folder_is_reported_not_sent() {
        let mut f = form();
        f.folder = "backup/../../etc".into();
        assert!(backend_for(&f).is_err());
    }

    #[test]
    fn a_peer_address_keeps_the_user_already_typed() {
        let peer = Peer {
            name: "nas".into(),
            host: "nas.local".into(),
            port: 47800,
            addresses: vec!["192.168.1.50".parse().unwrap()],
            fingerprint: "SHA256:x".into(),
        };
        assert_eq!(address_for("bob@old:1", &peer), "bob@192.168.1.50:47800");
        assert_eq!(address_for("", &peer), "192.168.1.50:47800");
    }
}

//! Remote destinations for the window: the form's address becomes a backend,
//! and discovery runs off the UI thread so the window never waits on it.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use fc_remote::backend::RemoteBackend;
use fc_remote::client::{Connector, RemoteSpec};
use fc_remote::config::{KnownHosts, default_client_dir};
use fc_remote::discovery::{Peer, browse};

/// The remote part of the new-transfer form.
#[derive(Default, Clone)]
pub struct RemoteForm {
    pub enabled: bool,
    pub download: bool,
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
    /// A saved pin, bound to the profile's endpoint rather than editable text.
    pub profile: Option<fc_remote::profiles::Profile>,
}

impl RemoteForm {
    pub fn for_target(target: &str, download: bool) -> Result<Self, String> {
        let spec = RemoteSpec::parse(target.trim()).map_err(|e| e.to_string())?;
        let user = spec.user.as_ref().map(|user| format!("{user}@")).unwrap_or_default();
        Ok(Self {
            enabled: true, download,
            address: format!("{user}{}", spec.host_port()),
            folder: spec.path, ..Default::default()
        })
    }

    pub fn from_profile(profile: fc_remote::profiles::Profile, download: bool) -> Result<Self, String> {
        let mut form = Self::for_target(&profile.target, download)?;
        form.fingerprint = profile.fingerprint.clone();
        form.fingerprint_checked = true;
        form.profile = Some(profile);
        Ok(form)
    }

    pub fn clear_identity(&mut self) {
        self.profile = None;
        self.password.clear();
        self.fingerprint.clear();
        self.fingerprint_checked = false;
    }
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
pub fn backend_for(form: &RemoteForm) -> Result<Arc<RemoteBackend>, String> {
    let connector = connector_for(form)?;
    let backend = RemoteBackend::new(connector).map_err(|e| e.to_string())?;
    Ok(Arc::new(backend))
}

fn connector_for(form: &RemoteForm) -> Result<Connector, String> {
    let folder = if form.download { "browse" } else { form.folder.trim().trim_matches('/') };
    let spec = RemoteSpec::parse(&format!("{}:/{}", form.address.trim(), folder))
        .map_err(|e| e.to_string())?;
    if form.password.is_empty() {
        return Err("enter the PC's password or access key".into());
    }
    let host_port = spec.host_port();
    let pinned = if let Some(profile) = &form.profile {
        let saved = fc_remote::profiles::validate(&profile.name, &profile.target, &profile.fingerprint)
            .map_err(|e| e.to_string())?;
        if saved.host_port() != host_port {
            return Err("the address no longer matches the saved profile; select the PC again".into());
        }
        profile.fingerprint.clone()
    } else {
        let known = KnownHosts::load(&known_hosts_path());
        match known.get(&host_port) {
        Some(fp) => fp.to_string(),
        None if form.fingerprint_checked && !form.fingerprint.is_empty() => {
            // The user has confirmed this fingerprint, so trust it and remember it.
            let mut known = known;
            known.set(&host_port, &form.fingerprint);
            known.save(&known_hosts_path()).map_err(|e| format!("cannot save trusted fingerprint: {e}"))?;
            form.fingerprint.clone()
        }
        None => {
            return Err(
                "compare the PC's fingerprint with the one it shows, then tick the box".into(),
            );
        }
        }
    };
    Ok(Connector::new(spec)
        .password(form.password.clone())
        .pinned(Some(pinned)))
}

type BrowseResult = Result<(String, Vec<fc_remote::protocol::DirEntry>), String>;

#[derive(Default)]
pub struct RemoteBrowser {
    pub path: String,
    pub entries: Vec<fc_remote::protocol::DirEntry>,
    pub error: Option<String>,
    rx: Option<Receiver<BrowseResult>>,
}

impl RemoteBrowser {
    pub fn busy(&self) -> bool { self.rx.is_some() }

    pub fn start(&mut self, form: &RemoteForm, path: String, ctx: &egui::Context) {
        let mut form = form.clone();
        form.download = true;
        let backend = match backend_for(&form) {
            Ok(b) => b,
            Err(e) => { self.error = Some(e); return; }
        };
        self.start_backend(backend, path, ctx);
    }

    fn start_backend(&mut self, backend: Arc<RemoteBackend>, path: String, ctx: &egui::Context) {
        let (tx, rx) = mpsc::channel();
        let ctx = ctx.clone();
        self.error = None;
        self.rx = Some(rx);
        std::thread::spawn(move || {
            let result = backend.browse(&path).map(|entries| (path, entries)).map_err(|e| e.to_string());
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }

    pub fn poll(&mut self) {
        let result = match self.rx.as_ref().map(Receiver::try_recv) {
            Some(Ok(r)) => r,
            Some(Err(mpsc::TryRecvError::Disconnected)) => Err("remote browser worker stopped".into()),
            _ => return,
        };
        self.rx = None;
        match result {
            Ok((path, entries)) => { self.path = path; self.entries = entries; }
            Err(e) => self.error = Some(e),
        }
    }
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

    #[test]
    fn saved_profiles_prefill_both_directions_and_pin_the_exact_endpoint() {
        let profile = fc_remote::profiles::Profile {
            id: 1, name: "VPN NAS".into(),
            target: "alice@[fd00::1]:47801:/backup/photos".into(),
            fingerprint: fc_remote::crypto::fingerprint(b"saved certificate"),
        };
        for download in [false, true] {
            let mut form = RemoteForm::from_profile(profile.clone(), download).unwrap();
            assert_eq!(form.address, "alice@[fd00::1]:47801");
            assert_eq!(form.folder, "backup/photos");
            assert_eq!(form.download, download);
            assert!(form.password.is_empty());
            form.password = "operation-only".into();
            form.fingerprint = fc_remote::crypto::fingerprint(b"untrusted UI text");
            let connector = connector_for(&form).unwrap();
            assert_eq!(connector.pinned_fingerprint().as_deref(), Some(profile.fingerprint.as_str()));
            form.address = "alice@[fd00::2]:47801".into();
            assert!(connector_for(&form).err().unwrap().contains("no longer matches"));
            form.clear_identity();
            assert!(form.profile.is_none());
            assert!(form.password.is_empty() && form.fingerprint.is_empty());
            assert!(!form.fingerprint_checked);
        }
    }

    fn form() -> RemoteForm {
        RemoteForm {
            enabled: true,
            download: false,
            address: "alice@127.0.0.1:47800".into(),
            folder: "backup/photos".into(),
            password: "agent-pass-1".into(),
            fingerprint: "SHA256:abc".into(),
            fingerprint_checked: false,
            profile: None,
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

    #[test]
    fn desktop_browser_loads_shares_and_entries_off_the_ui_thread() {
        use fc_remote::config::{AccountKind, AgentConfig, Share};
        use fc_remote::protocol::KdfParams;
        use fc_remote::server::Server;
        use fc_remote::tls::Identity;

        let rt = tokio::runtime::Runtime::new().unwrap();
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("file.txt"), b"browser").unwrap();
        let handle = rt.block_on(async {
            let mut cfg = AgentConfig::new("desktop-test");
            cfg.listen = "127.0.0.1:0".parse().unwrap();
            cfg.kdf = KdfParams { m_cost: 256, t_cost: 1, p_cost: 1 };
            cfg.shares.push(Share { name: "read-only".into(), path: root.path().into(), writable: false });
            cfg.set_account("alice", AccountKind::Interactive, "password1", None).unwrap();
            let server = Server::bind(cfg, &Identity::generate("localhost").unwrap()).unwrap();
            let handle = server.handle();
            tokio::spawn(server.run());
            handle
        });
        let profile = fc_remote::profiles::Profile {
            id: 1,
            name: "Browser test".into(),
            target: format!("alice@127.0.0.1:{}:/read-only", handle.local_addr().unwrap().port()),
            fingerprint: handle.fingerprint().into(),
        };
        let mut form = RemoteForm::from_profile(profile, true).unwrap();
        form.password = "password1".into();
        let backend = backend_for(&form).unwrap();
        let ctx = egui::Context::default();
        let mut browser = RemoteBrowser::default();
        for path in ["", "read-only"] {
            browser.start_backend(backend.clone(), path.into(), &ctx);
            assert!(browser.busy());
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while browser.busy() {
                browser.poll();
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(browser.error.is_none(), "{:?}", browser.error);
            assert_eq!(browser.path, path);
            assert_eq!(browser.entries.len(), 1);
        }
        assert_eq!(browser.entries[0].name, "file.txt");
        handle.shutdown();
    }

    #[test]
    fn desktop_browser_reports_a_disconnected_worker() {
        let (tx, rx) = mpsc::channel();
        drop(tx);
        let mut browser = RemoteBrowser { rx: Some(rx), ..Default::default() };
        browser.poll();
        assert!(!browser.busy());
        assert!(browser.error.unwrap().contains("stopped"));
    }
}

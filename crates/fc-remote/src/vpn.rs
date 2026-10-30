//! Provider links and local desktop preferences. VPN tunnels remain OS-managed.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
pub struct Provider {
    pub id: &'static str,
    pub name: &'static str,
    pub download: &'static str,
    pub guide: &'static str,
    pub mesh: Option<Mesh>,
}

#[derive(Debug, Serialize)]
pub struct Mesh {
    pub dashboard: &'static str,
    pub instructions: &'static str,
    pub example: &'static str,
}

/// A representative selection, not a ranking or endorsement.
pub const PROVIDERS: &[Provider] = &[
    Provider {
        id: "nordvpn",
        name: "NordVPN",
        download: "https://nordvpn.com/download/",
        guide: "https://support.nordvpn.com/",
        mesh: None,
    },
    Provider {
        id: "expressvpn",
        name: "ExpressVPN",
        download: "https://www.expressvpn.com/vpn-download",
        guide: "https://www.expressvpn.com/support/",
        mesh: None,
    },
    Provider {
        id: "surfshark",
        name: "Surfshark",
        download: "https://surfshark.com/download",
        guide: "https://support.surfshark.com/",
        mesh: None,
    },
    Provider {
        id: "protonvpn",
        name: "Proton VPN",
        download: "https://protonvpn.com/download",
        guide: "https://protonvpn.com/support/",
        mesh: None,
    },
    Provider {
        id: "mullvad",
        name: "Mullvad",
        download: "https://mullvad.net/en/download",
        guide: "https://mullvad.net/en/help",
        mesh: None,
    },
    Provider {
        id: "pia",
        name: "Private Internet Access",
        download: "https://www.privateinternetaccess.com/download",
        guide: "https://helpdesk.privateinternetaccess.com/",
        mesh: None,
    },
    Provider {
        id: "cyberghost",
        name: "CyberGhost",
        download: "https://www.cyberghostvpn.com/download",
        guide: "https://support.cyberghostvpn.com/",
        mesh: None,
    },
    Provider {
        id: "windscribe",
        name: "Windscribe",
        download: "https://windscribe.com/download",
        guide: "https://windscribe.com/guides",
        mesh: None,
    },
    Provider {
        id: "tunnelbear",
        name: "TunnelBear",
        download: "https://www.tunnelbear.com/download",
        guide: "https://help.tunnelbear.com/",
        mesh: None,
    },
    Provider {
        id: "ipvanish",
        name: "IPVanish",
        download: "https://www.ipvanish.com/vpn-setup/",
        guide: "https://support.ipvanish.com/",
        mesh: None,
    },
    Provider {
        id: "tailscale",
        name: "Tailscale",
        download: "https://tailscale.com/download",
        guide: "https://tailscale.com/docs/how-to/quickstart",
        mesh: Some(Mesh {
            dashboard: "https://console.tailscale.com/admin/machines",
            instructions: "Install Tailscale on both PCs and sign in to the same tailnet. Use the remote PC's Tailscale IP or MagicDNS name.",
            example: "alice@nas.tailnet-name.ts.net:47800:/backup",
        }),
    },
    Provider {
        id: "zerotier",
        name: "ZeroTier",
        download: "https://www.zerotier.com/download/",
        guide: "https://docs.zerotier.com/start/",
        mesh: Some(Mesh {
            dashboard: "https://my.zerotier.com/",
            instructions: "Install ZeroTier on both PCs, join the same private network, and authorize both members in ZeroTier Central. Use the remote PC's managed IP.",
            example: "alice@10.147.17.2:47800:/backup",
        }),
    },
];

pub fn provider(id: &str) -> Option<&'static Provider> {
    PROVIDERS.iter().find(|p| p.id == id)
}

/// No VPN passwords, access tokens, or shell arguments are stored here.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct DesktopLinks {
    #[serde(default)]
    pub apps: BTreeMap<String, PathBuf>,
    #[serde(default)]
    pub target: String,
}

impl DesktopLinks {
    pub fn load(path: &Path) -> io::Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e),
        };
        let links: Self =
            toml::from_str(&text).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        links.validate()?;
        Ok(links)
    }

    fn validate(&self) -> io::Result<()> {
        for (id, path) in &self.apps {
            if provider(id).is_none() || !path.is_absolute() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "VPN app links need a known provider and an absolute path",
                ));
            }
        }
        if !self.target.is_empty() {
            crate::client::RemoteSpec::parse(&self.target)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
        }
        Ok(())
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        self.validate()?;
        let text = toml::to_string_pretty(self).map_err(io::Error::other)?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        crate::config::write_private(path, text.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_has_ten_consumer_services_and_two_mesh_networks() {
        assert_eq!(PROVIDERS.iter().filter(|p| p.mesh.is_none()).count(), 10);
        assert_eq!(PROVIDERS.iter().filter(|p| p.mesh.is_some()).count(), 2);
        let mut ids = std::collections::HashSet::new();
        for p in PROVIDERS {
            assert!(ids.insert(p.id));
            for url in [p.download, p.guide]
                .into_iter()
                .chain(p.mesh.as_ref().map(|m| m.dashboard))
            {
                assert!(url.starts_with("https://"));
            }
            if let Some(mesh) = &p.mesh {
                crate::client::RemoteSpec::parse(mesh.example).unwrap();
            }
        }
    }

    #[test]
    fn links_round_trip_and_missing_files_are_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("vpn.toml");
        let mut links = DesktopLinks::load(&path).unwrap();
        assert!(links.apps.is_empty());
        links
            .apps
            .insert("tailscale".into(), dir.path().join("vpn-app"));
        links.target = "alice@[fd7a:115c:a1e0::1]:47800:/backup".into();
        links.save(&path).unwrap();
        let loaded = DesktopLinks::load(&path).unwrap();
        assert_eq!(loaded.apps, links.apps);
        assert_eq!(loaded.target, links.target);
        links.target.clear();
        links.save(&path).unwrap();
        assert!(DesktopLinks::load(&path).unwrap().target.is_empty());
        links.target = "alice@host:/../outside".into();
        assert!(links.save(&path).is_err());
        links.target.clear();
        links.apps.insert("tailscale".into(), "relative.exe".into());
        assert!(links.save(&path).is_err());
        links.apps = loaded.apps.clone();
        links.apps.insert("unknown".into(), dir.path().join("app"));
        assert!(links.save(&path).is_err());
        assert_eq!(DesktopLinks::load(&path).unwrap().apps, loaded.apps);
        std::fs::write(&path, "invalid = [").unwrap();
        assert!(DesktopLinks::load(&path).is_err());
    }
}

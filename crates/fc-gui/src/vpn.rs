//! VPN app links and handoff to the ordinary authenticated remote transfer form.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc::{self, Receiver};

use eframe::egui;
use fc_remote::client::RemoteSpec;
use fc_remote::vpn::{DesktopLinks, PROVIDERS};

use crate::remote::RemoteForm;

pub struct VpnSettings {
    links: DesktopLinks,
    selected: usize,
    app_path: String,
    target: String,
    notice: Option<String>,
    launches: Vec<Receiver<Result<(), String>>>,
}

impl Default for VpnSettings {
    fn default() -> Self {
        let (links, notice) = match DesktopLinks::load(&settings_path()) {
            Ok(links) => (links, None),
            Err(e) => (
                DesktopLinks::default(),
                Some(format!("Cannot load VPN links: {e}")),
            ),
        };
        let app_path = links
            .apps
            .get(PROVIDERS[0].id)
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let target = links.target.clone();
        Self {
            links,
            selected: 0,
            app_path,
            target,
            notice,
            launches: Vec::new(),
        }
    }
}

fn settings_path() -> PathBuf {
    fc_remote::config::default_client_dir().join("vpn-links.toml")
}

pub fn remote_form(target: &str, download: bool) -> Result<RemoteForm, String> {
    let spec = RemoteSpec::parse(target.trim()).map_err(|e| e.to_string())?;
    let user = spec
        .user
        .as_ref()
        .map(|user| format!("{user}@"))
        .unwrap_or_default();
    Ok(RemoteForm {
        enabled: true,
        download,
        address: format!("{user}{}", spec.host_port()),
        folder: spec.path,
        ..Default::default()
    })
}

fn launch_command(path: &Path) -> Result<Command, String> {
    if !path.is_absolute() {
        return Err("Enter an absolute app path, without arguments or surrounding quotes.".into());
    }
    #[cfg(target_os = "macos")]
    if path.extension().is_some_and(|ext| ext == "app") && path.is_dir() {
        let mut cmd = Command::new("/usr/bin/open");
        cmd.arg("-a").arg(path);
        return Ok(cmd);
    }
    if !path.is_file() {
        return Err("The app path is not a file. Install the provider's app first.".into());
    }
    #[cfg(windows)]
    if !path
        .extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("exe"))
    {
        return Err("Select the VPN app's .exe file, not a shortcut or command script.".into());
    }
    Ok(Command::new(path))
}

impl VpnSettings {
    /// Returns a prefilled form, never a connected or trusted session.
    pub fn show(&mut self, ui: &mut egui::Ui) -> Option<RemoteForm> {
        self.launches.retain(|rx| {
            loop {
                match rx.try_recv() {
                    Ok(result) => {
                        self.notice = Some(match result {
                            Ok(()) => {
                                "App launch requested. Sign in and connect in the provider's app."
                                    .into()
                            }
                            Err(e) => format!("Cannot open VPN app: {e}"),
                        });
                    }
                    Err(mpsc::TryRecvError::Disconnected) => return false,
                    Err(mpsc::TryRecvError::Empty) => return true,
                }
            }
        });
        let mut result = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading("Settings / VPN");
            ui.label("Ten popular VPN services, plus two private networks. Not a ranking or endorsement.");
            ui.label("Sign in and connect in the VPN app. FCOPY does not store VPN credentials, manage tunnels, or provide a kill switch.");
            ui.separator();
            let previous = self.selected;
            egui::ComboBox::from_label("VPN service")
                .selected_text(PROVIDERS[self.selected].name)
                .show_ui(ui, |ui| {
                    for (i, p) in PROVIDERS.iter().enumerate() {
                        ui.selectable_value(&mut self.selected, i, p.name);
                    }
                });
            let provider = &PROVIDERS[self.selected];
            if previous != self.selected {
                self.app_path = self.links.apps.get(provider.id).map(|p| p.display().to_string()).unwrap_or_default();
                self.notice = None;
            }
            ui.horizontal(|ui| {
                ui.hyperlink_to("Download / install", provider.download);
                ui.hyperlink_to("Setup / help", provider.guide);
            });
            ui.label("Installed app's absolute path (.exe on Windows, .app on macOS, executable on Linux):");
            ui.text_edit_singleline(&mut self.app_path);
            ui.horizontal(|ui| {
                if ui.button("Save app link").clicked() {
                    let path = PathBuf::from(self.app_path.trim());
                    let checked = if self.app_path.trim().is_empty() { Ok(()) } else { launch_command(&path).map(|_| ()) };
                    self.notice = Some(match checked {
                        Err(e) => e,
                        Ok(()) => {
                            let previous = self.links.apps.remove(provider.id);
                            if !self.app_path.trim().is_empty() { self.links.apps.insert(provider.id.into(), path); }
                            match self.links.save(&settings_path()) {
                                Ok(()) => "App link saved. Leave the path empty to remove a link.".into(),
                                Err(e) => {
                                    self.links.apps.remove(provider.id);
                                    if let Some(previous) = previous { self.links.apps.insert(provider.id.into(), previous); }
                                    format!("Cannot save VPN link: {e}")
                                }
                            }
                        }
                    });
                }
                if ui.button("Open installed app").clicked() {
                    let path = PathBuf::from(self.app_path.trim());
                    let (tx, rx) = mpsc::channel();
                    let ctx = ui.ctx().clone();
                    self.launches.push(rx);
                    std::thread::spawn(move || {
                        match launch_command(&path).and_then(|mut cmd| cmd.spawn().map_err(|e| e.to_string())) {
                            Ok(mut child) => {
                                let _ = tx.send(Ok(()));
                                ctx.request_repaint();
                                // Reap the app without blocking the GUI or owning its lifetime.
                                let result = child.wait().map_err(|e| e.to_string()).and_then(|status| {
                                    if status.success() { Ok(()) } else { Err(format!("{} exited with {status}", path.display())) }
                                });
                                if let Err(e) = result { let _ = tx.send(Err(e)); ctx.request_repaint(); }
                            }
                            Err(e) => { let _ = tx.send(Err(e)); ctx.request_repaint(); }
                        }
                    });
                }
            });
            if let Some(mesh) = &provider.mesh {
                ui.separator();
                ui.heading("Connect two PCs privately");
                ui.label(mesh.instructions);
                ui.hyperlink_to("Open network dashboard", mesh.dashboard);
                ui.label("Run fcopy-agent on the remote PC. Permit UDP 47800 (or your chosen port) in its firewall and the VPN's access rules. Check the agent's allowlist; do not make it public.");
                ui.label("LAN discovery may not cross the VPN. Enter the VPN address explicitly and verify the agent fingerprint.");
                ui.label(format!("Example: {}", mesh.example));
                ui.text_edit_singleline(&mut self.target);
                ui.horizontal(|ui| {
                    if ui.button("Save VPN target").clicked() {
                        let previous = std::mem::replace(&mut self.links.target, self.target.trim().to_owned());
                        self.notice = Some(match self.links.save(&settings_path()) {
                            Ok(()) => "VPN target saved; credentials are not saved.".into(),
                            Err(e) => {
                                self.links.target = previous;
                                format!("Cannot save VPN target: {e}")
                            }
                        });
                    }
                    for (label, download) in [("Upload to this PC", false), ("Download from this PC", true)] {
                        if ui.button(label).clicked() {
                            match remote_form(&self.target, download) {
                                Ok(form) => result = Some(form),
                                Err(e) => self.notice = Some(e),
                            }
                        }
                    }
                });
            } else {
                ui.label("A commercial VPN connection alone does not connect your PCs. Select Tailscale or ZeroTier for private PC-to-PC setup.");
            }
            ui.separator();
            ui.label("Transfers use OS routing. A second VPN, split tunneling, or a provider kill switch can block the private network; check both apps' settings.");
            if let Some(notice) = &self.notice { ui.label(notice); }
        });
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vpn_target_handoff_preserves_ipv6_user_port_and_share_without_trust() {
        for download in [false, true] {
            let form =
                remote_form(" alice@[fd7a:115c:a1e0::1]:47801:/backup/photos ", download).unwrap();
            assert_eq!(form.address, "alice@[fd7a:115c:a1e0::1]:47801");
            assert_eq!(form.folder, "backup/photos");
            assert!(form.enabled);
            assert_eq!(form.download, download);
            assert!(form.password.is_empty() && form.fingerprint.is_empty());
            assert!(!form.fingerprint_checked);
        }
        assert!(remote_form("alice@host:/../outside", false).is_err());
        assert!(remote_form("C:/backup", false).is_err());
    }

    #[test]
    fn launcher_accepts_only_an_existing_absolute_app_without_shell_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let app = dir.path().join("test app.exe");
        std::fs::write(&app, b"not launched").unwrap();
        let cmd = launch_command(&app).unwrap();
        assert_eq!(cmd.get_program(), app.as_os_str());
        assert_eq!(cmd.get_args().count(), 0);
        assert!(launch_command(Path::new("relative.exe")).is_err());
        assert!(launch_command(&dir.path().join("missing.exe")).is_err());
        assert!(launch_command(dir.path()).is_err());
        assert!(launch_command(Path::new(&format!("{} --argument", app.display()))).is_err());
    }
}

//! Saved-PC controls. Reachability tests run off the UI thread and never log in.

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};

use eframe::egui;
use fc_remote::client::{RemoteSpec, probe};
use fc_remote::profiles::{Profile, ProfileStore};

use crate::remote::RemoteForm;

#[derive(Clone, Default, PartialEq, Eq)]
struct Draft {
    id: Option<u64>,
    name: String,
    target: String,
    fingerprint: String,
    confirmed: bool,
}

impl Draft {
    fn from_profile(profile: &Profile) -> Self {
        Self {
            id: Some(profile.id),
            name: profile.name.clone(),
            target: profile.target.clone(),
            fingerprint: profile.fingerprint.clone(),
            confirmed: true,
        }
    }

    fn address(&self) -> Option<String> {
        let (address, _) = self.target.split_once(":/")?;
        RemoteSpec::parse(&format!("{address}:/profile"))
            .ok()
            .map(|s| s.host_port())
    }
}

struct Check {
    target: String,
    populate_draft: bool,
    label: String,
    pinned: bool,
    rx: Receiver<Result<String, String>>,
}

pub struct Profiles {
    path: PathBuf,
    store: Option<ProfileStore>,
    draft: Option<Draft>,
    removing: Option<u64>,
    check: Option<Check>,
    notice: Option<String>,
}

impl Default for Profiles {
    fn default() -> Self {
        Self::open(fc_remote::config::default_client_dir().join("desktop-profiles.toml"))
    }
}

impl Profiles {
    fn open(path: PathBuf) -> Self {
        let mut profiles = Self {
            path,
            store: None,
            draft: None,
            removing: None,
            check: None,
            notice: None,
        };
        profiles.reload();
        profiles
    }

    fn reload(&mut self) {
        self.draft = None;
        self.removing = None;
        self.check = None;
        match ProfileStore::open(self.path.clone()) {
            Ok(store) => {
                self.store = Some(store);
                self.notice = None;
            }
            Err(e) => {
                self.store = None;
                self.notice = Some(format!(
                    "Cannot load saved PCs: {e}. Fix the profile file and reload; it has not been overwritten."
                ));
            }
        }
    }

    pub fn list(&self) -> &[Profile] {
        self.store.as_ref().map(ProfileStore::list).unwrap_or(&[])
    }

    pub fn available(&self) -> bool {
        self.store.is_some()
    }

    pub fn edit_form(&mut self, form: &RemoteForm) {
        self.check = None;
        self.draft = Some(match &form.profile {
            Some(profile) => Draft::from_profile(profile),
            None => Draft {
                target: format!(
                    "{}:/{}",
                    form.address.trim(),
                    form.folder.trim().trim_matches('/')
                ),
                fingerprint: form.fingerprint.clone(),
                confirmed: form.fingerprint_checked,
                ..Default::default()
            },
        });
    }

    fn start_check(&mut self, draft: &Draft, populate_draft: bool, ctx: &egui::Context) {
        let spec = match RemoteSpec::parse(draft.target.trim()) {
            Ok(spec) => spec,
            Err(e) => {
                self.notice = Some(e.to_string());
                return;
            }
        };
        let pin =
            (!draft.fingerprint.trim().is_empty()).then(|| draft.fingerprint.trim().to_owned());
        let (tx, rx) = mpsc::channel();
        self.notice = None;
        self.check = Some(Check {
            target: draft.target.trim().into(),
            populate_draft,
            label: draft.name.clone(),
            pinned: pin.is_some(),
            rx,
        });
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let result = tokio::runtime::Runtime::new()
                .map_err(|e| e.to_string())
                .and_then(|rt| rt.block_on(probe(&spec, pin)).map_err(|e| e.to_string()));
            let _ = tx.send(result);
            ctx.request_repaint();
        });
    }

    fn poll(&mut self) {
        let Some(check) = &self.check else {
            return;
        };
        let result = match check.rx.try_recv() {
            Ok(result) => result,
            Err(mpsc::TryRecvError::Empty) => return,
            Err(mpsc::TryRecvError::Disconnected) => Err("connection test worker stopped".into()),
        };
        match result {
            Ok(fp) => {
                if let Some(draft) = &mut self.draft {
                    if check.populate_draft
                        && draft.target.trim() == check.target
                        && draft.fingerprint.is_empty()
                    {
                        draft.fingerprint = fp;
                        draft.confirmed = false;
                    }
                }
                let identity = if check.pinned {
                    "the fingerprint matches"
                } else {
                    "fingerprint read, not yet trusted; compare it on the remote PC before saving"
                };
                self.notice = Some(format!(
                    "{}: reachable; {identity}. Login and share access are checked during transfers.",
                    check.label
                ));
            }

            Err(e) => self.notice = Some(format!("{}: connection test failed: {e}", check.label)),
        }
        self.check = None;
    }

    pub fn show(&mut self, ui: &mut egui::Ui) -> Option<(Profile, bool)> {
        self.poll();
        let mut selected = None;
        ui.horizontal(|ui| {
            ui.heading("Saved PCs");
            if ui
                .add_enabled(self.available(), egui::Button::new("New PC"))
                .clicked()
            {
                self.draft = Some(Draft::default());
                self.check = None;
                self.notice = None;
            }
            if ui.button("Reload profiles").clicked() {
                self.reload();
            }
            if self.check.is_some() {
                ui.spinner();
            }
        });
        ui.weak("Profiles save names, targets and confirmed fingerprints only. Passwords are requested per operation.");
        for profile in self.list().to_vec() {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.strong(&profile.name);
                ui.monospace(&profile.target);
                ui.horizontal_wrapped(|ui| {
                    for (label, download) in [("Upload", false), ("Download", true)] {
                        if ui.button(label).clicked() { selected = Some((profile.clone(), download)); }
                    }
                    if ui.button("Edit").clicked() {
                        self.draft = Some(Draft::from_profile(&profile));
                        self.check = None;
                        self.notice = None;
                    }
                    if ui.add_enabled(self.check.is_none(), egui::Button::new("Test connection")).clicked() {
                        self.start_check(&Draft::from_profile(&profile), false, ui.ctx());
                    }
                    if ui.button("Remove").clicked() { self.removing = Some(profile.id); }
                });
                if self.removing == Some(profile.id) {
                    ui.horizontal(|ui| {
                        ui.label("Remove this saved PC? Queued transfers are not changed.");
                        if ui.button("Confirm removal").clicked() {
                            if let Some(store) = &mut self.store {
                                match store.remove(profile.id) {
                                    Ok(()) => {
                                        self.removing = None;
                                        self.check = None;
                                        if self.draft.as_ref().is_some_and(|d| d.id == Some(profile.id)) { self.draft = None; }
                                        self.notice = Some("PC profile removed. Separate known_hosts entries are unchanged.".into());
                                    }
                                    Err(e) => self.notice = Some(format!("Cannot remove PC: {e}")),
                                }
                            }
                        }
                        if ui.button("Keep PC").clicked() { self.removing = None; }
                    });
                }
            });
        }
        let mut test = None;
        let mut save = false;
        let mut close = false;
        if let Some(draft) = &mut self.draft {
            let previous = draft.clone();
            ui.separator();
            ui.strong(if draft.id.is_some() {
                "Edit PC profile"
            } else {
                "New PC profile"
            });
            ui.label("PC name");
            ui.text_edit_singleline(&mut draft.name);
            ui.label("Target: user@host:port:/share/folder (bracket IPv6 addresses)");
            ui.text_edit_singleline(&mut draft.target);
            if draft.address() != previous.address() {
                draft.fingerprint.clear();
                draft.confirmed = false;
            }
            ui.label("Agent fingerprint (test to read it, or paste it from the remote PC)");
            if ui.text_edit_singleline(&mut draft.fingerprint).changed() {
                draft.confirmed = false;
            }
            if !draft.fingerprint.is_empty() {
                ui.checkbox(
                    &mut draft.confirmed,
                    "I compared this fingerprint with fcopy-agent fingerprint on the remote PC",
                );
            }
            if *draft != previous {
                self.check = None;
                self.notice = None;
            }
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(
                        self.check.is_none(),
                        egui::Button::new("Test / read fingerprint"),
                    )
                    .clicked()
                {
                    test = Some(draft.clone());
                }
                save = ui
                    .add_enabled(draft.confirmed, egui::Button::new("Save PC"))
                    .clicked();
                close = ui.button("Cancel edit").clicked();
            });
        }
        if let Some(draft) = test {
            self.start_check(&draft, true, ui.ctx());
        }
        if save {
            if let (Some(store), Some(draft)) = (&mut self.store, &self.draft) {
                match store.save(draft.id, &draft.name, &draft.target, &draft.fingerprint) {
                    Ok(_) => {
                        self.draft = None;
                        self.check = None;
                        self.notice = Some("PC profile saved.".into());
                    }
                    Err(e) => self.notice = Some(format!("Cannot save PC: {e}")),
                }
            }
        }
        if close {
            self.draft = None;
            self.check = None;
        }
        if let Some(notice) = &self.notice {
            ui.label(notice);
        }
        selected
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn wait(profiles: &mut Profiles) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while profiles.check.is_some() {
            profiles.poll();
            assert!(Instant::now() < deadline, "connection check timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn background_checks_read_untrusted_pins_and_reject_identity_changes() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let handle = rt.block_on(async {
            let mut cfg = fc_remote::config::AgentConfig::new("profile-test");
            cfg.listen = "127.0.0.1:0".parse().unwrap();
            let identity = fc_remote::tls::Identity::generate("localhost").unwrap();
            let server = fc_remote::server::Server::bind(cfg, &identity).unwrap();
            let handle = server.handle();
            tokio::spawn(server.run());
            handle
        });
        let dir = tempfile::tempdir().unwrap();
        let mut profiles = Profiles::open(dir.path().join("profiles.toml"));
        let ctx = egui::Context::default();
        let draft = Draft {
            name: "Test PC".into(),
            target: format!(
                "alice@127.0.0.1:{}:/backup",
                handle.local_addr().unwrap().port()
            ),
            ..Default::default()
        };
        profiles.draft = Some(draft.clone());
        profiles.start_check(&draft, true, &ctx);
        assert!(profiles.check.is_some());
        wait(&mut profiles);
        let draft = profiles.draft.as_ref().unwrap();
        assert_eq!(draft.fingerprint, handle.fingerprint());
        assert!(
            !draft.confirmed,
            "a probe must never silently trust a new identity"
        );
        assert!(
            profiles
                .notice
                .as_ref()
                .unwrap()
                .contains("not yet trusted")
        );
        let mut draft = draft.clone();
        profiles.start_check(&draft, true, &ctx);
        wait(&mut profiles);
        assert!(
            profiles
                .notice
                .as_ref()
                .unwrap()
                .contains("fingerprint matches")
        );
        draft.fingerprint = fc_remote::crypto::fingerprint(b"different certificate");
        profiles.draft = Some(draft.clone());
        profiles.start_check(&draft, true, &ctx);
        wait(&mut profiles);
        assert!(
            profiles
                .notice
                .as_ref()
                .unwrap()
                .contains("HOST IDENTITY CHANGED")
        );
        assert_eq!(
            profiles.draft.as_ref().unwrap().fingerprint,
            draft.fingerprint
        );
        handle.shutdown();
    }

    #[test]
    fn editing_drops_old_checks_and_never_copies_passwords_into_profiles() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.toml");
        let mut profiles = Profiles::open(path.clone());
        let (tx, rx) = mpsc::channel();
        profiles.check = Some(Check {
            target: "old:/share".into(),
            populate_draft: true,
            label: "old".into(),
            pinned: false,
            rx,
        });
        let mut form = RemoteForm::for_target("alice@nas:47801:/share", false).unwrap();
        form.password = "never-persist-this-secret".into();
        form.fingerprint = fc_remote::crypto::fingerprint(b"cert");
        form.fingerprint_checked = true;
        profiles.edit_form(&form);
        assert!(tx.send(Ok("old result".into())).is_err());
        let draft = profiles.draft.as_mut().unwrap();
        draft.name = "NAS".into();
        profiles
            .store
            .as_mut()
            .unwrap()
            .save(None, &draft.name, &draft.target, &draft.fingerprint)
            .unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        assert!(!text.contains(&form.password));
        assert!(!text.contains("password"));
    }

    #[test]
    fn damaged_stores_and_dead_workers_are_visible_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.toml");
        std::fs::write(&path, "broken = [").unwrap();
        let mut profiles = Profiles::open(path.clone());
        assert!(!profiles.available());
        assert!(profiles.notice.as_ref().unwrap().contains("Cannot load"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "broken = [");
        let (tx, rx) = mpsc::channel();
        drop(tx);
        profiles.check = Some(Check {
            target: "host:/share".into(),
            populate_draft: true,
            label: "test".into(),
            pinned: false,
            rx,
        });
        profiles.poll();
        assert!(profiles.notice.as_ref().unwrap().contains("worker stopped"));
    }

    #[test]
    fn share_edits_keep_endpoint_identity_and_row_tests_leave_the_editor_alone() {
        let mut draft = Draft {
            target: "alice@[fd00::1]:47801:/".into(),
            ..Default::default()
        };
        let address = draft.address().unwrap();
        draft.target.push_str("backup/photos");
        assert_eq!(draft.address().as_deref(), Some(address.as_str()));
        draft.target = "alice@[fd00::2]:47801:/backup".into();
        assert_ne!(draft.address().as_deref(), Some(address.as_str()));

        let dir = tempfile::tempdir().unwrap();
        let mut profiles = Profiles::open(dir.path().join("profiles.toml"));
        profiles.draft = Some(draft.clone());
        let (tx, rx) = mpsc::channel();
        profiles.check = Some(Check {
            target: draft.target.clone(),
            populate_draft: false,
            label: "saved PC".into(),
            pinned: true,
            rx,
        });
        tx.send(Ok(fc_remote::crypto::fingerprint(b"cert")))
            .unwrap();
        profiles.poll();
        assert!(profiles.draft.as_ref().unwrap().fingerprint.is_empty());
        assert_eq!(profiles.draft.as_ref().unwrap().target, draft.target);
    }

    #[test]
    fn saved_pc_list_and_editor_render_in_a_headless_frame() {
        let dir = tempfile::tempdir().unwrap();
        let mut profiles = Profiles::open(dir.path().join("profiles.toml"));
        let p = profiles
            .store
            .as_mut()
            .unwrap()
            .save(
                None,
                "NAS",
                "alice@nas:/backup",
                &fc_remote::crypto::fingerprint(b"cert"),
            )
            .unwrap();
        profiles.draft = Some(Draft::from_profile(&p));
        let ctx = egui::Context::default();
        let output = ctx.run(egui::RawInput::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                assert!(profiles.show(ui).is_none());
            });
        });
        assert!(!output.shapes.is_empty());
        assert!(profiles.draft.is_some());
        assert_eq!(profiles.list().len(), 1);
    }
}

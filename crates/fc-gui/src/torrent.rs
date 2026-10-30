//! The BitTorrent tab. The engine runs on its own small async runtime, so the
//! window never waits on the network: list reads are immediate, and the few
//! engine calls that change state (add, pause, remove) return at once.
//!
//! The pure parts (form checks, peer parsing, labels) are free of egui and
//! tested headlessly; [`show`] only draws.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use eframe::egui::{self, Color32, RichText};
use fc_engine::units::human_rate;
use fc_torrent::{AddOptions, EngineConfig, TorrentEngine, TorrentPhase, TorrentStatus};

use crate::app::{ACCENT, BAD, DIM, OK, WARN};

/// Most peer addresses given with one transfer.
const MAX_PEERS: usize = 50;

/// What the add form asks for, after checking.
#[derive(Debug, Clone, PartialEq)]
pub enum Source {
    Magnet(String),
    File(PathBuf),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub source: Source,
    pub peers: Vec<SocketAddr>,
    pub folder: PathBuf,
}

/// The add form's fields, as typed.
#[derive(Debug, Default, Clone)]
pub struct TorrentForm {
    pub magnet: String,
    pub file: String,
    pub peers: String,
    pub folder: String,
}

impl TorrentForm {
    /// Check the fields and turn them into a request. Errors are for the user.
    pub fn request(&self, default_folder: &Path) -> Result<Request, String> {
        let magnet = self.magnet.trim();
        let file = self.file.trim();
        let source = match (magnet.is_empty(), file.is_empty()) {
            (true, true) => {
                return Err("Paste a magnet link, or give the path to a .torrent file.".into());
            }
            (false, false) => return Err("Use a magnet link or a .torrent file, not both.".into()),
            (false, true) if !magnet.starts_with("magnet:?") => {
                return Err("A magnet link starts with magnet:?".into());
            }
            (false, true) => Source::Magnet(magnet.to_string()),
            (true, false) => Source::File(PathBuf::from(file)),
        };
        let peers = parse_peers(&self.peers)?;
        let folder = match self.folder.trim() {
            "" => default_folder.to_path_buf(),
            f => PathBuf::from(f),
        };
        Ok(Request {
            source,
            peers,
            folder,
        })
    }
}

/// `host:port` addresses separated by commas. Empty means none.
pub fn parse_peers(raw: &str) -> Result<Vec<SocketAddr>, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(vec![]);
    }
    let peers: Vec<SocketAddr> = raw
        .split(',')
        .map(|p| {
            p.trim()
                .parse()
                .map_err(|_| format!("not a host:port address: {}", p.trim()))
        })
        .collect::<Result<_, _>>()?;
    if peers.len() > MAX_PEERS {
        return Err(format!("give at most {MAX_PEERS} peers"));
    }
    Ok(peers)
}

/// The one word shown for a transfer's situation.
pub fn phase_label(phase: TorrentPhase) -> &'static str {
    match phase {
        TorrentPhase::WaitingForMetadata => "Waiting for peers",
        TorrentPhase::Checking => "Checking",
        TorrentPhase::Downloading => "Downloading",
        TorrentPhase::Seeding => "Seeding",
        TorrentPhase::Paused => "Paused",
        TorrentPhase::Failed => "Failed",
    }
}

fn phase_color(phase: TorrentPhase) -> Color32 {
    match phase {
        TorrentPhase::Downloading => ACCENT,
        TorrentPhase::Seeding => OK,
        TorrentPhase::Paused => WARN,
        TorrentPhase::Failed => BAD,
        TorrentPhase::WaitingForMetadata | TorrentPhase::Checking => DIM,
    }
}

/// The default download folder: the user's Downloads folder, or the working folder.
pub fn default_folder() -> PathBuf {
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    match home {
        Some(h) => PathBuf::from(h).join("Downloads"),
        None => PathBuf::from("."),
    }
}

/// The running engine and the runtime it lives on.
pub struct Lane {
    rt: tokio::runtime::Runtime,
    engine: TorrentEngine,
}

impl Lane {
    /// Start the engine. Waiting magnets and transfers come back from the state folder.
    pub fn start(dht: bool, trackers: bool) -> Result<Lane, String> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .map_err(|e| e.to_string())?;
        let config = EngineConfig {
            state_dir: Some(fc_remote::config::default_client_dir().join("torrents")),
            dht,
            trackers,
            ..EngineConfig::new(default_folder())
        };
        let engine = rt
            .block_on(TorrentEngine::start(config))
            .map_err(|e| e.to_string())?;
        Ok(Lane { rt, engine })
    }

    pub fn list(&self) -> Vec<TorrentStatus> {
        self.engine.list()
    }

    fn add(&self, request: Request) -> Result<usize, String> {
        let opts = AddOptions {
            paused: false,
            peers: request.peers,
            output: Some(request.folder),
        };
        match request.source {
            Source::Magnet(magnet) => self.rt.block_on(self.engine.add_magnet_with(&magnet, opts)),
            Source::File(path) => {
                let bytes = std::fs::read(&path)
                    .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
                self.rt
                    .block_on(self.engine.add_torrent_bytes_with(bytes, opts))
            }
        }
        .map_err(|e| e.to_string())
    }

    fn toggle(&self, row: &TorrentStatus) -> Result<(), String> {
        let phase = row.phase();
        let result = if phase == TorrentPhase::Paused {
            self.rt.block_on(self.engine.resume(row.id))
        } else {
            self.rt.block_on(self.engine.pause(row.id))
        };
        result.map_err(|e| e.to_string())
    }

    fn remove(&self, id: usize, delete_files: bool) -> Result<(), String> {
        self.rt
            .block_on(self.engine.remove(id, delete_files))
            .map_err(|e| e.to_string())
    }

    fn shutdown(&self) {
        self.rt.block_on(self.engine.shutdown());
    }
}

/// Everything the tab shows and does. Owned by the app.
#[derive(Default)]
pub struct Torrents {
    lane: Option<Lane>,
    start_error: Option<String>,
    rows: Vec<TorrentStatus>,
    form: TorrentForm,
    show_form: bool,
    notice: Option<String>,
}

impl Torrents {
    /// Start the engine. A failure is shown in the tab rather than stopping the app.
    pub fn start(dht: bool, trackers: bool) -> Torrents {
        match Lane::start(dht, trackers) {
            Ok(lane) => Torrents {
                lane: Some(lane),
                ..Default::default()
            },
            Err(e) => Torrents {
                start_error: Some(e),
                ..Default::default()
            },
        }
    }

    pub fn is_running(&self) -> bool {
        self.lane.is_some()
    }

    /// Re-read the transfers. Called once per frame; reads are cheap.
    pub fn refresh(&mut self) {
        if let Some(lane) = &self.lane {
            self.rows = lane.list();
        }
    }

    /// True while something is changing, so the tab repaints on a timer.
    pub fn is_busy(&self) -> bool {
        self.rows.iter().any(|r| {
            matches!(
                r.phase(),
                TorrentPhase::Downloading
                    | TorrentPhase::Checking
                    | TorrentPhase::WaitingForMetadata
                    | TorrentPhase::Seeding
            )
        })
    }

    pub fn shutdown(&mut self) {
        if let Some(lane) = self.lane.take() {
            lane.shutdown();
        }
    }

    fn submit(&mut self) {
        let Some(lane) = &self.lane else { return };
        match self.form.request(&default_folder()) {
            Err(e) => self.notice = Some(e),
            Ok(request) => match lane.add(request) {
                Ok(_) => {
                    self.form.magnet.clear();
                    self.form.file.clear();
                    self.notice = Some("Added.".into());
                    self.refresh();
                }
                Err(e) => self.notice = Some(e),
            },
        }
    }
}

/// Draw the tab: the add form, then the transfers.
pub fn show(ui: &mut egui::Ui, t: &mut Torrents) {
    if t.lane.is_none() {
        ui.add_space(12.0);
        ui.colored_label(BAD, "The BitTorrent engine could not start.");
        if let Some(e) = &t.start_error {
            ui.weak(e);
        }
        return;
    }

    ui.horizontal(|ui| {
        ui.strong("BitTorrent transfers");
        if ui
            .button(if t.show_form {
                "Hide form"
            } else {
                "Add magnet or file"
            })
            .clicked()
        {
            t.show_form = !t.show_form;
        }
    });
    if t.show_form {
        add_form(ui, t);
    }
    if let Some(n) = &t.notice {
        ui.weak(n);
    }
    ui.separator();

    if t.rows.is_empty() {
        ui.weak("No BitTorrent transfers. Add a magnet link or a .torrent file.");
        return;
    }
    let mut action: Option<(usize, Action)> = None;
    egui::ScrollArea::vertical().show(ui, |ui| {
        egui::Grid::new("torrent_rows")
            .striped(true)
            .num_columns(6)
            .spacing([14.0, 6.0])
            .show(ui, |ui| {
                for h in ["Name", "Progress", "Status", "Down / up", "Peers", ""] {
                    ui.strong(h);
                }
                ui.end_row();
                for row in &t.rows {
                    let name = row.name.clone().unwrap_or_else(|| row.info_hash.clone());
                    ui.vertical(|ui| {
                        ui.label(RichText::new(name).strong());
                        ui.weak(RichText::new(&row.info_hash).small());
                    });
                    let phase = row.phase();
                    if phase == TorrentPhase::WaitingForMetadata {
                        ui.weak("no file list yet");
                    } else {
                        ui.add(
                            egui::ProgressBar::new((row.percent() / 100.0) as f32)
                                .desired_width(160.0)
                                .show_percentage(),
                        );
                    }
                    ui.vertical(|ui| {
                        ui.colored_label(phase_color(phase), phase_label(phase));
                        if let Some(e) = &row.error {
                            ui.colored_label(BAD, RichText::new(e).small());
                        }
                    });
                    ui.label(format!(
                        "{} / {}",
                        human_rate(row.download_mbps * 1024.0 * 1024.0),
                        human_rate(row.upload_mbps * 1024.0 * 1024.0)
                    ));
                    ui.label(row.live_peers.to_string());
                    ui.horizontal(|ui| {
                        if phase != TorrentPhase::WaitingForMetadata
                            && phase != TorrentPhase::Failed
                            && ui
                                .small_button(if phase == TorrentPhase::Paused {
                                    "Resume"
                                } else {
                                    "Pause"
                                })
                                .clicked()
                        {
                            action = Some((row.id, Action::Toggle));
                        }
                        if ui.small_button("Remove").clicked() {
                            action = Some((row.id, Action::Remove));
                        }
                        if phase != TorrentPhase::WaitingForMetadata
                            && ui.small_button("Delete data").clicked()
                        {
                            action = Some((row.id, Action::DeleteData));
                        }
                    });
                    ui.end_row();
                }
            });
    });

    if let Some((id, act)) = action {
        let Some(lane) = &t.lane else { return };
        let result = match act {
            Action::Toggle => match t.rows.iter().find(|r| r.id == id) {
                Some(row) => lane.toggle(row),
                None => Ok(()),
            },
            Action::Remove => lane.remove(id, false),
            Action::DeleteData => lane.remove(id, true),
        };
        t.notice = result.err();
        t.refresh();
    }
}

#[derive(Clone, Copy)]
enum Action {
    Toggle,
    Remove,
    DeleteData,
}

fn add_form(ui: &mut egui::Ui, t: &mut Torrents) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        egui::Grid::new("torrent_form").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
            ui.label("Magnet link");
            ui.add(egui::TextEdit::singleline(&mut t.form.magnet).hint_text("magnet:?xt=urn:btih:…").desired_width(f32::INFINITY));
            ui.end_row();
            ui.label(".torrent file");
            ui.add(egui::TextEdit::singleline(&mut t.form.file).hint_text("path to a .torrent file").desired_width(f32::INFINITY));
            ui.end_row();
            ui.label("Peers");
            ui.add(egui::TextEdit::singleline(&mut t.form.peers).hint_text("host:port, comma separated; needed when trackers are off").desired_width(f32::INFINITY));
            ui.end_row();
            ui.label("Save to");
            let folder_hint = default_folder().display().to_string();
            ui.add(egui::TextEdit::singleline(&mut t.form.folder).hint_text(folder_hint).desired_width(f32::INFINITY));
            ui.end_row();
        });
        ui.horizontal(|ui| {
            if ui.button("Add transfer").clicked() {
                t.submit();
            }
        });
        ui.weak("Peer discovery stays off unless enabled at launch with --torrent-dht or --torrent-trackers; discovery can expose your IP and info hash.");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder() -> PathBuf {
        PathBuf::from("/downloads")
    }

    #[test]
    fn peers_are_parsed_and_checked() {
        assert_eq!(parse_peers("").unwrap(), vec![]);
        let two = parse_peers(" 10.0.0.2:6881 , 127.0.0.1:51413 ").unwrap();
        assert_eq!(two.len(), 2);
        assert!(parse_peers("not-an-address").is_err());
        let many = (0..=MAX_PEERS)
            .map(|i| format!("10.0.0.{}:6881", i % 250 + 1))
            .collect::<Vec<_>>()
            .join(",");
        assert!(parse_peers(&many).is_err(), "too many peers");
    }

    #[test]
    fn a_form_needs_exactly_one_source() {
        let empty = TorrentForm::default();
        assert!(empty.request(&folder()).is_err());

        let both = TorrentForm {
            magnet: "magnet:?xt=urn:btih:00".into(),
            file: "a.torrent".into(),
            ..Default::default()
        };
        assert!(both.request(&folder()).is_err());

        let not_magnet = TorrentForm {
            magnet: "https://example.com".into(),
            ..Default::default()
        };
        assert!(not_magnet.request(&folder()).is_err());
    }

    #[test]
    fn a_valid_form_becomes_a_request_with_the_default_folder() {
        let magnet = TorrentForm {
            magnet: "  magnet:?xt=urn:btih:00  ".into(),
            ..Default::default()
        };
        let r = magnet.request(&folder()).unwrap();
        assert_eq!(r.source, Source::Magnet("magnet:?xt=urn:btih:00".into()));
        assert_eq!(r.folder, folder());

        let file = TorrentForm {
            file: "/tmp/x.torrent".into(),
            folder: "/data".into(),
            ..Default::default()
        };
        let r = file.request(&folder()).unwrap();
        assert_eq!(r.source, Source::File(PathBuf::from("/tmp/x.torrent")));
        assert_eq!(
            r.folder,
            PathBuf::from("/data"),
            "a typed folder wins over the default"
        );
    }

    #[test]
    fn every_phase_has_a_label() {
        for p in [
            TorrentPhase::WaitingForMetadata,
            TorrentPhase::Checking,
            TorrentPhase::Downloading,
            TorrentPhase::Seeding,
            TorrentPhase::Paused,
            TorrentPhase::Failed,
        ] {
            assert!(!phase_label(p).is_empty());
        }
    }

    #[test]
    fn a_lane_lists_a_waiting_magnet_in_its_folder() {
        // This runs against the real engine, so keep the state folder out of the user's profile.
        let dir = tempfile::tempdir().unwrap();
        let config = EngineConfig {
            state_dir: Some(dir.path().join("state")),
            ..EngineConfig::new(dir.path().join("out"))
        };
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let engine = rt.block_on(TorrentEngine::start(config)).unwrap();
        let lane = Lane { rt, engine };

        let request = Request {
            source: Source::Magnet(
                "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=wait".into(),
            ),
            peers: vec![],
            folder: dir.path().join("out"),
        };
        let id = lane.add(request).unwrap();
        let rows = lane.list();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
        assert_eq!(rows[0].phase(), TorrentPhase::WaitingForMetadata);
        lane.shutdown();
    }
}

//! Drawing + commands. Reads [`AppState`], sends commands to the [`Queue`].
//!
//! Layout follows the desktop console: a tab bar and action bar on top, a
//! navigation sidebar on the left, the queue table in the middle with the active
//! transfer above it, and a status bar along the bottom. New transfers are
//! entered in an "Add task" window.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::Receiver;
use std::sync::Arc;
use std::time::Duration;

use eframe::egui::{self, Color32, RichText};
use fc_engine::units::{human_bytes, human_duration, human_rate};
use fc_engine::{
    Algorithm, CopyOptions, ErrorAction, ErrorClass, ErrorPolicy, Event, JobState, Phase, Queue, QueueConfig,
    TransferRequest, VerifyMode,
};

use crate::remote::{self, PeerSearch, RemoteForm};
use crate::state::{AppState, JobRow, LogKind};
use crate::torrent::{self, Torrents};
use crate::Startup;

pub(crate) const OK: Color32 = Color32::from_rgb(110, 200, 120);
pub(crate) const WARN: Color32 = Color32::from_rgb(230, 180, 80);
pub(crate) const BAD: Color32 = Color32::from_rgb(235, 100, 90);
pub(crate) const ACCENT: Color32 = Color32::from_rgb(234, 88, 12);
pub(crate) const DIM: Color32 = Color32::from_rgb(120, 124, 134);

/// Speed presets for the throttle menu. `None` is unlimited.
const THROTTLE: [(&str, Option<u64>); 7] = [
    ("Unlimited", None),
    ("10 MB/s", Some(10_000_000)),
    ("50 MB/s", Some(50_000_000)),
    ("100 MB/s", Some(100_000_000)),
    ("250 MB/s", Some(250_000_000)),
    ("500 MB/s", Some(500_000_000)),
    ("1 GB/s", Some(1_000_000_000)),
];

/// Which main panel is showing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum View {
    Queue,
    Peers,
    Torrents,
    Logs,
}

/// Which jobs the queue table lists.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Filter {
    All,
    Active,
    Queued,
    Paused,
    Done,
    Errors,
}

impl Filter {
    const ALL: [(Filter, &'static str); 6] = [
        (Filter::All, "All items"),
        (Filter::Active, "Copying"),
        (Filter::Queued, "Queued"),
        (Filter::Paused, "Paused"),
        (Filter::Done, "Done"),
        (Filter::Errors, "Errors"),
    ];

    fn matches(self, row: &JobRow) -> bool {
        let done = matches!(row.state, JobState::Completed | JobState::CompletedWithErrors);
        match self {
            Filter::All => true,
            Filter::Active => row.state == JobState::Running && !row.paused,
            Filter::Queued => row.state == JobState::Pending,
            Filter::Paused => row.paused,
            Filter::Done => done,
            Filter::Errors => matches!(row.state, JobState::Failed(_) | JobState::CompletedWithErrors),
        }
    }
}

/// Form for a new transfer.
struct NewTransfer {
    sources: Vec<PathBuf>,
    add_path: String,
    dest: String,
    verify: VerifyMode,
    overwrite: bool,
    remote: RemoteForm,
}

pub struct FcApp {
    queue: Queue,
    events: Receiver<Event>,
    state: AppState,
    form: NewTransfer,
    view: View,
    filter: Filter,
    search: String,
    show_add: bool,
    /// Index into [`THROTTLE`].
    throttle: usize,
    /// "Do this for all errors of this kind" answers.
    remembered: HashMap<ErrorClass, ErrorAction>,
    remember_choice: bool,
    peers: PeerSearch,
    peer_list: Vec<fc_remote::discovery::Peer>,
    torrents: Torrents,
    notice: Option<String>,
}

impl FcApp {
    pub fn new(cc: &eframe::CreationContext<'_>, startup: Startup) -> Self {
        // Event binding: the worker pokes egui whenever it sends an event, so the
        // UI repaints promptly while transferring and sleeps while idle.
        let ctx = cc.egui_ctx.clone();
        let (queue, events) = Queue::start(QueueConfig {
            policy: ErrorPolicy::Ask,
            speed_limit: startup.limit,
            telemetry_interval: Duration::from_millis(100),
            on_event: Some(Arc::new(move || ctx.request_repaint())),
            ..Default::default()
        });

        let throttle = THROTTLE.iter().position(|(_, l)| *l == startup.limit).unwrap_or(0);
        cc.egui_ctx.set_visuals(accent_visuals());
        let mut app = Self {
            queue,
            events,
            state: AppState::default(),
            form: NewTransfer {
                sources: startup.sources,
                add_path: String::new(),
                dest: startup.dest.map(|d| d.display().to_string()).unwrap_or_default(),
                verify: if startup.verify { VerifyMode::ReadBack } else { VerifyMode::Off },
                overwrite: false,
                remote: RemoteForm::default(),
            },
            view: View::Queue,
            filter: Filter::All,
            search: String::new(),
            show_add: false,
            throttle,
            remembered: HashMap::new(),
            remember_choice: false,
            peers: PeerSearch::default(),
            peer_list: Vec::new(),
            torrents: Torrents::start(),
            notice: None,
        };
        app.state.speed_limit = startup.limit;
        if !app.form.sources.is_empty() && !app.form.dest.is_empty() {
            app.start_transfer();
        }
        app
    }

    /// Drain every pending event without blocking.
    fn pump_events(&mut self) {
        while let Ok(ev) = self.events.try_recv() {
            if let Event::Decision(d) = ev {
                if let Some(a) = self.remembered.get(&d.class) {
                    d.respond(*a);
                    continue;
                }
                self.state.apply(Event::Decision(d));
            } else {
                self.state.apply(ev);
            }
        }
    }

    fn start_transfer(&mut self) {
        let sources = std::mem::take(&mut self.form.sources);
        let (request, target) = if self.form.remote.enabled {
            let backend = match remote::backend_for(&self.form.remote) {
                Ok(b) => b,
                Err(e) => {
                    self.form.sources = sources;
                    self.notice = Some(e);
                    return;
                }
            };
            let target = format!("{}/{}", self.form.remote.address.trim(), self.form.remote.folder.trim().trim_matches('/'));
            self.form.remote.password.clear();
            (TransferRequest::to_backend(sources.clone(), backend), target)
        } else {
            let dest = PathBuf::from(self.form.dest.trim());
            let request = if sources.len() == 1 && sources[0].is_file() && !dest.is_dir() {
                TransferRequest::file_to(&sources[0], &dest)
            } else {
                TransferRequest::into_dir(sources.clone(), &dest)
            };
            (request, dest.display().to_string())
        };
        let options = CopyOptions {
            verify: self.form.verify,
            algorithm: Algorithm::Xxh3,
            overwrite: self.form.overwrite,
            ..Default::default()
        };
        self.notice = None;
        let id = self.queue.add(request.with_options(options));
        self.state.details.insert(id, (sources, target));
        self.show_add = false;
    }

    // ----- top of the window -----

    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("■").color(ACCENT));
            ui.label(RichText::new("fcopy").strong().size(16.0));
            ui.label(RichText::new("v1.0.0").weak().small());
            ui.add_space(12.0);
            if ui.selectable_label(self.view == View::Queue, "Local copy").clicked() {
                self.view = View::Queue;
            }
            if ui.selectable_label(self.view == View::Peers, "Remote PCs").clicked() {
                self.view = View::Peers;
            }
            let running = self.torrents.is_running();
            let tab = ui.add_enabled(running, egui::Button::new("BitTorrent").selected(self.view == View::Torrents).frame(false));
            if running {
                if tab.clicked() {
                    self.view = View::Torrents;
                }
            } else {
                tab.on_disabled_hover_text("The BitTorrent engine could not start. See the BitTorrent tab.");
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.weak("Tokio async engine");
            });
        });
    }

    fn action_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let add = egui::Button::new(RichText::new("+ Add task / files").strong().color(Color32::WHITE)).fill(ACCENT);
            if ui.add(add).clicked() {
                self.show_add = true;
            }
            let running = self.state.active.is_some();
            if self.state.paused {
                if ui.button("▶ Resume all").clicked() {
                    self.queue.resume();
                }
            } else if ui.add_enabled(running, egui::Button::new("⏸ Pause all")).clicked() {
                self.queue.pause();
            }
            if ui.add_enabled(running, egui::Button::new("⏭ Skip file")).on_hover_text("Abandon the current file and continue").clicked() {
                self.queue.skip_current_file();
            }
            if ui.button("Cancel all").clicked() {
                self.queue.cancel_all();
            }
            if ui.button("Clear finished").clicked() {
                self.queue.clear_finished();
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let before = self.throttle;
                egui::ComboBox::from_id_salt("throttle").selected_text(THROTTLE[self.throttle].0).width(130.0).show_ui(ui, |ui| {
                    for (i, (label, _)) in THROTTLE.iter().enumerate() {
                        ui.selectable_value(&mut self.throttle, i, *label);
                    }
                });
                ui.label("Throttle");
                if self.throttle != before {
                    let limit = THROTTLE[self.throttle].1;
                    self.queue.set_speed_limit(limit);
                }
            });
        });
    }

    // ----- sidebar -----

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        let jobs = &self.state.jobs;
        let unfinished = jobs.iter().filter(|r| !matches!(r.state, JobState::Completed | JobState::CompletedWithErrors | JobState::Failed(_) | JobState::Cancelled)).count();
        let done = jobs.iter().filter(|r| matches!(r.state, JobState::Completed | JobState::CompletedWithErrors)).count();

        ui.add_space(6.0);
        ui.label(RichText::new("TRANSFERS").small().color(DIM));
        if ui.selectable_label(self.view == View::Queue && self.filter == Filter::All, format!("Active queue   {unfinished}")).clicked() {
            self.view = View::Queue;
            self.filter = Filter::All;
        }
        if ui.selectable_label(self.view == View::Queue && self.filter == Filter::Done, format!("Completed jobs   {done}")).clicked() {
            self.view = View::Queue;
            self.filter = Filter::Done;
        }
        if ui.selectable_label(self.view == View::Peers, format!("Network peers   {}", self.peer_list.len())).clicked() {
            self.view = View::Peers;
        }

        ui.add_space(14.0);
        ui.label(RichText::new("FEEDS & LOGS").small().color(DIM));
        ui.add_enabled(false, egui::Label::new("Torrent feeds")).on_disabled_hover_text("Not built yet");
        if ui.selectable_label(self.view == View::Logs, format!("System logs   {}", self.state.log.len())).clicked() {
            self.view = View::Logs;
            self.state.unseen = 0;
        }
        ui.add_enabled(false, egui::Label::new("Settings")).on_disabled_hover_text("Not built yet");

        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            let running = self.state.jobs.iter().filter(|r| r.state == JobState::Running).count();
            let failed = self.state.jobs.iter().filter(|r| matches!(r.state, JobState::Failed(_))).count();
            ui.weak(format!("Failed jobs   {failed}"));
            ui.weak(format!("Running jobs   {running}"));
            ui.label(RichText::new("ENGINE").small().color(DIM));
            ui.add_space(8.0);
        });
    }

    // ----- main panel -----

    fn active_card(&self, ui: &mut egui::Ui) {
        let Some(active) = self.state.active.clone() else {
            return;
        };
        let Some(t) = &active.telemetry else {
            ui.weak("Preparing…");
            return;
        };
        let (sources, target) = self.state.details.get(&active.job).cloned().unwrap_or_default();
        let name = sources.first().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let phase = match t.phase {
            Phase::Verifying => "verifying",
            Phase::Finalizing => "finishing",
            Phase::Preparing => "opening",
            _ => "copying",
        };
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("Active transfer").small().color(DIM));
                ui.label(RichText::new(&name).strong());
                ui.weak(format!("→ {target}"));
                if self.state.paused {
                    ui.label(RichText::new(" PAUSED ").strong().color(Color32::BLACK).background_color(WARN));
                }
            });
            ui.add(egui::ProgressBar::new(t.fraction as f32).show_percentage().desired_height(14.0).fill(ACCENT));
            ui.horizontal_wrapped(|ui| {
                let speed = if self.state.paused { "—".to_string() } else { human_rate(t.rate_bps) };
                let eta = if self.state.paused { "paused".into() } else { t.eta.map(human_duration).unwrap_or_else(|| "—".into()) };
                ui.label(format!("{} of {}", human_bytes(t.bytes_copied), human_bytes(t.bytes_total)));
                ui.weak("·");
                ui.label(speed);
                ui.weak("·");
                ui.label(format!("ETA {eta}"));
                ui.weak("·");
                ui.label(format!("{} / {} files, {phase}", t.files_done, t.files_total));
            });
            if let Some(link) = &t.link {
                ui.weak(format!(
                    "Link to {}: round trip {} ms · sent {} · acknowledged {} · {} reconnects{}",
                    link.peer,
                    link.rtt.as_millis(),
                    human_bytes(link.sent_bytes),
                    human_bytes(link.acked_bytes),
                    link.reconnects,
                    if link.remote_paused { " · PC paused" } else { "" }
                ));
            }
        });
    }

    fn filter_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            for (f, label) in Filter::ALL {
                let count = self.state.jobs.iter().filter(|r| f.matches(r)).count();
                let text = format!("{label} ({count})");
                if ui.selectable_label(self.filter == f, text).clicked() {
                    self.filter = f;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("Search transfers").desired_width(200.0));
            });
        });
    }

    fn queue_table(&mut self, ui: &mut egui::Ui) {
        let query = self.search.trim().to_lowercase();
        let rows: Vec<JobRow> = self
            .state
            .jobs
            .iter()
            .filter(|r| self.filter.matches(r))
            .filter(|r| {
                query.is_empty()
                    || self.state.details.get(&r.id).is_some_and(|(s, t)| {
                        t.to_lowercase().contains(&query) || s.iter().any(|p| p.to_string_lossy().to_lowercase().contains(&query))
                    })
            })
            .cloned()
            .collect();

        if rows.is_empty() {
            ui.add_space(12.0);
            ui.weak(if self.state.jobs.is_empty() { "No transfers yet. Use Add task / files to start one." } else { "Nothing matches this filter." });
            return;
        }

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new("queue").num_columns(8).striped(true).spacing([14.0, 8.0]).min_col_width(40.0).show(ui, |ui| {
                for h in ["ST", "FILE / TARGET", "SIZE", "PROGRESS", "SPEED", "FILES", "STATUS", "ACTIONS"] {
                    ui.label(RichText::new(h).small().color(DIM));
                }
                ui.end_row();

                for row in &rows {
                    let (sources, target) = self.state.details.get(&row.id).cloned().unwrap_or_default();
                    let name = match sources.len() {
                        0 => row.label.clone(),
                        1 => sources[0].file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
                        n => format!("{} items", n),
                    };
                    let (icon, color) = status_icon(&row.state, row.paused);
                    ui.colored_label(color, icon);
                    ui.vertical(|ui| {
                        ui.label(RichText::new(&name).strong());
                        ui.label(RichText::new(&target).small().color(DIM));
                    });
                    let size = row.bytes_total;
                    ui.monospace(human_bytes(size));
                    ui.horizontal(|ui| {
                        ui.add(egui::ProgressBar::new(row.fraction as f32).desired_width(140.0).fill(progress_color(&row.state)));
                        ui.monospace(format!("{:.0}%", row.fraction * 100.0));
                    });
                    let rate = self.state.active.as_ref().filter(|a| a.job == row.id).and_then(|a| a.telemetry.as_ref()).map(|t| t.rate_bps);
                    ui.monospace(match (rate, row.state == JobState::Running && !row.paused) {
                        (Some(r), true) => human_rate(r),
                        _ => "—".into(),
                    });
                    ui.monospace(format!("{}/{}", row.files_total.min(row.summary.as_ref().map_or(0, |s| s.files_copied)), row.files_total));
                    ui.label(status_text(&row.state, row.paused));
                    ui.horizontal(|ui| {
                        let held = row.paused || row.state == JobState::Pending;
                        if held && matches!(row.state, JobState::Pending | JobState::Running) {
                            if ui.small_button("▶").on_hover_text("Resume this transfer").clicked() {
                                self.queue.resume_job(row.id);
                            }
                        } else if row.state == JobState::Running && ui.small_button("⏸").on_hover_text("Pause this transfer").clicked() {
                            self.queue.pause_job(row.id);
                        }
                        if matches!(row.state, JobState::Pending | JobState::Running) {
                            if ui.small_button("✖").on_hover_text("Cancel").clicked() {
                                self.queue.cancel(row.id);
                            }
                        } else if ui.small_button("🗑").on_hover_text("Remove from the list").clicked() {
                            self.queue.remove(row.id);
                        }
                    });
                    ui.end_row();
                }
            });
        });
    }

    fn queue_view(&mut self, ui: &mut egui::Ui) {
        self.active_card(ui);
        if self.state.active.is_some() {
            ui.add_space(8.0);
        }
        self.filter_bar(ui);
        ui.separator();
        self.queue_table(ui);
    }

    fn peers_view(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("PCs on this network");
            if ui.add_enabled(!self.peers.busy(), egui::Button::new("Search")).clicked() {
                self.peers.start();
            }
            if self.peers.busy() {
                ui.spinner();
            }
        });
        ui.separator();
        if self.peer_list.is_empty() {
            ui.weak("No PCs found. Make sure their agents are running on this network, then search again.");
            return;
        }
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for peer in self.peer_list.clone() {
                let addr = remote::address_for("", &peer);
                egui::Frame::group(ui.style()).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&peer.name).strong());
                        ui.monospace(&addr);
                        match remote::trusted_fingerprint(&addr) {
                            Some(fp) if fp == peer.fingerprint => ui.colored_label(OK, "trusted"),
                            Some(_) => ui.colored_label(BAD, "FINGERPRINT CHANGED"),
                            None => ui.colored_label(WARN, "not yet trusted"),
                        };
                    });
                    ui.weak(format!("fingerprint {}", peer.fingerprint));
                    if ui.button("Copy to this PC").clicked() {
                        self.form.remote.enabled = true;
                        self.form.remote.address = remote::address_for(&self.form.remote.address, &peer);
                        self.form.remote.fingerprint = peer.fingerprint.clone();
                        self.form.remote.fingerprint_checked = false;
                        self.show_add = true;
                    }
                });
            }
        });
    }

    fn logs_view(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("System logs");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Clear").clicked() {
                    self.state.log.clear();
                }
            });
        });
        ui.separator();
        if self.state.log.is_empty() {
            ui.weak("Nothing to report.");
            return;
        }
        egui::ScrollArea::vertical().auto_shrink([false, false]).stick_to_bottom(false).show(ui, |ui| {
            for e in self.state.log.iter().rev() {
                let (tag, color) = match e.kind {
                    LogKind::Skipped => ("SKIPPED", WARN),
                    LogKind::Failed => ("FAILED ", BAD),
                    LogKind::JobFailed => ("JOB    ", BAD),
                    LogKind::Warning => ("warning", ui.visuals().weak_text_color()),
                };
                ui.horizontal(|ui| {
                    let ago = e.at.elapsed().map(|d| human_duration(d) + " ago").unwrap_or_default();
                    ui.weak(format!("#{}", e.job)).on_hover_text(ago);
                    ui.colored_label(color, RichText::new(tag).monospace());
                    let name = e.path.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned());
                    let short = match &name {
                        Some(n) => format!("{n} — {}", e.message),
                        None => e.message.clone(),
                    };
                    let full = match &e.path {
                        Some(p) => format!("{}\n{}", p.display(), e.message),
                        None => e.message.clone(),
                    };
                    ui.add(egui::Label::new(short).truncate()).on_hover_text(full);
                });
            }
        });
    }

    // ----- add task window -----

    /// The new-transfer form, shown in the "Add task" window.
    fn add_form(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Source");
            let resp = ui.add(egui::TextEdit::singleline(&mut self.form.add_path).hint_text("path, or drop files onto the window").desired_width(ui.available_width() - 60.0));
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.button("Add").clicked() || enter) && !self.form.add_path.trim().is_empty() {
                self.form.sources.push(PathBuf::from(self.form.add_path.trim()));
                self.form.add_path.clear();
            }
        });
        let mut remove = None;
        for (i, s) in self.form.sources.iter().enumerate() {
            ui.horizontal(|ui| {
                if ui.small_button("✖").clicked() {
                    remove = Some(i);
                }
                ui.monospace(s.display().to_string());
            });
        }
        if let Some(i) = remove {
            self.form.sources.remove(i);
        }
        ui.add_space(4.0);
        ui.checkbox(&mut self.form.remote.enabled, "Copy to a PC on the network");
        if self.form.remote.enabled {
            self.remote_fields(ui);
        } else {
            ui.horizontal(|ui| {
                ui.label("Destination");
                ui.add(egui::TextEdit::singleline(&mut self.form.dest).hint_text("folder (or new file name for a single file)").desired_width(f32::INFINITY));
            });
        }
        ui.horizontal(|ui| {
            ui.label("Verify");
            egui::ComboBox::from_id_salt("verify")
                .selected_text(match self.form.verify {
                    VerifyMode::Off => "Off",
                    VerifyMode::SourceHash => "Hash source",
                    VerifyMode::ReadBack => "Read back & compare",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.form.verify, VerifyMode::Off, "Off");
                    ui.selectable_value(&mut self.form.verify, VerifyMode::SourceHash, "Hash source");
                    ui.selectable_value(&mut self.form.verify, VerifyMode::ReadBack, "Read back & compare");
                });
            ui.checkbox(&mut self.form.overwrite, "Overwrite existing");
        });
        if let Some(msg) = &self.notice {
            ui.colored_label(WARN, msg);
        }
        ui.add_space(4.0);
        let ready = !self.form.sources.is_empty() && (self.form.remote.enabled || !self.form.dest.trim().is_empty());
        if ui.add_enabled(ready, egui::Button::new(RichText::new("Start copy").strong())).clicked() {
            self.start_transfer();
        }
    }

    /// The remote destination: a PC from discovery, or an address typed by hand.
    fn remote_fields(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("PC");
            let before = self.peer_list.clone();
            egui::ComboBox::from_id_salt("peers").selected_text("Found on the network…").show_ui(ui, |ui| {
                if before.is_empty() {
                    ui.weak("No PCs found yet. Search, or type the address below.");
                }
                for peer in &before {
                    let addr = remote::address_for("", peer);
                    if ui.selectable_label(false, format!("{}  {addr}", peer.name)).clicked() {
                        self.form.remote.address = remote::address_for(&self.form.remote.address, peer);
                        self.form.remote.fingerprint = peer.fingerprint.clone();
                        self.form.remote.fingerprint_checked = false;
                    }
                }
            });
            if ui.add_enabled(!self.peers.busy(), egui::Button::new("Search")).clicked() {
                self.peers.start();
            }
            if self.peers.busy() {
                ui.spinner();
            }
        });
        ui.horizontal(|ui| {
            ui.label("Address");
            ui.add(egui::TextEdit::singleline(&mut self.form.remote.address).hint_text("user@host:port").desired_width(220.0));
            ui.label("Folder");
            ui.add(egui::TextEdit::singleline(&mut self.form.remote.folder).hint_text("backup/photos").desired_width(f32::INFINITY));
        });
        ui.horizontal(|ui| {
            ui.label("Password");
            ui.add(egui::TextEdit::singleline(&mut self.form.remote.password).password(true).hint_text("used for this job only").desired_width(220.0));
        });
        if let Some(trusted) = remote::trusted_fingerprint(&self.form.remote.address) {
            ui.weak(format!("Trusted: {trusted}"));
        } else if !self.form.remote.fingerprint.is_empty() {
            ui.weak(format!("The PC presents {}", self.form.remote.fingerprint));
            ui.checkbox(&mut self.form.remote.fingerprint_checked, "I checked this matches the PC's own fingerprint");
        } else {
            ui.weak("First time with this PC: pick it from the list, so its fingerprint can be checked.");
        }
    }

    // ----- status bar -----

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.weak("Engine: Tokio async");
            ui.weak("|");
            let copy_rate = self.state.active.as_ref().and_then(|a| a.telemetry.as_ref()).map(|t| t.rate_bps).unwrap_or(0.0);
            ui.label(format!("Copy {}", human_rate(copy_rate)));
            ui.weak("|");
            let remote_running = self.state.jobs.iter().filter(|r| r.state == JobState::Running && self.state.details.get(&r.id).is_some_and(|(_, t)| t.contains('/') && t.contains(':'))).count();
            ui.label(format!("Remote transfers {remote_running}"));
            if self.state.paused {
                ui.colored_label(WARN, "queue paused");
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.selectable_label(self.view == View::Logs, "Ticker log").clicked() {
                    self.view = View::Logs;
                    self.state.unseen = 0;
                }
                if self.state.unseen > 0 {
                    ui.colored_label(WARN, format!("{} new", self.state.unseen));
                }
            });
        });
    }

    fn decision_modal(&mut self, ctx: &egui::Context) {
        let Some(d) = self.state.decisions.front() else { return };
        let mut answer = None;
        egui::Modal::new(egui::Id::new("decision")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.heading(format!("⚠ {}", d.class));
            ui.monospace(d.path.display().to_string());
            let at = d.offset.map(|o| format!(" at byte {o}")).unwrap_or_default();
            ui.label(format!("While {}{at}: {}", d.operation, d.message));
            if d.attempt > 1 {
                ui.weak(format!("Attempt {}", d.attempt));
            }
            ui.add_space(8.0);
            ui.checkbox(&mut self.remember_choice, format!("Do this for every '{}' error (Skip/Cancel)", d.class));
            ui.horizontal(|ui| {
                if ui.button("Retry").clicked() {
                    answer = Some(ErrorAction::Retry);
                }
                if ui.button("Skip file").clicked() {
                    answer = Some(ErrorAction::Skip);
                }
                if ui.button("Cancel transfer").clicked() {
                    answer = Some(ErrorAction::Cancel);
                }
            });
        });
        if let Some(a) = answer {
            let d = self.state.decisions.pop_front().unwrap();
            if self.remember_choice && a != ErrorAction::Retry {
                self.remembered.insert(d.class, a);
            }
            self.remember_choice = false;
            d.respond(a);
        }
    }

    /// Files dropped on the window go into the Add task window.
    fn handle_drops(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        if !dropped.is_empty() {
            self.form.sources.extend(dropped);
            self.show_add = true;
        }
    }
}

fn status_icon(state: &JobState, paused: bool) -> (&'static str, Color32) {
    match state {
        JobState::Pending => ("◌", DIM),
        JobState::Running if paused => ("⏸", WARN),
        JobState::Running => ("●", ACCENT),
        JobState::Completed => ("✔", OK),
        JobState::CompletedWithErrors => ("⚠", WARN),
        JobState::Failed(_) => ("✖", BAD),
        JobState::Cancelled => ("⊘", DIM),
    }
}

fn status_text(state: &JobState, paused: bool) -> RichText {
    let (text, color) = match state {
        JobState::Pending => ("Queued".to_string(), DIM),
        JobState::Running if paused => ("Paused".to_string(), WARN),
        JobState::Running => ("Active".to_string(), ACCENT),
        JobState::Completed => ("Complete".to_string(), OK),
        JobState::CompletedWithErrors => ("Complete with errors".to_string(), WARN),
        JobState::Failed(msg) => (format!("Failed: {msg}"), BAD),
        JobState::Cancelled => ("Cancelled".to_string(), DIM),
    };
    RichText::new(text).color(color)
}

fn progress_color(state: &JobState) -> Color32 {
    match state {
        JobState::Completed => OK,
        JobState::Failed(_) => BAD,
        _ => ACCENT,
    }
}

impl eframe::App for FcApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump_events();
        self.handle_drops(ctx);
        if let Some(found) = self.peers.poll() {
            match found {
                Ok(list) => self.peer_list = list,
                Err(e) => self.notice = Some(format!("discovery failed: {e}")),
            }
        }

        // Panel order matters in egui: edges first, central last.
        egui::TopBottomPanel::top("tabs").show(ctx, |ui| {
            ui.add_space(4.0);
            self.tab_bar(ui);
            ui.add_space(4.0);
        });
        egui::TopBottomPanel::top("actions").show(ctx, |ui| {
            ui.add_space(2.0);
            self.action_bar(ui);
            ui.add_space(4.0);
        });
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.add_space(2.0);
            self.status_bar(ui);
            ui.add_space(2.0);
        });
        egui::SidePanel::left("nav").resizable(false).exact_width(210.0).show(ctx, |ui| self.sidebar(ui));
        self.torrents.refresh();
        egui::CentralPanel::default().show(ctx, |ui| match self.view {
            View::Queue => self.queue_view(ui),
            View::Peers => self.peers_view(ui),
            View::Torrents => torrent::show(ui, &mut self.torrents),
            View::Logs => self.logs_view(ui),
        });

        let mut open = self.show_add;
        egui::Window::new("Add task / files").open(&mut open).resizable(false).default_width(560.0).show(ctx, |ui| self.add_form(ui));
        self.show_add = open;

        self.decision_modal(ctx);

        // Keep "Elapsed" ticking between telemetry events (e.g. at very low speed limits).
        if self.state.active.is_some() && !self.state.paused {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
        // Torrent rates and progress change without events, so the tab repaints on a timer.
        if self.torrents.is_busy() {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Cancel cleanly so no .fcpart files are left behind.
        self.queue.shutdown();
        self.torrents.shutdown();
    }
}

/// Dark visuals with the orange accent on selections instead of the default blue.
fn accent_visuals() -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    v.selection.bg_fill = Color32::from_rgb(120, 48, 10);
    v.selection.stroke = egui::Stroke::new(1.0_f32, ACCENT);
    v.hyperlink_color = ACCENT;
    v
}

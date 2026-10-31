//! Drawing + commands. Reads [`AppState`], sends commands to the [`Queue`].
//!
//! Layout follows the desktop console: a tab bar and action bar on top, a
//! optional navigation sidebar on the left, telemetry monitor on the right, the
//! queue table and progress summary in the middle, and a bottom status bar. New transfers are
//! entered in an "Add task" window.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use eframe::egui::{self, Color32, RichText};
use fc_engine::units::{human_bytes, human_duration, human_rate};
use fc_engine::{
    Algorithm, CopyOptions, ErrorAction, ErrorClass, ErrorPolicy, Event, JobState, Phase, Queue,
    QueueConfig, TransferRequest, VerifyMode,
};

use crate::Startup;
use crate::remote::{self, PeerSearch, RemoteBrowser, RemoteForm};
use crate::state::{AppState, JobRow, LogKind};
use crate::torrent::{self, Torrents};
use fc_engine::conflict::{ConflictPolicy, ConflictAction, ConflictChoice};

pub(crate) fn ok(ui: &egui::Ui) -> Color32 {
    if ui.visuals().dark_mode { Color32::from_rgb(105, 225, 160) } else { Color32::from_rgb(16, 110, 65) }
}
pub(crate) fn warn(ui: &egui::Ui) -> Color32 { ui.visuals().warn_fg_color }
pub(crate) fn bad(ui: &egui::Ui) -> Color32 { ui.visuals().error_fg_color }
pub(crate) fn accent(ui: &egui::Ui) -> Color32 { ui.visuals().hyperlink_color }
pub(crate) fn dim(ui: &egui::Ui) -> Color32 { ui.visuals().weak_text_color() }

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
    History,
    Recovery,
    Peers,
    Torrents,
    Logs,
    Settings,
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
        let done = matches!(
            row.state,
            JobState::Completed | JobState::CompletedWithErrors
        );
        match self {
            Filter::All => true,
            Filter::Active => row.state == JobState::Running && !row.paused,
            Filter::Queued => row.state == JobState::Pending,
            Filter::Paused => row.paused,
            Filter::Done => done,
            Filter::Errors => matches!(
                row.state,
                JobState::Failed(_) | JobState::CompletedWithErrors
            ),
        }
    }
}

/// Form for a new transfer.
struct NewTransfer {
    sources: Vec<PathBuf>,
    add_path: String,
    dest: String,
    verify: VerifyMode,
    conflict_policy: ConflictPolicy,
    remote: RemoteForm,
}

impl NewTransfer {
    fn from_startup(startup: &Startup) -> Self {
        Self {
            sources: startup.sources.clone(),
            add_path: String::new(),
            dest: startup.dest.as_ref().map(|d| d.display().to_string()).unwrap_or_default(),
            verify: if startup.verify { VerifyMode::ReadBack } else { VerifyMode::Off },
            conflict_policy: ConflictPolicy::Ask,
            remote: RemoteForm::default(),
        }
    }
}

pub struct FcApp {
    launch_listener_failed: bool,
    queue: Queue,
    events: Receiver<Event>,
    state: AppState,
    form: NewTransfer,
    view: View,
    filter: Filter,
    search: String,
    show_add: bool,
    /// "Do this for all errors of this kind" answers.
    remembered: HashMap<ErrorClass, ErrorAction>,
    remember_choice: bool,
    conflict_choice: Option<(u64, u64)>,
    conflict_all: bool,
    peers: PeerSearch,
    peer_list: Vec<fc_remote::discovery::Peer>,
    browser: RemoteBrowser,
    torrents: Torrents,
    notice: Option<String>,
    vpn: crate::vpn::VpnSettings,
    profiles: crate::profiles::Profiles,
    history: crate::history::History,
    recovery: crate::recovery::Recovery,
    appearance: crate::appearance::Appearance,
    rates: std::collections::VecDeque<f32>,
    last_rate: std::time::Instant,
    instance: crate::instance::Instance,
}

impl FcApp {
    pub fn new(cc: &eframe::CreationContext<'_>, startup: Startup, instance: crate::instance::Instance) -> std::io::Result<Self> {
        instance.attach(cc)?;
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

        let mut app = Self {
            instance,
            launch_listener_failed: false,
            queue,
            events,
            state: AppState::default(),
            form: NewTransfer::from_startup(&startup),
            view: View::Queue,
            filter: Filter::All,
            search: String::new(),
            show_add: !startup.sources.is_empty(),
            remembered: HashMap::new(),
            remember_choice: false,
            conflict_choice: None,
            conflict_all: false,
            peers: PeerSearch::default(),
            peer_list: Vec::new(),
            browser: RemoteBrowser::default(),
            torrents: Torrents::start(startup.torrent_dht, startup.torrent_trackers),
            notice: None,
            vpn: crate::vpn::VpnSettings::default(),
            profiles: crate::profiles::Profiles::default(),
            history: crate::history::History::new(cc.egui_ctx.clone()),
            recovery: crate::recovery::Recovery::new(cc.egui_ctx.clone()),
            appearance: crate::appearance::Appearance::default(),
            rates: std::collections::VecDeque::new(),
            last_rate: std::time::Instant::now(),
        };
        app.state.speed_limit = startup.limit;
        if !app.form.sources.is_empty() && !app.form.dest.is_empty() {
            app.start_transfer();
        }
        Ok(app)
    }

    fn pump_launches(&mut self) {
        if self.show_add || self.recovery.is_busy() || self.launch_listener_failed { return; }
        loop {
            let startup = match self.instance.try_recv() {
                Ok(Some(startup)) => startup,
                Ok(None) => return,
                Err(error) => {
                    self.launch_listener_failed = true;
                    self.notice = Some(error);
                    self.show_add = true;
                    return;
                }
            };
            if let Some(limit) = startup.limit {
                self.queue.set_speed_limit(Some(limit));
                self.state.speed_limit = Some(limit);
            }
            if startup.sources.is_empty() {
                if startup.verify { self.form.verify = VerifyMode::ReadBack; }
                if startup.torrent_dht || startup.torrent_trackers {
                    self.notice = Some("Torrent network options are startup-only. Close FCOPY before changing them.".into());
                    self.show_add = true;
                    return;
                }
                continue;
            }
            self.form = NewTransfer::from_startup(&startup);
            self.browser = RemoteBrowser::default();
            self.view = View::Queue;
            self.notice = if startup.torrent_dht || startup.torrent_trackers {
                Some("Files received. Torrent network options apply only when starting the first window.".into())
            } else { None };
            self.show_add = true;
            if startup.dest.is_some() { self.start_transfer(); }
            return;
        }
    }

    /// Drain every pending event without blocking.
    fn pump_events(&mut self) {
        while let Ok(ev) = self.events.try_recv() {
            self.history.observe(&ev);
            self.recovery.observe(&ev);
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
        if self.recovery.is_busy() { self.notice = Some("Wait for the current job to be saved before adding another.".into()); return; }
        let sources = std::mem::take(&mut self.form.sources);
        let (request, destination) = if self.form.remote.enabled {
            let backend = match remote::backend_for(&self.form.remote) {
                Ok(b) => b,
                Err(e) => {
                    self.form.sources = sources;
                    self.notice = Some(e);
                    return;
                }
            };
            self.form.remote.password.clear();
            if self.form.remote.download {
                let dest = match fc_remote::recovery::absolute(std::path::Path::new(self.form.dest.trim())) {
                    Ok(dest) => dest,
                    Err(error) => { self.notice = Some(error.to_string()); self.form.sources = sources; return; }
                };
                let destination = match backend.recovery_destination(Some(dest.clone())) {
                    Ok(destination) => destination,
                    Err(error) => { self.notice = Some(error.to_string()); self.form.sources = sources; return; }
                };
                let backend = Arc::new(fc_remote::backend::PullBackend::new(backend, dest));
                (TransferRequest::to_backend(sources.clone(), backend), destination)
            } else {
                let destination = match backend.recovery_destination(None) {
                    Ok(destination) => destination,
                    Err(error) => { self.notice = Some(error.to_string()); self.form.sources = sources; return; }
                };
                (TransferRequest::to_backend(sources.clone(), backend), destination)
            }
        } else {
            let dest = PathBuf::from(self.form.dest.trim());
            let request = if sources.len() == 1 && sources[0].is_file() && !dest.is_dir() {
                TransferRequest::file_to(&sources[0], &dest)
            } else {
                TransferRequest::into_dir(sources.clone(), &dest)
            };
            let destination = match &request.target {
                fc_engine::Target::FileTo(path) => fc_remote::recovery::Destination::File { path: path.clone() },
                _ => fc_remote::recovery::Destination::Directory { path: dest },
            };
            (request, destination)
        };
        let options = CopyOptions {
            verify: self.form.verify,
            algorithm: Algorithm::Xxh3,
            ..Default::default()
        };
        self.notice = None;
        let mut request = request.with_options(options).with_conflict_policy(self.form.conflict_policy);
        if !self.form.remote.enabled || !self.form.remote.download {
            let absolute: Result<Vec<_>, _> = request.sources.iter().map(|p| fc_remote::recovery::absolute(p)).collect();
            match absolute {
                Ok(paths) => request.sources = paths,
                Err(error) => { self.notice = Some(error.to_string()); self.form.sources = sources; return; }
            }
        }
        let destination = match destination {
            fc_remote::recovery::Destination::File { path } => fc_remote::recovery::absolute(&path).map(|path| fc_remote::recovery::Destination::File { path }),
            fc_remote::recovery::Destination::Directory { path } => fc_remote::recovery::absolute(&path).map(|path| fc_remote::recovery::Destination::Directory { path }),
            other => Ok(other),
        };
        let job = destination.and_then(|dest| {
            match &dest {
                fc_remote::recovery::Destination::File { path } => request.target = fc_engine::Target::FileTo(path.clone()),
                fc_remote::recovery::Destination::Directory { path } => request.target = fc_engine::Target::IntoDir(path.clone()),
                _ => {}
            }
            fc_remote::recovery::Job::new(&request, dest, None, &self.queue.copy_options())
        });
        match job {
            Ok(job) => self.recovery.save(job, request),
            Err(error) => { self.notice = Some(format!("Job was not started: {error}")); self.form.sources = sources; return; }
        }
        self.form.sources = sources;
        self.show_add = false;
    }

    // ----- top of the window -----

    fn tab_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("■").color(accent(ui)));
            ui.label(RichText::new("fcopy").strong().size(16.0));
            ui.label(RichText::new("v1.0.0").weak().small());
            ui.add_space(12.0);
            if ui
                .selectable_label(self.view == View::Queue, "Local copy")
                .clicked()
            {
                self.view = View::Queue;
            }
            if ui
                .selectable_label(self.view == View::Peers, "Remote PCs")
                .clicked()
            {
                self.view = View::Peers;
            }
            if ui.selectable_label(self.view == View::History, "History").clicked() { self.view = View::History; }
            if ui.selectable_label(self.view == View::Recovery, "Recovery").clicked() { self.view = View::Recovery; }
            let running = self.torrents.is_running();
            let tab = ui.add_enabled(
                running,
                egui::Button::new("BitTorrent")
                    .selected(self.view == View::Torrents)
                    .frame(false),
            );
            if running {
                if tab.clicked() {
                    self.view = View::Torrents;
                }
            } else {
                tab.on_disabled_hover_text(
                    "The BitTorrent engine could not start. See the BitTorrent tab.",
                );
            }
            if ui.selectable_label(self.view == View::Logs, "Logs").clicked() {
                self.view = View::Logs;
                self.state.unseen = 0;
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.selectable_label(self.view == View::Settings, "Settings").clicked() {
                    self.view = View::Settings;
                }
            });
        });
    }

    fn action_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            let add = egui::Button::new(
                RichText::new("+ Add task / files")
                    .strong()
                    .color(Color32::WHITE),
            )
            .fill(Color32::from_rgb(156, 51, 6));
            if ui.add(add).clicked() {
                self.show_add = true;
            }
            let running = self.state.active.is_some();
            if self.state.paused {
                if ui.button("▶ Resume all").clicked() {
                    self.queue.resume();
                }
            } else if ui
                .add_enabled(running, egui::Button::new("⏸ Pause all"))
                .clicked()
            {
                self.queue.pause();
            }
            if ui
                .add_enabled(running, egui::Button::new("⏭ Skip file"))
                .on_hover_text("Abandon the current file and continue")
                .clicked()
            {
                self.queue.skip_current_file();
            }
            if ui.button("Cancel all").clicked() {
                self.queue.cancel_all();
            }
            if ui.button("Clear finished").clicked() {
                self.queue.clear_finished();
            }
        });
    }

    // ----- sidebar -----

    fn sidebar(&mut self, ui: &mut egui::Ui) {
        let jobs = &self.state.jobs;
        let unfinished = jobs
            .iter()
            .filter(|r| {
                !matches!(
                    r.state,
                    JobState::Completed
                        | JobState::CompletedWithErrors
                        | JobState::Failed(_)
                        | JobState::Cancelled
                )
            })
            .count();
        let done = jobs
            .iter()
            .filter(|r| matches!(r.state, JobState::Completed | JobState::CompletedWithErrors))
            .count();

        ui.add_space(6.0);
        ui.label(RichText::new("TRANSFERS").small().color(dim(ui)));
        if ui
            .selectable_label(
                self.view == View::Queue && self.filter == Filter::All,
                format!("Active queue   {unfinished}"),
            )
            .clicked()
        {
            self.view = View::Queue;
            self.filter = Filter::All;
        }
        if ui
            .selectable_label(
                self.view == View::Queue && self.filter == Filter::Done,
                format!("Completed jobs   {done}"),
            )
            .clicked()
        {
            self.view = View::Queue;
            self.filter = Filter::Done;
        }
        if ui
            .selectable_label(
                self.view == View::Peers,
                format!("Network peers   {}", self.peer_list.len()),
            )
            .clicked()
        {
            self.view = View::Peers;
        }

        ui.add_space(14.0);
        if ui.selectable_label(self.view == View::History, "Transfer history").clicked() {
            self.view = View::History;
        }
        if ui.selectable_label(self.view == View::Recovery, "Queue recovery").clicked() { self.view = View::Recovery; }
        ui.label(RichText::new("LOGS & SETTINGS").small().color(dim(ui)));
        if ui
            .selectable_label(
                self.view == View::Logs,
                format!("System logs   {}", self.state.log.len()),
            )
            .clicked()
        {
            self.view = View::Logs;
            self.state.unseen = 0;
        }
        if ui.selectable_label(self.view == View::Settings, "Settings / appearance").clicked() { self.view = View::Settings; }

        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            let running = self
                .state
                .jobs
                .iter()
                .filter(|r| r.state == JobState::Running)
                .count();
            let failed = self
                .state
                .jobs
                .iter()
                .filter(|r| matches!(r.state, JobState::Failed(_)))
                .count();
            ui.weak(format!("Failed jobs   {failed}"));
            ui.weak(format!("Running jobs   {running}"));
            ui.label(RichText::new("ENGINE").small().color(dim(ui)));
            ui.add_space(8.0);
        });
    }

    // ----- main panel -----

    fn bandwidth(&mut self, ui: &mut egui::Ui) {
        let before = self.state.speed_limit;
        let mut limit = before;
        egui::ComboBox::from_id_salt("throttle")
            .selected_text(limit.map(|value| human_rate(value as f64)).unwrap_or_else(|| "Unlimited".into()))
            .show_ui(ui, |ui| {
                for (label, value) in THROTTLE { ui.selectable_value(&mut limit, value, label); }
            });
        if limit != before {
            self.queue.set_speed_limit(limit);
            self.state.speed_limit = limit;
        }
    }

    fn active_paused(&self) -> bool {
        self.state.paused || self.state.active.as_ref().is_some_and(|active| {
            self.state.jobs.iter().any(|job| job.id == active.job && job.paused)
        })
    }

    fn current_rate(&self) -> f64 {
        if self.active_paused() { 0.0 } else {
            self.state.active.as_ref().and_then(|a| a.telemetry.as_ref()).map_or(0.0, |t| t.rate_bps)
        }
    }

    fn monitor(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.heading("Transfer monitor");
            ui.add_space(8.0);
            ui.label("Global bandwidth cap");
            self.bandwidth(ui);
            ui.separator();
            let rate = self.current_rate();
            ui.label(RichText::new(human_rate(rate)).size(26.0).strong().color(accent(ui)));
            ui.weak("Copy throughput - recent samples");
            let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 110.0), egui::Sense::hover());
            let painter = ui.painter_at(rect);
            for step in 1..4 {
                let y = rect.top() + rect.height() * step as f32 / 4.0;
                painter.line_segment([egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)], egui::Stroke::new(1.0_f32, ui.visuals().widgets.noninteractive.bg_stroke.color));
            }
            let max = self.rates.iter().copied().fold(1.0_f32, f32::max);
            if self.rates.len() > 1 {
                let points = self.rates.iter().enumerate().map(|(i, value)| {
                    egui::pos2(rect.left() + rect.width() * i as f32 / 119.0, rect.bottom() - rect.height() * value / max)
                }).collect();
                painter.add(egui::Shape::line(points, egui::Stroke::new(2.0_f32, accent(ui))));
            }
            ui.weak(format!("Scale: {} peak", human_rate(f64::from(max))));
            ui.separator();
            ui.label(format!("Queued: {}", self.state.jobs.iter().filter(|j| j.state == JobState::Pending).count()));
            ui.label(format!("Finished: {}", self.state.jobs.iter().filter(|j| j.state.is_finished()).count()));
            if let Some(t) = self.state.active.as_ref().and_then(|a| a.telemetry.as_ref()) {
                ui.label(format!("ETA: {}", t.eta.map(human_duration).unwrap_or_else(|| "--".into())));
                if let Some(link) = &t.link {
                    ui.separator();
                    ui.strong("Remote link");
                    ui.label(format!("RTT: {} ms", link.rtt.as_millis()));
                    ui.label(format!("Reconnects: {}", link.reconnects));
                    if link.remote_paused { ui.label("Paused by remote PC"); }
                }
            }
            ui.separator();
            ui.weak("Torrent activity stays in its own tab and is not included in copy progress.");
            if ui.button("Appearance / settings").clicked() { self.view = View::Settings; }
        });
    }

    fn queue_summary(&self, ui: &mut egui::Ui) {
        let planned: u64 = self.state.jobs.iter().map(|j| j.bytes_total).sum();
        let processed: f64 = self.state.jobs.iter().map(|j| j.bytes_total as f64 * j.fraction.clamp(0.0, 1.0)).sum();
        let fraction = if planned > 0 { (processed / planned as f64) as f32 } else { 0.0 };
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(format!("{:.0}%", fraction * 100.0)).size(34.0).strong().color(accent(ui)));
                ui.vertical(|ui| {
                    ui.strong("Copy queue");
                    ui.weak(format!("{} jobs - planned bytes only", self.state.jobs.len()));
                });
            });
            ui.add(egui::ProgressBar::new(fraction).desired_width(ui.available_width()).desired_height(8.0).fill(accent(ui)));
            ui.label(format!("Processed {} / {} planned", human_bytes(processed as u64), human_bytes(planned)));
            if self.state.jobs.iter().any(|j| j.state == JobState::Pending) { ui.weak("Queued jobs are measured when they start; totals may grow."); }
        });
    }

    fn active_card(&self, ui: &mut egui::Ui) {
        let Some(active) = self.state.active.clone() else {
            return;
        };
        let Some(t) = &active.telemetry else {
            ui.weak("Preparing…");
            return;
        };
        let (sources, target) = self
            .state
            .details
            .get(&active.job)
            .cloned()
            .unwrap_or_default();
        let name = sources
            .first()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let phase = match t.phase {
            Phase::Verifying => "verifying",
            Phase::Finalizing => "finishing",
            Phase::Preparing => "opening",
            _ => "copying",
        };
        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new("Active transfer").small().color(dim(ui)));
                ui.label(RichText::new(&name).strong());
                ui.weak(format!("→ {target}"));
                if self.active_paused() {
                    ui.label(
                        RichText::new(" PAUSED ")
                            .strong()
                            .color(warn(ui)),
                    );
                }
            });
            ui.add(
                egui::ProgressBar::new(t.fraction as f32)
                    .show_percentage()
                    .desired_height(14.0)
                    .fill(accent(ui)),
            );
            ui.horizontal_wrapped(|ui| {
                let speed = if self.active_paused() {
                    "—".to_string()
                } else {
                    human_rate(t.rate_bps)
                };
                let eta = if self.state.paused {
                    "paused".into()
                } else {
                    t.eta.map(human_duration).unwrap_or_else(|| "—".into())
                };
                ui.label(format!(
                    "{} of {}",
                    human_bytes(t.bytes_copied),
                    human_bytes(t.bytes_total)
                ));
                ui.weak("·");
                ui.label(speed);
                ui.weak("·");
                ui.label(format!("ETA {eta}"));
                ui.weak("·");
                ui.label(format!(
                    "{} / {} files, {phase}",
                    t.files_done, t.files_total
                ));
            });
            if let Some(link) = &t.link {
                let bytes = match link.received_bytes {
                    Some(n) => format!("received {}", human_bytes(n)),
                    None => format!("sent {} · acknowledged {}", human_bytes(link.sent_bytes), human_bytes(link.acked_bytes)),
                };
                ui.weak(format!(
                    "Link to {}: round trip {} ms · {} · {} reconnects{}",
                    link.peer,
                    link.rtt.as_millis(),
                    bytes,
                    link.reconnects,
                    if link.remote_paused {
                        " · PC paused"
                    } else {
                        ""
                    }
                ));
            }
        });
    }

    fn filter_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            for (f, label) in Filter::ALL {
                let count = self.state.jobs.iter().filter(|r| f.matches(r)).count();
                let text = format!("{label} ({count})");
                if ui.selectable_label(self.filter == f, text).clicked() {
                    self.filter = f;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add(
                    egui::TextEdit::singleline(&mut self.search)
                        .hint_text("Search transfers")
                        .desired_width(200.0),
                );
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
                        t.to_lowercase().contains(&query)
                            || s.iter()
                                .any(|p| p.to_string_lossy().to_lowercase().contains(&query))
                    })
            })
            .cloned()
            .collect();

        if rows.is_empty() {
            ui.add_space(12.0);
            ui.weak(if self.state.jobs.is_empty() {
                "No transfers yet. Use Add task / files to start one."
            } else {
                "Nothing matches this filter."
            });
            return;
        }

        egui::ScrollArea::both()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                egui::Grid::new("queue")
                    .num_columns(8)
                    .striped(true)
                    .spacing([14.0, 8.0])
                    .min_col_width(40.0)
                    .show(ui, |ui| {
                        for h in [
                            "ST",
                            "FILE / TARGET",
                            "SIZE",
                            "PROGRESS",
                            "SPEED",
                            "FILES",
                            "STATUS",
                            "ACTIONS",
                        ] {
                            ui.label(RichText::new(h).small().color(dim(ui)));
                        }
                        ui.end_row();

                        for row in &rows {
                            let (sources, target) =
                                self.state.details.get(&row.id).cloned().unwrap_or_default();
                            let name = match sources.len() {
                                0 => row.label.clone(),
                                1 => sources[0]
                                    .file_name()
                                    .map(|n| n.to_string_lossy().into_owned())
                                    .unwrap_or_default(),
                                n => format!("{} items", n),
                            };
                            let (icon, color) = status_icon(ui, &row.state, row.paused);
                            ui.colored_label(color, icon);
                            ui.vertical(|ui| {
                                ui.label(RichText::new(&name).strong());
                                ui.label(RichText::new(&target).small().color(dim(ui)));
                            });
                            let size = row.bytes_total;
                            ui.monospace(human_bytes(size));
                            ui.horizontal(|ui| {
                                ui.add(
                                    egui::ProgressBar::new(row.fraction as f32)
                                        .desired_width(140.0)
                                        .fill(progress_color(ui, &row.state)),
                                );
                                ui.monospace(format!("{:.0}%", row.fraction * 100.0));
                            });
                            let rate = self
                                .state
                                .active
                                .as_ref()
                                .filter(|a| a.job == row.id)
                                .and_then(|a| a.telemetry.as_ref())
                                .map(|t| t.rate_bps);
                            ui.monospace(
                                match (rate, row.state == JobState::Running && !row.paused) {
                                    (Some(r), true) => human_rate(r),
                                    _ => "—".into(),
                                },
                            );
                            ui.monospace(format!(
                                "{}/{}",
                                row.files_total
                                    .min(row.summary.as_ref().map_or(0, |s| s.files_copied)),
                                row.files_total
                            ));
                            ui.label(status_text(ui, &row.state, row.paused));
                            ui.horizontal(|ui| {
                                let held = row.paused || row.state == JobState::Pending;
                                if held
                                    && matches!(row.state, JobState::Pending | JobState::Running)
                                {
                                    if ui
                                        .small_button("▶")
                                        .on_hover_text("Resume this transfer")
                                        .clicked()
                                    {
                                        self.queue.resume_job(row.id);
                                    }
                                } else if row.state == JobState::Running
                                    && ui
                                        .small_button("⏸")
                                        .on_hover_text("Pause this transfer")
                                        .clicked()
                                {
                                    self.queue.pause_job(row.id);
                                }
                                if matches!(row.state, JobState::Pending | JobState::Running) {
                                    if ui.small_button("✖").on_hover_text("Cancel").clicked() {
                                        self.queue.cancel(row.id);
                                    }
                                } else if ui
                                    .small_button("🗑")
                                    .on_hover_text("Remove from the list")
                                    .clicked()
                                {
                                    self.queue.remove(row.id);
                                }
                            });
                            ui.end_row();
                        }
                    });
            });
    }

    fn queue_view(&mut self, ui: &mut egui::Ui) {
        self.queue_summary(ui);
        ui.add_space(6.0);
        self.active_card(ui);
        if self.state.active.is_some() {
            ui.add_space(8.0);
        }
        self.filter_bar(ui);
        if ui.ctx().screen_rect().width() < 1100.0 {
            ui.horizontal_wrapped(|ui| { ui.label("Bandwidth cap"); self.bandwidth(ui); });
        }
        ui.separator();
        self.queue_table(ui);
    }

    fn select_profile(&mut self, profile: fc_remote::profiles::Profile, download: bool) {
        match RemoteForm::from_profile(profile, download) {
            Ok(form) => {
                if download {
                    self.form.sources = vec![PathBuf::from(&form.folder)];
                } else if self.form.remote.download {
                    self.form.sources.clear();
                }
                self.form.add_path.clear();
                self.form.remote = form;
                self.browser = RemoteBrowser::default();
                self.notice = None;
                self.show_add = true;
            }
            Err(e) => self.notice = Some(e),
        }
    }

    fn edit_profile(&mut self) {
        self.profiles.edit_form(&self.form.remote);
        self.form.remote.password.clear();
        self.browser = RemoteBrowser::default();
        self.show_add = false;
        self.view = View::Peers;
    }

    fn peers_view(&mut self, ui: &mut egui::Ui) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            if let Some((profile, download)) = self.profiles.show(ui) {
                self.select_profile(profile, download);
            }
            ui.separator();
            self.discovered_peers(ui);
        });
    }

    fn discovered_peers(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("PCs on this network");
            if ui
                .add_enabled(!self.peers.busy(), egui::Button::new("Search"))
                .clicked()
            {
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
        for peer in self.peer_list.clone() {
            let addr = remote::address_for("", &peer);
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&peer.name).strong());
                    ui.monospace(&addr);
                    match remote::trusted_fingerprint(&addr) {
                        Some(fp) if fp == peer.fingerprint => ui.colored_label(ok(ui), "trusted"),
                        Some(_) => ui.colored_label(bad(ui), "FINGERPRINT CHANGED"),
                        None => ui.colored_label(warn(ui), "not yet trusted"),
                    };
                });
                ui.weak(format!("fingerprint {}", peer.fingerprint));
                if ui.button("Copy to this PC").clicked() {
                    self.form.remote.clear_identity();
                    if self.form.remote.download { self.form.sources.clear(); }
                    self.form.remote.download = false;
                    self.browser = RemoteBrowser::default();
                    self.form.remote.enabled = true;
                    self.form.remote.address = remote::address_for(&self.form.remote.address, &peer);
                    self.form.remote.fingerprint = peer.fingerprint.clone();
                    self.show_add = true;
                }
                if ui.add_enabled(self.profiles.available(), egui::Button::new("Save PC profile")).clicked() {
                    let form = RemoteForm {
                        enabled: true,
                        address: remote::address_for(&self.form.remote.address, &peer),
                        fingerprint: peer.fingerprint.clone(),
                        ..Default::default()
                    };
                    self.profiles.edit_form(&form);
                }
            });
        }
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
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .stick_to_bottom(false)
            .show(ui, |ui| {
                for e in self.state.log.iter().rev() {
                    let (tag, color) = match e.kind {
                        LogKind::Skipped => ("SKIPPED", warn(ui)),
                        LogKind::Failed => ("FAILED ", bad(ui)),
                        LogKind::JobFailed => ("JOB    ", bad(ui)),
                        LogKind::Warning => ("warning", ui.visuals().weak_text_color()),
                    };
                    ui.horizontal(|ui| {
                        let ago =
                            e.at.elapsed()
                                .map(|d| human_duration(d) + " ago")
                                .unwrap_or_default();
                        ui.weak(format!("#{}", e.job)).on_hover_text(ago);
                        ui.colored_label(color, RichText::new(tag).monospace());
                        let name = e
                            .path
                            .as_ref()
                            .and_then(|p| p.file_name())
                            .map(|n| n.to_string_lossy().into_owned());
                        let short = match &name {
                            Some(n) => format!("{n} — {}", e.message),
                            None => e.message.clone(),
                        };
                        let full = match &e.path {
                            Some(p) => format!("{}\n{}", p.display(), e.message),
                            None => e.message.clone(),
                        };
                        ui.add(egui::Label::new(short).truncate())
                            .on_hover_text(full);
                    });
                }
            });
    }

    // ----- add task window -----

    /// The new-transfer form, shown in the "Add task" window.
    fn add_form(&mut self, ui: &mut egui::Ui) {
        let old_mode = (self.form.remote.enabled, self.form.remote.download);
        ui.horizontal(|ui| {
            if ui.selectable_label(!self.form.remote.enabled, "Local copy").clicked() {
                self.form.remote.enabled = false;
                self.form.remote.download = false;
            }
            if ui.selectable_label(self.form.remote.enabled && !self.form.remote.download, "Upload to PC").clicked() {
                self.form.remote.enabled = true;
                self.form.remote.download = false;
            }
            if ui.selectable_label(self.form.remote.enabled && self.form.remote.download, "Download from PC").clicked() {
                self.form.remote.enabled = true;
                self.form.remote.download = true;
            }
        });
        if old_mode != (self.form.remote.enabled, self.form.remote.download) {
            self.form.sources.clear();
            self.form.add_path.clear();
            self.browser = RemoteBrowser::default();
            if self.form.remote.download && self.form.remote.profile.is_some() {
                self.form.sources.push(PathBuf::from(&self.form.remote.folder));
            }
        }
        ui.horizontal(|ui| {
            ui.label("Source");
            let resp = ui.add(
                egui::TextEdit::singleline(&mut self.form.add_path)
                    .hint_text(if self.form.remote.download { "share/path, or select below" } else { "path, or drop files onto the window" })
                    .desired_width(ui.available_width() - 60.0),
            );
            let enter = resp.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.button("Add").clicked() || enter) && !self.form.add_path.trim().is_empty() {
                self.form
                    .sources
                    .push(PathBuf::from(self.form.add_path.trim()));
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
        if self.form.remote.enabled {
            let address = self.form.remote.address.clone();
            self.remote_fields(ui);
            if self.form.remote.address != address {
                self.browser = RemoteBrowser::default();
                if self.form.remote.download && self.form.remote.profile.is_none() { self.form.sources.clear(); }
            }
            if self.form.remote.download {
                self.remote_browser(ui);
            }
        }
        if !self.form.remote.enabled || self.form.remote.download {
            ui.horizontal(|ui| {
                ui.label("Destination");
                ui.add(
                    egui::TextEdit::singleline(&mut self.form.dest)
                        .hint_text(if self.form.remote.download { "local destination folder" } else { "folder (or new file name for a single file)" })
                        .desired_width(f32::INFINITY),
                );
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
                    ui.selectable_value(
                        &mut self.form.verify,
                        VerifyMode::SourceHash,
                        "Hash source",
                    );
                    ui.selectable_value(
                        &mut self.form.verify,
                        VerifyMode::ReadBack,
                        "Read back & compare",
                    );
                });
            egui::ComboBox::from_label("Existing files")
                .selected_text(format!("{:?}", self.form.conflict_policy))
                .show_ui(ui, |ui| {
                    for (policy, label) in [
                        (ConflictPolicy::Ask, "Ask"), (ConflictPolicy::Replace, "Replace"),
                        (ConflictPolicy::Skip, "Skip"), (ConflictPolicy::KeepBoth, "Keep both"),
                        (ConflictPolicy::Compare, "Compare; ask if different"),
                    ] { ui.selectable_value(&mut self.form.conflict_policy, policy, label); }
                });
        });
        if let Some(msg) = &self.notice {
            ui.colored_label(warn(ui), msg);
        }
        ui.add_space(4.0);
        let ready = !self.form.sources.is_empty()
            && ((self.form.remote.enabled && !self.form.remote.download) || !self.form.dest.trim().is_empty());
        if ui
            .add_enabled(
                ready,
                egui::Button::new(RichText::new("Start copy").strong()),
            )
            .clicked()
        {
            self.start_transfer();
        }
    }

    fn remote_browser(&mut self, ui: &mut egui::Ui) {
        self.browser.poll();
        let mut navigate = None;
        ui.horizontal(|ui| {
            if ui.add_enabled(!self.browser.busy(), egui::Button::new("Browse shares")).clicked() {
                navigate = Some(String::new());
            }
            if !self.browser.path.is_empty() && ui.add_enabled(!self.browser.busy(), egui::Button::new("Up")).clicked() {
                navigate = Some(self.browser.path.rsplit_once('/').map(|(p, _)| p).unwrap_or("").into());
            }
            ui.monospace(if self.browser.path.is_empty() { "Shares" } else { &self.browser.path });
            if self.browser.busy() { ui.spinner(); }
        });
        if let Some(error) = &self.browser.error { ui.colored_label(warn(ui), error); }
        let mut selected = None;
        egui::ScrollArea::vertical().id_salt("remote-browser").max_height(160.0).show(ui, |ui| {
            for entry in &self.browser.entries {
                let path = if self.browser.path.is_empty() { entry.name.clone() } else { format!("{}/{}", self.browser.path, entry.name) };
                ui.horizontal(|ui| {
                    if ui.add_enabled(!self.browser.busy(), egui::Button::new("Add")).clicked() {
                        selected = Some(PathBuf::from(&path));
                    }
                    if entry.kind == fc_remote::protocol::EntryKind::Dir {
                        if ui.add_enabled(!self.browser.busy(), egui::Button::new(format!("{}/", entry.name))).clicked() {
                            navigate = Some(path);
                        }
                    } else {
                        ui.label(&entry.name);
                        ui.weak(human_bytes(entry.size));
                    }
                });
            }
        });
        if let Some(path) = selected {
            if !self.form.sources.contains(&path) { self.form.sources.push(path); }
        }
        if let Some(path) = navigate {
            self.browser.start(&self.form.remote, path, ui.ctx());
        }
    }

    /// The remote destination: a PC from discovery, or an address typed by hand.
    fn remote_fields(&mut self, ui: &mut egui::Ui) {
        let current = self.form.remote.profile.clone();
        let label = current.as_ref().map(|p| p.name.as_str()).unwrap_or("Manual address");
        let mut selected = None;
        let mut manual = false;
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("saved-pc")
                .selected_text(label)
                .show_ui(ui, |ui| {
                    manual = ui.selectable_label(current.is_none(), "Manual address").clicked();
                    for profile in self.profiles.list() {
                        if ui.selectable_label(current.as_ref().is_some_and(|p| p.id == profile.id), format!("{} - {}", profile.name, profile.target)).clicked() {
                            selected = Some(profile.clone());
                        }
                    }
                });
            if ui.add_enabled(self.profiles.available(), egui::Button::new(if current.is_some() { "Edit profile" } else { "Save as profile" })).clicked() {
                self.edit_profile();
            }
        });
        if manual {
            self.form.remote.clear_identity();
            self.browser = RemoteBrowser::default();
        }
        if let Some(profile) = selected { self.select_profile(profile, self.form.remote.download); }
        ui.horizontal(|ui| {
            ui.label("PC");
            let before = self.peer_list.clone();
            egui::ComboBox::from_id_salt("peers")
                .selected_text("Found on the network…")
                .show_ui(ui, |ui| {
                    if before.is_empty() {
                        ui.weak("No PCs found yet. Search, or type the address below.");
                    }
                    for peer in &before {
                        let addr = remote::address_for("", peer);
                        if ui
                            .selectable_label(false, format!("{}  {addr}", peer.name))
                            .clicked()
                        {
                            self.form.remote.clear_identity();
                            self.form.remote.address =
                                remote::address_for(&self.form.remote.address, peer);
                            self.form.remote.fingerprint = peer.fingerprint.clone();
                            self.form.remote.fingerprint_checked = false;
                        }
                    }
                });
            if ui
                .add_enabled(!self.peers.busy(), egui::Button::new("Search"))
                .clicked()
            {
                self.peers.start();
            }
            if self.peers.busy() {
                ui.spinner();
            }
        });
        ui.horizontal(|ui| {
            ui.label("Address");
            let changed = ui.add(
                egui::TextEdit::singleline(&mut self.form.remote.address)
                    .hint_text("user@host:port")
                    .desired_width(220.0),
            ).changed();
            if changed { self.form.remote.clear_identity(); }
            if !self.form.remote.download {
                ui.label("Folder");
                ui.add(
                    egui::TextEdit::singleline(&mut self.form.remote.folder)
                        .hint_text("backup/photos")
                        .desired_width(f32::INFINITY),
                );
            }
        });
        ui.horizontal(|ui| {
            ui.label("Password");
            ui.add(
                egui::TextEdit::singleline(&mut self.form.remote.password)
                    .password(true)
                    .hint_text("browse and job only; not saved")
                    .desired_width(220.0),
            );
        });
        if let Some(profile) = &self.form.remote.profile {
            ui.weak(format!("Saved profile pin: {}", profile.fingerprint));
        } else if let Some(trusted) = remote::trusted_fingerprint(&self.form.remote.address) {
            ui.weak(format!("Trusted: {trusted}"));
        } else if !self.form.remote.fingerprint.is_empty() {
            ui.weak(format!("The PC presents {}", self.form.remote.fingerprint));
            ui.checkbox(
                &mut self.form.remote.fingerprint_checked,
                "I checked this matches the PC's own fingerprint",
            );
        } else {
            ui.weak("First time with this PC: use Save as profile to test its address and confirm its fingerprint, or pick a discovered PC.");
        }
    }

    // ----- status bar -----

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.weak("Engine: Tokio async");
            ui.weak("|");
            let copy_rate = self
                .state
                .active
                .as_ref()
                .and_then(|a| a.telemetry.as_ref())
                .map(|t| t.rate_bps)
                .unwrap_or(0.0);
            ui.label(format!("Copy {}", human_rate(copy_rate)));
            ui.weak("|");
            let remote_running = self
                .state
                .jobs
                .iter()
                .filter(|r| {
                    r.state == JobState::Running
                        && self
                            .state
                            .details
                            .get(&r.id)
                            .is_some_and(|(_, t)| t.contains('/') && t.contains(':'))
                })
                .count();
            ui.label(format!("Remote transfers {remote_running}"));
            if self.state.paused {
                ui.colored_label(warn(ui), "queue paused");
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .selectable_label(self.view == View::Logs, "Ticker log")
                    .clicked()
                {
                    self.view = View::Logs;
                    self.state.unseen = 0;
                }
                if self.state.unseen > 0 {
                    ui.colored_label(warn(ui), format!("{} new", self.state.unseen));
                }
            });
        });
    }

    fn conflict_modal(&mut self, ctx: &egui::Context) {
        let Some(job) = self.queue.active_job() else { return; };
        let Some(conflict) = job.conflict else { return; };
        if self.conflict_choice != Some((job.id, conflict.id)) {
            self.conflict_choice = Some((job.id, conflict.id));
            self.conflict_all = false;
        }
        let mut answer = None;
        egui::Modal::new(egui::Id::new("file-conflict")).show(ctx, |ui| {
            ui.set_width(480.0);
            ui.heading(if conflict.details.different { "Files have different contents" } else { "Destination already exists" });
            ui.label(format!("Source: {}", conflict.details.source.display()));
            ui.label(format!("Destination: {}", conflict.details.destination.display()));
            ui.checkbox(&mut self.conflict_all, "Apply to remaining conflicts in this job");
            ui.horizontal_wrapped(|ui| {
                for (action, label) in [
                    (ConflictAction::Replace, "Replace"), (ConflictAction::Skip, "Skip"),
                    (ConflictAction::KeepBoth, "Keep both"), (ConflictAction::Compare, "Compare"),
                    (ConflictAction::Cancel, "Cancel job"),
                ] {
                    if action == ConflictAction::Compare && conflict.details.different { continue; }
                    if ui.button(label).clicked() { answer = Some(action); }
                }
            });
        });
        if let Some(action) = answer {
            if !self.queue.resolve_conflict(job.id, conflict.id, ConflictChoice { action, apply_to_all: self.conflict_all }) {
                self.notice = Some("That conflict is no longer waiting for an answer.".into());
            }
        }
    }

    fn decision_modal(&mut self, ctx: &egui::Context) {
        let Some(d) = self.state.decisions.front() else {
            return;
        };
        let mut answer = None;
        egui::Modal::new(egui::Id::new("decision")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.heading(format!("⚠ {}", d.class));
            ui.monospace(d.path.display().to_string());
            let at = d
                .offset
                .map(|o| format!(" at byte {o}"))
                .unwrap_or_default();
            ui.label(format!("While {}{at}: {}", d.operation, d.message));
            if d.attempt > 1 {
                ui.weak(format!("Attempt {}", d.attempt));
            }
            ui.add_space(8.0);
            ui.checkbox(
                &mut self.remember_choice,
                format!("Do this for every '{}' error (Skip/Cancel)", d.class),
            );
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
        let dropped: Vec<PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .filter_map(|f| f.path.clone())
                .collect()
        });
        if !dropped.is_empty() {
            if self.form.remote.download {
                self.notice = Some("Select remote sources in the browser; dropped files are local.".into());
                return;
            }
            self.form.sources.extend(dropped);
            self.show_add = true;
        }
    }
}

fn status_icon(ui: &egui::Ui, state: &JobState, paused: bool) -> (&'static str, Color32) {
    match state {
        JobState::Pending => ("◌", dim(ui)),
        JobState::Running if paused => ("⏸", warn(ui)),
        JobState::Running => ("●", accent(ui)),
        JobState::Completed => ("✔", ok(ui)),
        JobState::CompletedWithErrors => ("⚠", warn(ui)),
        JobState::Failed(_) => ("✖", bad(ui)),
        JobState::Cancelled => ("⊘", dim(ui)),
    }
}

fn status_text(ui: &egui::Ui, state: &JobState, paused: bool) -> RichText {
    let (text, color) = match state {
        JobState::Pending => ("Queued".to_string(), dim(ui)),
        JobState::Running if paused => ("Paused".to_string(), warn(ui)),
        JobState::Running => ("Active".to_string(), accent(ui)),
        JobState::Completed => ("Complete".to_string(), ok(ui)),
        JobState::CompletedWithErrors => ("Complete with errors".to_string(), warn(ui)),
        JobState::Failed(msg) => (format!("Failed: {msg}"), bad(ui)),
        JobState::Cancelled => ("Cancelled".to_string(), dim(ui)),
    };
    RichText::new(text).color(color)
}

fn progress_color(ui: &egui::Ui, state: &JobState) -> Color32 {
    match state {
        JobState::Completed => ok(ui),
        JobState::Failed(_) => bad(ui),
        _ => accent(ui),
    }
}

impl eframe::App for FcApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.appearance.apply(ctx);
        self.pump_events();
        self.pump_launches();
        if self.last_rate.elapsed() >= Duration::from_millis(500) {
            let rate = self.current_rate();
            self.rates.push_back(rate as f32);
            if self.rates.len() > 120 { self.rates.pop_front(); }
            self.last_rate = std::time::Instant::now();
        }
        for (saved_id, request) in self.recovery.poll() {
            let details = (request.sources.clone(), request.target.describe());
            let id = self.history.enqueue(&self.queue, request);
            self.recovery.bind(id, saved_id);
            self.state.details.insert(id, details);
            self.view = View::Queue;
        }
        if self.recovery.take_attention() { self.view = View::Recovery; }
        if self.view != View::Recovery { self.recovery.hide(); }
        if self.recovery.is_busy() { ctx.request_repaint_after(Duration::from_millis(100)); }
        if let Some(error) = self.history.poll() { self.notice = Some(error); }
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
            if self.instance.pending() > 0 {
                ui.label(format!(
                    "{} incoming launch(es) waiting. Finish or close Add task to open the next selection.",
                    self.instance.pending()
                ));
            }
            if self.launch_listener_failed {
                ui.colored_label(bad(ui), "Desktop launch listener stopped. Restart FCOPY after finishing current transfers.");
            }
            ui.add_space(4.0);
        });
        egui::TopBottomPanel::bottom("status").show(ctx, |ui| {
            ui.add_space(2.0);
            self.status_bar(ui);
            ui.add_space(2.0);
        });
        if ctx.screen_rect().width() >= 1450.0 {
            egui::SidePanel::left("nav").resizable(false).exact_width(180.0).show(ctx, |ui| self.sidebar(ui));
        }
        if ctx.screen_rect().width() >= 1100.0 && self.view == View::Queue {
            ctx.request_repaint_after(Duration::from_millis(500));
            egui::SidePanel::right("monitor").resizable(true).default_width(260.0).width_range(220.0..=340.0)
                .show(ctx, |ui| self.monitor(ui));
        }
        self.torrents.refresh();
        egui::CentralPanel::default().show(ctx, |ui| match self.view {
            View::Queue => self.queue_view(ui),
            View::History => self.history.show(ui),
            View::Recovery => self.recovery.show(ui),
            View::Peers => self.peers_view(ui),
            View::Torrents => torrent::show(ui, &mut self.torrents),
            View::Logs => self.logs_view(ui),
            View::Settings => {
                egui::ScrollArea::vertical().show(ui, |ui| {
                    self.appearance.show(ui);
                    ui.separator();
                    if let Some(remote) = self.vpn.show(ui) {
                    self.form.sources = if remote.download {
                        vec![PathBuf::from(&remote.folder)]
                    } else {
                        Vec::new()
                    };
                    self.form.remote = remote;
                    self.browser = RemoteBrowser::default();
                    self.notice = None;
                    self.show_add = true;
                    }
                });
            }
        });

        if let Some(selected) = &self.form.remote.profile {
            if !self.profiles.list().iter().any(|p| p == selected) {
                self.form.remote.clear_identity();
                self.browser = RemoteBrowser::default();
                if self.form.remote.download { self.form.sources.clear(); }
                self.notice = Some("The selected profile changed or was removed. Select the saved PC again before transferring.".into());
            }
        }
        let mut open = self.show_add;
        egui::Window::new("Add task / files")
            .open(&mut open)
            .resizable(false)
            .default_width(560.0)
            .show(ctx, |ui| self.add_form(ui));
        if self.show_add && !open {
            self.form.remote.password.clear();
            self.browser = RemoteBrowser::default();
        }
        self.show_add &= open;
        if self.instance.pending() > 0 && !self.show_add && !self.recovery.is_busy() {
            ctx.request_repaint();
        }

        self.decision_modal(ctx);
        self.conflict_modal(ctx);

        // Keep "Elapsed" ticking between telemetry events (e.g. at very low speed limits).
        if self.state.active.is_some() && !self.state.paused {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
        // Torrent rates and progress change without events, so the tab repaints on a timer.
        if self.torrents.is_busy() {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
        self.appearance.overlay(ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.instance.shutdown();
        // Keep job descriptions while the worker cleans up interrupted local files.
        self.recovery.suspend();
        self.queue.shutdown();
        self.pump_events();
        self.recovery.shutdown();
        self.history.shutdown();
        self.torrents.shutdown();
    }
}

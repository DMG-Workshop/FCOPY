//! Drawing + commands. Reads [`AppState`], sends commands to the [`Queue`].

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

use crate::state::{AppState, LogKind};
use crate::Startup;

const OK: Color32 = Color32::from_rgb(110, 200, 120);
const WARN: Color32 = Color32::from_rgb(230, 180, 80);
const BAD: Color32 = Color32::from_rgb(235, 100, 90);

/// Form for a new transfer.
struct NewTransfer {
    sources: Vec<PathBuf>,
    add_path: String,
    dest: String,
    verify: VerifyMode,
    overwrite: bool,
}

pub struct FcApp {
    queue: Queue,
    events: Receiver<Event>,
    state: AppState,
    form: NewTransfer,
    log_open: bool,
    limit_on: bool,
    limit_mb: f64,
    /// "Do this for all errors of this kind" answers.
    remembered: HashMap<ErrorClass, ErrorAction>,
    remember_choice: bool,
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
            },
            log_open: false,
            limit_on: startup.limit.is_some(),
            limit_mb: startup.limit.map(|l| l as f64 / 1e6).unwrap_or(50.0),
            remembered: HashMap::new(),
            remember_choice: false,
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
        let dest = PathBuf::from(self.form.dest.trim());
        let sources = std::mem::take(&mut self.form.sources);
        let request = if sources.len() == 1 && sources[0].is_file() && !dest.is_dir() {
            TransferRequest::file_to(&sources[0], &dest)
        } else {
            TransferRequest::into_dir(sources.clone(), &dest)
        };
        let options = CopyOptions {
            verify: self.form.verify,
            algorithm: Algorithm::Xxh3,
            overwrite: self.form.overwrite,
            ..Default::default()
        };
        let id = self.queue.add(request.with_options(options));
        self.state.details.insert(id, (sources, dest.display().to_string()));
    }

    // ----- panels -----

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.heading("fcopy");
            ui.separator();
            let running = self.state.active.is_some();
            if self.state.paused {
                if ui.button("▶ Resume").clicked() {
                    self.queue.resume();
                }
            } else if ui.add_enabled(running, egui::Button::new("⏸ Pause")).clicked() {
                self.queue.pause();
            }
            if ui.add_enabled(running, egui::Button::new("⏭ Skip file")).on_hover_text("Abandon the current file and continue").clicked() {
                self.queue.skip_current_file();
            }
            if ui.add_enabled(running, egui::Button::new("⏹ Cancel")).on_hover_text("Cancel the running transfer").clicked() {
                if let Some(a) = &self.state.active {
                    self.queue.cancel(a.job);
                }
            }
            if ui.button("Cancel all").clicked() {
                self.queue.cancel_all();
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut changed = ui
                    .add_enabled(self.limit_on, egui::DragValue::new(&mut self.limit_mb).range(0.1..=10_000.0).speed(1.0).suffix(" MB/s"))
                    .changed();
                changed |= ui.checkbox(&mut self.limit_on, "Limit speed").changed();
                if changed {
                    let limit = self.limit_on.then_some((self.limit_mb * 1e6) as u64);
                    self.queue.set_speed_limit(limit);
                }
            });
        });
    }

    fn queue_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.strong("Queue");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Clear finished").clicked() {
                    self.queue.clear_finished();
                }
            });
        });
        ui.separator();
        if self.state.jobs.is_empty() {
            ui.weak("No transfers yet.");
        }
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            for row in &self.state.jobs {
                let (icon, color) = match &row.state {
                    JobState::Pending => ("⏳", ui.visuals().weak_text_color()),
                    JobState::Running if self.state.paused => ("⏸", WARN),
                    JobState::Running => ("▶", ui.visuals().text_color()),
                    JobState::Completed => ("✔", OK),
                    JobState::CompletedWithErrors => ("⚠", WARN),
                    JobState::Failed(_) => ("✖", BAD),
                    JobState::Cancelled => ("⊘", ui.visuals().weak_text_color()),
                };
                ui.horizontal(|ui| {
                    ui.colored_label(color, icon);
                    ui.add(egui::Label::new(&row.label).truncate());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let finished = row.state.is_finished();
                        let tip = if finished { "Remove from list" } else { "Cancel" };
                        if ui.small_button("✖").on_hover_text(tip).clicked() {
                            if finished { self.queue.remove(row.id); } else { self.queue.cancel(row.id); }
                        }
                    });
                });
                if row.state == JobState::Running || row.state == JobState::Pending {
                    ui.add(egui::ProgressBar::new(row.fraction as f32).desired_height(6.0));
                } else if let Some(s) = &row.summary {
                    ui.weak(format!(
                        "{} files · {} · {}",
                        s.files_copied,
                        human_bytes(s.bytes_copied),
                        human_rate(s.throughput_bps)
                    ));
                }
                ui.add_space(6.0);
            }
        });
    }

    fn dashboard(&mut self, ui: &mut egui::Ui) {
        let Some(active) = self.state.active.clone() else {
            ui.add_space(8.0);
            ui.weak("Idle. Add a transfer below or drop files onto the window.");
            return;
        };
        let row = self.state.jobs.iter().find(|r| r.id == active.job).cloned();
        let label = row.as_ref().map(|r| r.label.clone()).unwrap_or_default();
        let (sources, target) = self.state.details.get(&active.job).cloned().unwrap_or_default();

        egui::Frame::group(ui.style()).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(&label).strong().size(18.0));
                if self.state.paused {
                    ui.label(RichText::new(" PAUSED ").strong().color(Color32::BLACK).background_color(WARN));
                }
            });
            egui::Grid::new("paths").num_columns(2).spacing([12.0, 4.0]).show(ui, |ui| {
                ui.weak("From");
                let from = match sources.len() {
                    0 => "—".to_string(),
                    1 => sources[0].display().to_string(),
                    n => format!("{} (+{} more)", sources[0].display(), n - 1),
                };
                ui.add(egui::Label::new(egui::RichText::new(from).monospace()).truncate());
                ui.end_row();
                ui.weak("To");
                ui.add(egui::Label::new(egui::RichText::new(if target.is_empty() { "—".into() } else { target }).monospace()).truncate());
                ui.end_row();
            });
            ui.separator();

            let Some(t) = &active.telemetry else {
                ui.weak("Preparing…");
                return;
            };
            let file = t.current_file.as_ref().and_then(|p| p.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
            let phase = match t.phase {
                Phase::Verifying => "Verifying",
                Phase::Finalizing => "Finalizing",
                Phase::Preparing => "Opening",
                _ => "Copying",
            };
            ui.horizontal(|ui| {
                ui.weak(phase);
                ui.add(egui::Label::new(RichText::new(&file).strong()).truncate());
            });
            let file_frac = if t.file_bytes_total > 0 { t.file_bytes_done as f32 / t.file_bytes_total as f32 } else { 0.0 };
            ui.add(
                egui::ProgressBar::new(file_frac)
                    .text(format!("{} / {}", human_bytes(t.file_bytes_done), human_bytes(t.file_bytes_total))),
            );
            ui.add_space(4.0);
            ui.add(egui::ProgressBar::new(t.fraction as f32).show_percentage().desired_height(22.0));
            ui.add_space(6.0);

            egui::Grid::new("stats").num_columns(4).spacing([28.0, 6.0]).show(ui, |ui| {
                let speed = if self.state.paused { "—".to_string() } else { human_rate(t.rate_bps) };
                let eta = if self.state.paused { "paused".to_string() } else { t.eta.map(human_duration).unwrap_or_else(|| "—".into()) };
                stat(ui, "Speed", &speed);
                stat(ui, "ETA", &eta);
                ui.end_row();
                // Interpolate between telemetry ticks (slow, throttled transfers emit rarely).
                let since = if self.state.paused { Duration::ZERO } else { active.received.elapsed() };
                stat(ui, "Elapsed", &human_duration(t.elapsed + since));
                stat(ui, "Files", &format!("{} / {}", t.files_done, t.files_total));
                ui.end_row();
                stat(ui, "Copied", &format!("{} / {}", human_bytes(t.bytes_copied), human_bytes(t.bytes_total)));
                let problems = active.files_skipped + active.files_failed;
                stat(ui, "Skipped/failed", &problems.to_string());
                ui.end_row();
                if let Some(l) = self.state.speed_limit {
                    // Same decimal unit the user typed in the toolbar.
                    stat(ui, "Limit", &format!("{:.1} MB/s", l as f64 / 1e6));
                    ui.end_row();
                }
            });
        });
    }

    fn new_transfer(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new("New transfer").default_open(true).show(ui, |ui| {
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
            ui.horizontal(|ui| {
                ui.label("Destination");
                ui.add(egui::TextEdit::singleline(&mut self.form.dest).hint_text("folder (or new file name for a single file)").desired_width(f32::INFINITY));
            });
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
                let ready = !self.form.sources.is_empty() && !self.form.dest.trim().is_empty();
                if ui.add_enabled(ready, egui::Button::new(RichText::new("Start copy").strong())).clicked() {
                    self.start_transfer();
                }
            });
        });
    }

    fn log_drawer(&mut self, ui: &mut egui::Ui) {
        let errors = self.state.error_count();
        let warnings = self.state.log.len() - errors;
        ui.horizontal(|ui| {
            let arrow = if self.log_open { "⏷" } else { "⏵" };
            let mut title = RichText::new(format!("{arrow} Errors & skipped ({errors})"));
            if self.state.unseen > 0 && !self.log_open {
                title = title.color(WARN).strong();
            }
            if ui.selectable_label(self.log_open, title).clicked() {
                self.log_open = !self.log_open;
            }
            ui.weak(format!("{warnings} warning(s)"));
            if self.log_open {
                self.state.unseen = 0;
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("Clear").clicked() {
                        self.state.log.clear();
                    }
                });
            }
        });
        if !self.log_open {
            return;
        }
        ui.separator();
        egui::ScrollArea::vertical().auto_shrink([false, false]).stick_to_bottom(true).show(ui, |ui| {
            for e in &self.state.log {
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
                    // Short form on the row; full path + message on hover.
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

    fn handle_drops(&mut self, ctx: &egui::Context) {
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        self.form.sources.extend(dropped);
    }
}

fn stat(ui: &mut egui::Ui, name: &str, value: &str) {
    ui.weak(name);
    ui.label(RichText::new(value).strong().monospace());
}

impl eframe::App for FcApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump_events();
        self.handle_drops(ctx);

        // Panel order matters in egui: edges first, central last.
        egui::TopBottomPanel::top("toolbar").show(ctx, |ui| {
            ui.add_space(4.0);
            self.toolbar(ui);
            ui.add_space(2.0);
        });
        egui::TopBottomPanel::bottom("log")
            .resizable(self.log_open)
            .min_height(if self.log_open { 140.0 } else { 0.0 })
            .show(ctx, |ui| self.log_drawer(ui));
        egui::SidePanel::left("queue").resizable(true).default_width(240.0).show(ctx, |ui| self.queue_panel(ui));
        egui::CentralPanel::default().show(ctx, |ui| {
            self.dashboard(ui);
            ui.add_space(12.0);
            self.new_transfer(ui);
            if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
                ui.colored_label(OK, "Drop to add as sources");
            }
        });
        self.decision_modal(ctx);

        // Keep "Elapsed" ticking between telemetry events (e.g. at very low speed limits).
        if self.state.active.is_some() && !self.state.paused {
            ctx.request_repaint_after(Duration::from_millis(500));
        }
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // Cancel cleanly so no .fcpart files are left behind.
        self.queue.shutdown();
    }
}

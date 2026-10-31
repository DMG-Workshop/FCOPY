//! History I/O stays off the drawing thread.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;

use eframe::egui;
use fc_engine::{Event, JobId, Queue, TransferRequest};
use fc_engine::units::{human_bytes, human_duration};
use fc_remote::history::{Entry, HistoryStore, Pending};

enum Command { Record(Entry), Refresh, Clear, Stop }

pub struct History {
    pending: HashMap<JobId, Pending>,
    tx: Option<Sender<Command>>,
    rx: Receiver<Result<Vec<Entry>, String>>,
    worker: Option<JoinHandle<()>>,
    entries: Vec<Entry>,
    error: Option<String>,
    search: String,
    confirm_clear: bool,
}

impl History {
    pub fn new(ctx: egui::Context) -> Self {
        Self::open(fc_remote::config::default_client_dir().join("desktop-history.toml"), ctx)
    }

    fn open(path: PathBuf, ctx: egui::Context) -> Self {
        let (tx, commands) = mpsc::channel();
        let (results, rx) = mpsc::channel();
        let worker = std::thread::Builder::new().name("fc-desktop-history".into()).spawn(move || {
            let mut store = None;
            let mut backlog = Vec::new();
            while let Ok(command) = commands.recv() {
                let stopping = matches!(command, Command::Stop);
                if let Command::Record(entry) = &command {
                    backlog.push(entry.clone());
                    if backlog.len() > fc_remote::history::LIMIT { backlog.remove(0); }
                }
                let result = (|| {
                    if store.is_none() { store = Some(HistoryStore::open(Some(path.clone())).map_err(|e| e.to_string())?); }
                    let store = store.as_mut().expect("opened history");
                    // The store retains a failed write; do not enqueue it twice.
                    for entry in std::mem::take(&mut backlog) {
                        if let Err(error) = store.record(entry) {
                            eprintln!("transfer history write pending: {error}");
                        }
                    }
                    if matches!(command, Command::Clear) { store.clear(None).map_err(|e| e.to_string())?; }
                    store.list().map_err(|e| e.to_string())
                })();
                if let Err(error) = &result { eprintln!("transfer history: {error}"); }
                let _ = results.send(result);
                ctx.request_repaint();
                if stopping { break; }
            }
        });
        let (worker, tx, error) = match worker {
            Ok(worker) => (Some(worker), Some(tx), None),
            Err(error) => (None, None, Some(format!("Cannot start history worker: {error}"))),
        };
        let mut history = Self { pending: HashMap::new(), tx, rx, worker, entries: Vec::new(), error, search: String::new(), confirm_clear: false };
        if history.tx.is_some() { history.send(Command::Refresh); }
        history
    }

    fn send(&mut self, command: Command) {
        if self.tx.as_ref().is_none_or(|tx| tx.send(command).is_err()) {
            self.error = Some("History worker is unavailable; results cannot be saved.".into());
        }
    }

    pub fn enqueue(&mut self, queue: &Queue, request: TransferRequest) -> JobId {
        let pending = Pending::new(&request, None);
        let id = queue.add(request);
        self.pending.insert(id, pending);
        id
    }

    pub fn observe(&mut self, event: &Event) {
        if let Event::Finished { job, state, summary } = event {
            if let Some(pending) = self.pending.remove(job) {
                match pending.finish(state, summary) {
                    Ok(entry) => self.send(Command::Record(entry)),
                    Err(error) => self.error = Some(error.to_string()),
                }
            }
        }
    }

    pub fn poll(&mut self) -> Option<String> {
        let mut error = None;
        while let Ok(result) = self.rx.try_recv() {
            match result {
                Ok(entries) => { self.entries = entries; self.error = None; }
                Err(message) => { error = Some(format!("History was not saved: {message}. Open History and Refresh to retry.")); self.error = Some(message); }
            }
        }
        error
    }

    pub fn show(&mut self, ui: &mut egui::Ui) {
        ui.heading("Transfer history");
        ui.label("Latest 1,000 finished jobs on this PC. History does not restart transfers.");
        if let Some(error) = &self.error { ui.colored_label(crate::app::bad(ui), error); }
        ui.horizontal_wrapped(|ui| {
            ui.label("Search");
            ui.text_edit_singleline(&mut self.search);
            if ui.button("Refresh / retry saving").clicked() { self.send(Command::Refresh); }
            if ui.button("Clear history...").clicked() { self.confirm_clear = true; }
        });
        if self.confirm_clear {
            ui.horizontal_wrapped(|ui| {
                ui.label("Delete saved summaries? Files and queued transfers are not changed.");
                if ui.button("Confirm clear").clicked() { self.send(Command::Clear); self.confirm_clear = false; }
                if ui.button("Keep history").clicked() { self.confirm_clear = false; }
            });
        }
        ui.separator();
        let search = self.search.to_lowercase();
        egui::ScrollArea::vertical().show(ui, |ui| {
            for entry in self.entries.iter().filter(|e| search.is_empty() || e.label.to_lowercase().contains(&search)
                || e.destination.to_lowercase().contains(&search) || e.sources.iter().any(|s| s.to_lowercase().contains(&search))) {
                egui::CollapsingHeader::new(format!("{} - {} - {}", entry.label, entry.outcome.label(), human_bytes(entry.bytes_copied)))
                    .id_salt(entry.id).show(ui, |ui| {
                        ui.label(format!("Finished: {} UTC", timestamp(entry.finished_at_ms)));
                        ui.label(format!("Queued: {} UTC", timestamp(entry.queued_at_ms)));
                        ui.label(format!("Duration: {}", human_duration(std::time::Duration::from_millis(entry.elapsed_ms))));
                        ui.label(format!("Copied: {}  Skipped: {}  Failed: {}", entry.files_copied, entry.files_skipped, entry.files_failed));
                        ui.label(if entry.remote { "Remote transfer" } else { "Local copy" });
                        ui.label(format!("Destination: {}", entry.destination));
                        ui.label("Sources:");
                        for source in &entry.sources { ui.label(source); }
                    });
            }
            if self.entries.is_empty() { ui.label("No finished transfers saved yet."); }
        });
    }

    pub fn shutdown(&mut self) {
        self.send(Command::Stop);
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() { eprintln!("transfer history worker failed during shutdown"); }
        }
        self.tx = None;
        self.poll();
    }
}

// Gregorian civil date from Unix days; avoids a date/time dependency for one label.
fn timestamp(ms: u64) -> String {
    let seconds = ms / 1_000;
    let days = (seconds / 86_400) as i64 + 719_468;
    let era = days / 146_097;
    let day = days - era * 146_097;
    let year_of_era = (day - day / 1_460 + day / 36_524 - day / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let date = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    year += i64::from(month <= 2);
    format!("{year:04}-{month:02}-{date:02} {:02}:{:02}:{:02}", seconds / 3_600 % 24, seconds / 60 % 60, seconds % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_dates_are_utc_and_handle_leap_days() {
        assert_eq!(timestamp(0), "1970-01-01 00:00:00");
        assert_eq!(timestamp(1_709_164_800_000), "2024-02-29 00:00:00");
    }

    #[test]
    fn desktop_history_saves_on_shutdown_and_renders_after_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("history.toml");
        let ctx = egui::Context::default();
        let mut history = History::open(path.clone(), ctx.clone());
        let pending = Pending::new(&TransferRequest::file_to("source", "destination"), None);
        history.send(Command::Record(pending.finish(&fc_engine::JobState::Completed, &Default::default()).unwrap()));
        history.shutdown();
        let mut reopened = History::open(path, ctx.clone());
        reopened.shutdown();
        assert_eq!(reopened.entries.len(), 1);
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| reopened.show(ui));
        });
    }

    #[test]
    fn desktop_history_records_real_queue_events_before_exit() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        std::fs::write(&source, b"saved summary").unwrap();
        let mut history = History::open(dir.path().join("history.toml"), egui::Context::default());
        let (queue, events) = Queue::start(Default::default());
        let id = history.enqueue(&queue, TransferRequest::file_to(source, dir.path().join("destination")).with_label("Copied file"));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let event = events.recv_timeout(deadline.saturating_duration_since(std::time::Instant::now())).unwrap();
            history.observe(&event);
            if matches!(event, Event::Finished { job, .. } if job == id) { break; }
        }
        queue.shutdown();
        history.shutdown();
        assert_eq!(history.entries.len(), 1);
        assert_eq!(history.entries[0].label, "Copied file");
        assert_eq!(history.entries[0].files_copied, 1);
        assert_eq!(history.entries[0].bytes_copied, 13);
    }
}

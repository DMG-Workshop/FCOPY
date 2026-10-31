//! Persist before queueing. Recovered jobs never connect until explicitly resumed.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::JoinHandle;

use egui::Context;
use fc_engine::{Event, JobId, JobState, TransferRequest};
use fc_remote::recovery::{Action, Job, RecoveryStore};

enum Command {
    Save(Box<Job>, TransferRequest),
    Run(u64, Action, String),
    Discard(u64),
    Finish(u64),
    Release(u64),
    Refresh,
    Stop,
}

enum Reply {
    Rows(Vec<Job>),
    Ready(u64, TransferRequest),
    Error(String),
}

pub struct Recovery {
    tx: Sender<Command>,
    rx: Receiver<Reply>,
    worker: Option<JoinHandle<()>>,
    active: HashMap<JobId, u64>,
    rows: Vec<Job>,
    error: Option<String>,
    password: String,
    selected: Option<u64>,
    confirming: Option<u64>,
    busy: bool,
    suspending: bool,
    first_load: bool,
    attention: bool,
}

impl Recovery {
    pub fn new(ctx: Context) -> Self {
        Self::open(
            fc_remote::config::default_client_dir().join("unfinished-desktop-jobs.toml"),
            ctx,
        )
    }

    fn open(path: PathBuf, ctx: Context) -> Self {
        let (tx, commands) = mpsc::channel();
        let (results, rx) = mpsc::channel();
        let worker = std::thread::Builder::new()
            .name("fc-recovery".into())
            .spawn(move || {
                let mut store = None;
                let mut claims = HashMap::new();
                while let Ok(command) = commands.recv() {
                    if matches!(command, Command::Stop) {
                        break;
                    }
                    let _finished_claim = match &command {
                        Command::Finish(id) | Command::Release(id) => claims.remove(id),
                        _ => None,
                    };
                    let result = (|| {
                        if store.is_none() {
                            store = Some(RecoveryStore::open(Some(path.clone()))?);
                        }
                        let store = store.as_mut().expect("opened recovery");
                        store.refresh()?;
                        match command {
                            Command::Save(job, request) => {
                                let id = store.add(*job)?;
                                let claim = store.claim(id)?;
                                claims.insert(id, claim);
                                let _ = results.send(Reply::Ready(id, request));
                            }
                            Command::Run(id, action, password) => {
                                if claims.contains_key(&id) {
                                    return Err(std::io::Error::other("job is already queued"));
                                }
                                let claim = store.claim(id)?;
                                let job = store.get(id).ok_or_else(|| {
                                    std::io::Error::other("saved job no longer exists")
                                })?;
                                let request = job.request(action, password)?;
                                claims.insert(id, claim);
                                let _ = results.send(Reply::Ready(id, request));
                            }
                            Command::Discard(id) => {
                                let _claim = store.claim(id)?;
                                store.remove(id)?;
                            }
                            Command::Finish(id) => {
                                store.remove(id)?;
                            }
                            Command::Release(_) => {}
                            Command::Refresh => {}
                            Command::Stop => unreachable!(),
                        }
                        let rows = store
                            .list()
                            .iter()
                            .filter(|j| !claims.contains_key(&j.id))
                            .cloned()
                            .collect();
                        let _ = results.send(Reply::Rows(rows));
                        Ok::<_, std::io::Error>(())
                    })();
                    if let Err(error) = result {
                        eprintln!("queue recovery: {error}");
                        let _ = results.send(Reply::Error(error.to_string()));
                    }
                    ctx.request_repaint();
                }
            });
        let (worker, error) = match worker {
            Ok(worker) => (Some(worker), None),
            Err(error) => (None, Some(format!("Cannot start recovery worker: {error}"))),
        };
        let mut recovery = Self {
            tx,
            rx,
            worker,
            active: HashMap::new(),
            rows: Vec::new(),
            error,
            password: String::new(),
            selected: None,
            confirming: None,
            busy: false,
            suspending: false,
            first_load: true,
            attention: false,
        };
        recovery.send(Command::Refresh);
        recovery
    }

    fn send(&mut self, command: Command) {
        if self.tx.send(command).is_err() {
            self.error = Some("Recovery worker is unavailable. The job was not started.".into());
            self.busy = false;
            self.attention = true;
        }
    }

    pub fn save(&mut self, job: Job, request: TransferRequest) {
        self.busy = true;
        self.send(Command::Save(Box::new(job), request));
    }

    pub fn bind(&mut self, queue_id: JobId, saved_id: u64) {
        self.active.insert(queue_id, saved_id);
    }

    pub fn observe(&mut self, event: &Event) {
        let id = match event {
            Event::Finished { job, state, .. }
                if !(self.suspending && *state == JobState::Cancelled) =>
            {
                *job
            }
            Event::Removed { job } if !self.suspending => *job,
            _ => return,
        };
        if let Some(id) = self.active.remove(&id) {
            let failed = matches!(
                event,
                Event::Finished {
                    state: JobState::Failed(_),
                    ..
                }
            ) || matches!(event, Event::Finished { summary, .. } if summary.files_failed > 0);
            self.send(if failed {
                Command::Release(id)
            } else {
                Command::Finish(id)
            });
        }
    }

    pub fn poll(&mut self) -> Vec<(u64, TransferRequest)> {
        let mut ready = Vec::new();
        while let Ok(reply) = self.rx.try_recv() {
            self.busy = false;
            match reply {
                Reply::Ready(id, request) => ready.push((id, request)),
                Reply::Rows(rows) => {
                    if self.first_load && !rows.is_empty() {
                        self.attention = true;
                    }
                    self.first_load = false;
                    self.rows = rows;
                    self.error = None;
                }
                Reply::Error(error) => {
                    self.error = Some(error);
                    self.attention = true;
                }
            }
        }
        ready
    }

    pub fn take_attention(&mut self) -> bool {
        std::mem::take(&mut self.attention)
    }
    pub fn is_busy(&self) -> bool {
        self.busy
    }

    pub fn show(&mut self, ui: &mut egui::Ui) {
        ui.heading("Queue recovery");
        ui.label("Restored jobs are paused. No connections or copies start until Resume or Retry.");
        ui.label("Resume checks existing files and skips identical content; different content asks again. Remote partials are verified. Local interrupted files restart safely.");
        ui.label("Retry starts unfinished files from byte zero, using the saved conflict policy.");
        if let Some(error) = &self.error {
            ui.colored_label(crate::app::bad(ui), error);
        }
        if ui
            .add_enabled(!self.busy, egui::Button::new("Refresh / retry storage"))
            .clicked()
        {
            self.send(Command::Refresh);
        }
        if self.busy {
            ui.spinner();
        }
        let mut command = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            for job in &self.rows {
                ui.group(|ui| {
                    ui.strong(format!("{} - Paused", job.label));
                    ui.label(job.describe());
                    egui::CollapsingHeader::new("Sources").id_salt(job.id).show(ui, |ui| {
                        for source in &job.sources { ui.label(source.display().to_string()); }
                    });
                    if job.remote() {
                        if self.selected != Some(job.id) {
                            if ui.button("Enter password for this PC").clicked() { self.password.clear(); self.selected = Some(job.id); }
                        } else {
                            ui.label("PC password / access key (not saved)");
                            ui.add(egui::TextEdit::singleline(&mut self.password).password(true));
                        }
                    }
                    let can_run = !self.busy && (!job.remote() || (self.selected == Some(job.id) && !self.password.is_empty()));
                    ui.horizontal_wrapped(|ui| {
                        if ui.add_enabled(can_run, egui::Button::new("Resume")).clicked() { command = Some(Command::Run(job.id, Action::Resume, std::mem::take(&mut self.password))); }
                        if ui.add_enabled(can_run, egui::Button::new("Retry")).clicked() { command = Some(Command::Run(job.id, Action::Retry, std::mem::take(&mut self.password))); }
                        if ui.add_enabled(!self.busy, egui::Button::new("Discard...")).clicked() { self.confirming = Some(job.id); self.password.clear(); }
                    });
                    if self.confirming == Some(job.id) {
                        ui.label("Remove this saved job only? Copied data and partial files are left untouched.");
                        ui.horizontal(|ui| {
                            if ui.button("Confirm discard").clicked() { command = Some(Command::Discard(job.id)); self.confirming = None; }
                            if ui.button("Keep job").clicked() { self.confirming = None; }
                        });
                    }
                });
            }
            if self.rows.is_empty() { ui.label("No unfinished jobs to recover."); }
        });
        if let Some(command) = command {
            self.busy = true;
            self.send(command);
        }
    }

    pub fn hide(&mut self) {
        self.password.clear();
        self.selected = None;
    }
    pub fn suspend(&mut self) {
        self.suspending = true;
        self.hide();
    }
    pub fn shutdown(&mut self) {
        self.send(Command::Stop);
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                eprintln!("recovery worker failed during shutdown");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fc_engine::{CopyOptions, Queue, QueueConfig};
    use fc_remote::recovery::Destination;
    use std::time::{Duration, Instant};

    fn wait(
        recovery: &mut Recovery,
        predicate: impl Fn(&Recovery) -> bool,
    ) -> Vec<(u64, TransferRequest)> {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut requests = Vec::new();
        loop {
            requests.extend(recovery.poll());
            if predicate(recovery) {
                return requests;
            }
            assert!(Instant::now() < deadline, "recovery worker did not reply");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn shutdown_preserves_jobs_and_restart_is_paused_until_explicit_resume() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.toml");
        let source = dir.path().join("source");
        let dest = dir.path().join("out");
        std::fs::write(&source, b"saved job").unwrap();
        let request = TransferRequest::into_dir(vec![source], &dest);
        let job = Job::new(
            &request,
            Destination::Directory { path: dest.clone() },
            None,
            &CopyOptions::default(),
        )
        .unwrap();
        let mut recovery = Recovery::open(path.clone(), Context::default());
        wait(&mut recovery, |r| !r.first_load);
        recovery.save(job, request);
        let mut ready = wait(&mut recovery, |r| !r.busy);
        assert_eq!(ready.len(), 1);
        assert_eq!(
            RecoveryStore::open(Some(path.clone()))
                .unwrap()
                .list()
                .len(),
            1,
            "saved before request is runnable"
        );
        let (queue, events) = Queue::start(QueueConfig::default());
        queue.pause();
        let (saved_id, request) = ready.pop().unwrap();
        let live_id = queue.add(request);
        recovery.bind(live_id, saved_id);
        recovery.suspend();
        queue.shutdown();
        for event in events.try_iter() {
            recovery.observe(&event);
        }
        recovery.shutdown();
        assert!(!dest.exists());

        let mut recovery = Recovery::open(path.clone(), Context::default());
        let ready = wait(&mut recovery, |r| !r.first_load);
        assert!(ready.is_empty(), "opening must not submit jobs");
        assert_eq!(recovery.rows.len(), 1);
        assert!(recovery.take_attention());
        assert!(
            !recovery.take_attention(),
            "navigation is not repeatedly forced into Recovery"
        );
        std::fs::create_dir(&dest).unwrap();
        let partial = dest.join("source.fcpart");
        std::fs::write(&partial, b"wrong bytes from an interrupted local attempt").unwrap();
        recovery.busy = true;
        recovery.send(Command::Run(saved_id, Action::Resume, String::new()));
        let mut ready = wait(&mut recovery, |r| !r.busy);
        assert_eq!(ready.len(), 1);
        let (queue, events) = Queue::start(QueueConfig::default());
        let (saved_id, request) = ready.pop().unwrap();
        let live_id = queue.add(request);
        recovery.bind(live_id, saved_id);
        loop {
            let event = events.recv_timeout(Duration::from_secs(10)).unwrap();
            let done = matches!(event, Event::Finished { .. });
            recovery.observe(&event);
            if done {
                break;
            }
        }
        recovery.shutdown();
        queue.shutdown();
        assert_eq!(std::fs::read(dest.join("source")).unwrap(), b"saved job");
        assert!(
            !partial.exists(),
            "an arbitrary local partial is restarted, never spliced"
        );
        assert!(RecoveryStore::open(Some(path)).unwrap().list().is_empty());
    }

    #[test]
    fn persistence_failure_never_emits_a_runnable_request() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.toml");
        let mut recovery = Recovery::open(path.clone(), Context::default());
        wait(&mut recovery, |r| !r.first_load);
        std::fs::create_dir(path.with_extension("tmp")).unwrap();
        let request =
            TransferRequest::into_dir(vec![dir.path().join("source")], dir.path().join("out"));
        let job = Job::new(
            &request,
            Destination::Directory {
                path: dir.path().join("out"),
            },
            None,
            &CopyOptions::default(),
        )
        .unwrap();
        recovery.save(job, request);
        let ready = wait(&mut recovery, |r| r.error.is_some());
        assert!(ready.is_empty());
        assert!(recovery.take_attention());
        recovery.shutdown();
        assert!(!path.exists());
        assert!(!dir.path().join("out").exists());
    }

    #[test]
    fn finished_job_releases_claim_even_when_storage_cannot_be_refreshed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("jobs.toml");
        let mut recovery = Recovery::open(path.clone(), Context::default());
        wait(&mut recovery, |r| !r.first_load);
        let request =
            TransferRequest::into_dir(vec![dir.path().join("source")], dir.path().join("out"));
        let job = Job::new(
            &request,
            Destination::Directory {
                path: dir.path().join("out"),
            },
            None,
            &CopyOptions::default(),
        )
        .unwrap();
        recovery.save(job, request);
        let ready = wait(&mut recovery, |r| !r.busy);
        let id = ready[0].0;
        recovery.bind(1, id);
        let before = std::fs::read(&path).unwrap();
        std::fs::write(&path, "broken = [").unwrap();
        recovery.observe(&Event::Finished {
            job: 1,
            state: JobState::Completed,
            summary: Default::default(),
        });
        wait(&mut recovery, |r| r.error.is_some());
        std::fs::write(&path, before).unwrap();
        recovery.send(Command::Refresh);
        wait(&mut recovery, |r| r.rows.len() == 1 && r.error.is_none());
        let mut store = RecoveryStore::open(Some(path)).unwrap();
        assert!(
            store.claim(id).is_ok(),
            "an ended job must not remain hidden behind an orphaned claim"
        );
        recovery.shutdown();
    }
}

//! Disks tab: lists attached disks and, when this build allows it, erases one.
//!
//! Listing is read-only. Erasing never runs in this process: the app seals a plan
//! and starts `fcopy-storage-worker` through `pkexec`, which asks the administrator
//! to authenticate. The worker repeats every safety check, so the checks here only
//! stop obvious mistakes early and explain them to the user.
//!
//! Erasing needs the `storage-destructive` feature. Without it the tab only lists disks.

use std::collections::HashSet;
use std::sync::mpsc::{Receiver, TryRecvError};
use std::time::Duration;

use egui::{Button, RichText};
use fc_storage::model::{Disk, DiskId};
use fc_storage::wipe::WipeMethod;

#[cfg(feature = "storage-destructive")]
use fc_storage::orchestrate::{Outcome, Request, WipeScope};
#[cfg(feature = "storage-destructive")]
use fc_storage::protocol::{self, PlanBody, WorkerEvent};
#[cfg(feature = "storage-destructive")]
use fc_storage::safety::Policy;
#[cfg(feature = "storage-destructive")]
use fc_storage::worker_client;
#[cfg(feature = "storage-destructive")]
use std::path::Path;
#[cfg(feature = "storage-destructive")]
use std::sync::mpsc;
#[cfg(feature = "storage-destructive")]
use std::time::{SystemTime, UNIX_EPOCH};

/// Where the packaged worker is installed. polkit checks this exact program, so it must be absolute.
#[cfg(feature = "storage-destructive")]
const WORKER_PATH: &str = "/usr/libexec/fcopy/fcopy-storage-worker";

/// A sealed plan is refused by the worker after this many seconds.
#[cfg(feature = "storage-destructive")]
const PLAN_LIFETIME_SECS: u64 = 300;

const METHODS: [(WipeMethod, &str); 3] = [
    (WipeMethod::Clear, "Clear (one pass of zeros)"),
    (WipeMethod::ClearRandom, "Random (one pass, then verify)"),
    (WipeMethod::Dod3Pass, "DoD 5220.22-M (three passes, legacy)"),
];

/// What the listing reports about the machine's disks.
#[derive(Clone)]
struct Listing {
    disks: Vec<Disk>,
    /// `None` when the system disks could not be determined. Erasing is then refused.
    system: Option<HashSet<DiskId>>,
}

/// Whether a disk may be offered for erasing, and if not, why. Pure, so it is tested
/// on every platform.
pub fn eligibility(disk: &Disk, system: Option<&HashSet<DiskId>>) -> Result<(), &'static str> {
    let Some(system) = system else {
        return Err("Fcopy cannot tell which disks host this system, so erasing is refused.");
    };
    if system.contains(&disk.id) {
        return Err("This disk hosts the running system.");
    }
    if disk.identity.serial.is_none() {
        return Err("This disk reports no serial number, so it cannot be confirmed.");
    }
    Ok(())
}

/// The request the worker will check. The identity is copied from the listing, so the
/// worker can compare it with what it finds when it opens the disk.
#[cfg(feature = "storage-destructive")]
pub fn build_request(disk: &Disk, typed_serial: &str, method: WipeMethod) -> Request {
    Request {
        disk: disk.id.clone(),
        confirmed: disk.identity.clone(),
        typed_serial: typed_serial.to_owned(),
        policy: Policy::default(),
        protected_paths: Vec::new(),
        scope: WipeScope::WholeDevice,
        method,
        table: None,
    }
}

#[cfg(target_os = "linux")]
fn load() -> Result<Listing, String> {
    use fc_storage::linux::LinuxSysfs;
    use fc_storage::platform::Platform;

    let host = LinuxSysfs::system();
    let disks = host
        .enumerate()
        .map_err(|e| format!("cannot list disks: {e}"))?;
    let system = host.system_disks();
    Ok(Listing { disks, system })
}

#[cfg(not(target_os = "linux"))]
fn load() -> Result<Listing, String> {
    Err("Disk listing is available on Linux in this build.".to_owned())
}

#[derive(Default)]
pub struct Storage {
    /// `None` until the first view, or after a refresh.
    listing: Option<Result<Listing, String>>,
    selected: Option<usize>,
    typed: String,
    method: usize,
    /// Carries the finished message from the worker thread.
    running: Option<Receiver<String>>,
    last: Option<String>,
}

impl Storage {
    /// Shows the tab. The worker result is polled each frame, so no other hook is needed.
    pub fn show(&mut self, ui: &mut egui::Ui) {
        self.poll(ui.ctx());
        if self.listing.is_none() {
            self.listing = Some(load());
        }

        ui.horizontal(|ui| {
            ui.heading("Disks");
            if ui
                .add_enabled(self.running.is_none(), Button::new("Refresh"))
                .clicked()
            {
                self.reset_selection();
                self.listing = None;
            }
        });
        ui.weak("Disks that host the running system cannot be erased. Erasing destroys all data on the disk.");
        ui.separator();

        let listing = match self.listing.clone() {
            Some(Ok(listing)) => listing,
            Some(Err(message)) => {
                ui.colored_label(ui.visuals().error_fg_color, message);
                return;
            }
            None => return,
        };

        if listing.disks.is_empty() {
            ui.label("No disks were found.");
            return;
        }

        egui::Grid::new("disk-list")
            .striped(true)
            .num_columns(6)
            .show(ui, |ui| {
                for h in ["Device", "Model", "Size", "Bus", "Removable", "Mounted at"] {
                    ui.strong(h);
                }
                ui.end_row();
                for (index, disk) in listing.disks.iter().enumerate() {
                    if ui
                        .selectable_label(self.selected == Some(index), &disk.os_path)
                        .clicked()
                    {
                        self.selected = Some(index);
                        self.typed.clear();
                    }
                    ui.label(&disk.identity.model);
                    ui.label(human_size(disk.identity.size_bytes));
                    ui.label(format!("{:?}", disk.transport));
                    ui.label(if disk.removable { "yes" } else { "no" });
                    let mounts = if disk.mounts.is_empty() {
                        "-".to_owned()
                    } else {
                        disk.mounts
                            .iter()
                            .map(|m| m.point.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    };
                    ui.label(mounts);
                    ui.end_row();
                }
            });

        ui.separator();
        match self.selected.and_then(|i| listing.disks.get(i)) {
            Some(disk) => self.erase_panel(ui, disk, listing.system.as_ref()),
            None => {
                ui.label("Select a disk to see whether it can be erased.");
            }
        }

        if let Some(message) = &self.last {
            ui.separator();
            ui.label(message);
        }
    }

    fn reset_selection(&mut self) {
        self.selected = None;
        self.typed.clear();
    }

    fn erase_panel(&mut self, ui: &mut egui::Ui, disk: &Disk, system: Option<&HashSet<DiskId>>) {
        ui.label(
            RichText::new(format!(
                "{} ({}) at {}",
                disk.identity.model,
                human_size(disk.identity.size_bytes),
                disk.os_path
            ))
            .strong(),
        );

        if let Err(reason) = eligibility(disk, system) {
            ui.colored_label(ui.visuals().error_fg_color, reason);
            return;
        }
        if !disk.mounts.is_empty() {
            ui.weak("Its mounted volumes will be unmounted before erasing.");
        }

        ui.horizontal(|ui| {
            ui.label("Method:");
            egui::ComboBox::from_id_salt("erase-method")
                .selected_text(METHODS[self.method].1)
                .show_ui(ui, |ui| {
                    for (i, (_, label)) in METHODS.iter().enumerate() {
                        ui.selectable_value(&mut self.method, i, *label);
                    }
                });
        });

        // eligibility() accepted this disk, so it has a serial.
        let serial = disk.identity.serial.clone().unwrap_or_default();
        ui.label(format!(
            "To confirm, type this disk's serial number exactly: {serial}"
        ));
        ui.text_edit_singleline(&mut self.typed);
        let matches = self.typed == serial;

        let busy = self.running.is_some();
        self.erase_button(ui, disk, matches && !busy, busy);
    }

    #[cfg(feature = "storage-destructive")]
    fn erase_button(&mut self, ui: &mut egui::Ui, disk: &Disk, enabled: bool, busy: bool) {
        let label = if busy {
            "Erasing (waiting for administrator)..."
        } else {
            "Erase disk"
        };
        if ui.add_enabled(enabled, Button::new(label)).clicked() {
            let request = build_request(disk, &self.typed, METHODS[self.method].0);
            self.running = Some(start(request));
            self.last = Some("Waiting for the administrator to authorize erasing...".to_owned());
        }
    }

    #[cfg(not(feature = "storage-destructive"))]
    fn erase_button(&mut self, ui: &mut egui::Ui, _disk: &Disk, _enabled: bool, _busy: bool) {
        ui.weak("This build of fcopy cannot erase disks. It was built without the storage-destructive feature.");
    }

    fn poll(&mut self, ctx: &egui::Context) {
        let Some(rx) = &self.running else { return };
        match rx.try_recv() {
            Ok(message) => {
                self.running = None;
                self.last = Some(message);
                // The disk may have changed; list it again.
                self.reset_selection();
                self.listing = None;
            }
            Err(TryRecvError::Empty) => ctx.request_repaint_after(Duration::from_millis(200)),
            Err(TryRecvError::Disconnected) => {
                self.running = None;
                self.last = Some(
                    "The erase stopped unexpectedly. Check the disk before using it.".to_owned(),
                );
            }
        }
    }
}

/// Starts the worker on a background thread. The channel receives the message to show.
#[cfg(feature = "storage-destructive")]
fn start(request: Request) -> Receiver<String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let message = match run_worker_for(request) {
            Ok(outcome) => describe(&outcome),
            Err(message) => message,
        };
        let _ = tx.send(message);
    });
    rx
}

#[cfg(feature = "storage-destructive")]
fn describe(outcome: &Outcome) -> String {
    match outcome {
        Outcome::Completed => "Erase completed and verified.".to_owned(),
        Outcome::Refused(reason) => format!("Erase refused: {reason:?}"),
        Outcome::Cancelled => "Erase cancelled.".to_owned(),
        Outcome::Failed(reason) => format!("Erase failed: {reason}"),
    }
}

#[cfg(feature = "storage-destructive")]
fn run_worker_for(request: Request) -> Result<Outcome, String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let key = protocol::new_key().map_err(|e| format!("cannot create an operation key: {e}"))?;
    let nonce = protocol::new_nonce().map_err(|e| format!("cannot create a nonce: {e}"))?;
    let body = PlanBody {
        expires_at_unix: now + PLAN_LIFETIME_SECS,
        nonce,
        request,
    };
    let envelope = protocol::seal(&key, &body).map_err(|e| format!("cannot seal the plan: {e}"))?;
    let command = worker_client::pkexec_command(Path::new(WORKER_PATH))
        .map_err(|e| format!("cannot start the worker: {e}"))?;
    let run = worker_client::run_worker(command, &key, &envelope)
        .map_err(|e| format!("the worker failed: {e}"))?;
    run.events
        .into_iter()
        .rev()
        .find_map(|event| match event {
            WorkerEvent::Finished(outcome) => Some(outcome),
            _ => None,
        })
        .ok_or_else(|| {
            format!(
                "the worker ended without a result (exit {:?})",
                run.exit_code
            )
        })
}

fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fc_storage::model::{DiskIdentity, Transport};

    fn disk(serial: Option<&str>) -> Disk {
        Disk::new(
            "/dev/sdb",
            Transport::Usb,
            true,
            DiskIdentity {
                serial: serial.map(str::to_owned),
                model: "Test stick".into(),
                size_bytes: 8 << 30,
            },
            512,
            Vec::new(),
        )
    }

    #[test]
    fn a_disk_with_an_unknown_system_set_is_refused() {
        assert!(eligibility(&disk(Some("A1")), None).is_err());
    }

    #[test]
    fn the_running_system_disk_is_refused() {
        let d = disk(Some("A1"));
        let system = HashSet::from([d.id.clone()]);
        assert_eq!(
            eligibility(&d, Some(&system)),
            Err("This disk hosts the running system.")
        );
    }

    #[test]
    fn a_disk_without_a_serial_is_refused() {
        assert!(eligibility(&disk(None), Some(&HashSet::new())).is_err());
    }

    #[test]
    fn an_ordinary_removable_disk_is_offered() {
        assert!(eligibility(&disk(Some("A1")), Some(&HashSet::new())).is_ok());
    }

    #[cfg(feature = "storage-destructive")]
    #[test]
    fn the_request_copies_the_identity_the_user_saw() {
        let d = disk(Some("A1"));
        let r = build_request(&d, "A1", WipeMethod::Clear);
        assert_eq!(r.disk, d.id);
        assert_eq!(r.confirmed, d.identity);
        assert_eq!(r.typed_serial, "A1");
        assert_eq!(r.scope, WipeScope::WholeDevice);
    }

    #[test]
    fn sizes_use_binary_units() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(8 << 30), "8.0 GiB");
    }
}

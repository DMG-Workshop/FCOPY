//! `fcopy-gui`: lightweight egui front end.
//!
//! ```text
//! fcopy-gui                                   # empty window; drop files onto it
//! fcopy-gui [--limit 20MB] [--verify] SRC... DEST   # start with a transfer queued
//! fcopy-gui --sources PATH...                 # pre-fill sources (file-manager menus)
//! ```
//!
//! The last form is what desktop integration (Explorer/Finder/Nautilus
//! "Copy with fcopy…") calls. Install that integration for the current user
//! with `fcopy-gui --install-integration` (undo: `--uninstall-integration`).
//!
//! # Event binding
//!
//! ```text
//!  engine worker ──Event──▶ mpsc ──▶ App::update(): drain all, AppState::apply()
//!        │                                  ▲
//!        └── on_event: ctx.request_repaint()┘   (wakes the UI only when needed)
//! ```
//!
//! * The UI thread **never blocks**: it drains with `try_recv()`.
//! * egui coalesces repaint requests, and telemetry is throttled to ~10 Hz at
//!   the source, so a 100 000-file job costs the same UI time as a 1-file job.
//! * While idle the app does not repaint at all (0 % CPU).

// Release builds on Windows are GUI-subsystem apps: no console window flashes up
// when launched from Explorer. Debug builds keep the console for logs.
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

mod app;
mod history;
mod integration;
mod remote;
mod profiles;
mod state;
mod torrent;
mod vpn;

use fc_engine::units::parse_size;
use std::path::PathBuf;

/// Transfer requested on the command line.
pub struct Startup {
    pub sources: Vec<PathBuf>,
    pub dest: Option<PathBuf>,
    pub limit: Option<u64>,
    pub verify: bool,
    pub torrent_dht: bool,
    pub torrent_trackers: bool,
}

fn parse_startup() -> Result<Startup, String> {
    let mut s = Startup {
        sources: vec![],
        dest: None,
        limit: None,
        verify: false,
        torrent_dht: false,
        torrent_trackers: false,
    };
    let mut paths = vec![];
    let mut all_sources = false;
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--limit" => s.limit = Some(parse_size(&it.next().ok_or("--limit needs a value")?)?),
            "--verify" => s.verify = true,
            "--torrent-dht" => s.torrent_dht = true,
            "--torrent-trackers" => s.torrent_trackers = true,
            // File-manager launches: every path is a source; the user picks the destination.
            "--sources" => all_sources = true,
            "--install-integration" | "--uninstall-integration" => {
                let result = if a == "--install-integration" { integration::install() } else { integration::uninstall() };
                match result {
                    Ok(paths) => {
                        for p in paths {
                            println!("{}", p.display());
                        }
                        std::process::exit(0);
                    }
                    Err(e) => {
                        eprintln!("{a}: {e}");
                        std::process::exit(1);
                    }
                }
            }
            "-h" | "--help" => return Err("usage: fcopy-gui [--limit RATE] [--verify] [--torrent-dht] [--torrent-trackers] [SOURCE... DEST]\n       fcopy-gui --sources PATH...      (pre-fill sources only)\n       fcopy-gui --install-integration | --uninstall-integration".into()),
            _ => paths.push(PathBuf::from(a)),
        }
    }
    if all_sources || paths.len() == 1 {
        s.sources = paths; // pre-fill sources, let the user pick a destination
    } else if paths.len() >= 2 {
        s.dest = paths.pop();
        s.sources = paths;
    }
    Ok(s)
}

fn main() -> eframe::Result<()> {
    let startup = match parse_startup() {
        Ok(s) => s,
        Err(msg) => {
            eprintln!("{msg}");
            std::process::exit(2);
        }
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("fcopy")
            .with_app_id(integration::APP_ID) // Wayland app id: must match the .desktop file name
            .with_inner_size([900.0, 620.0])
            .with_min_inner_size([640.0, 420.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native(
        "fcopy",
        options,
        Box::new(move |cc| Ok(Box::new(app::FcApp::new(cc, startup)))),
    )
}

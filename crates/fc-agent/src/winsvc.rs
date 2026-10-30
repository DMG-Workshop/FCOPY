//! Windows service support: install, remove, and run under the Service Control Manager.
//!
//! The service writes timestamped diagnostics to `service.log` in its state
//! directory, rotating one previous file to `service.log.1` at 2 MiB.

use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode,
    ServiceInfo, ServiceStartType, ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_dispatcher;
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{Result as WinResult, define_windows_service};

use fc_remote::config::AgentConfig;
use fc_remote::server::Server;
use fc_remote::tls::Identity;

/// The name the Service Control Manager knows the service by.
pub const SERVICE_NAME: &str = "fcopy-agent";

/// Set before the dispatcher starts; the service entry point reads it.
static STATE_DIR: OnceLock<PathBuf> = OnceLock::new();
const LOG_NAME: &str = "service.log";
const LOG_ARCHIVE_NAME: &str = "service.log.1";
const MAX_LOG_BYTES: u64 = 2 * 1024 * 1024;

define_windows_service!(ffi_service_main, service_main);

/// Hand this process to the Service Control Manager. Blocks until the service stops.
pub fn run(dir: PathBuf) -> Result<(), String> {
    let _ = STATE_DIR.set(dir);
    service_dispatcher::start(SERVICE_NAME, ffi_service_main).map_err(|e| e.to_string())
}

fn service_main(_args: Vec<OsString>) {
    // With no console, retain dispatcher failures in the state directory.
    if let Err(error) = run_service() {
        let dir = STATE_DIR
            .get()
            .cloned()
            .unwrap_or_else(fc_remote::config::default_agent_dir);
        log_service(&dir, &format!("service dispatcher failed: {error}"));
    }
}

fn run_service() -> WinResult<()> {
    let dir = STATE_DIR
        .get()
        .cloned()
        .unwrap_or_else(fc_remote::config::default_agent_dir);
    log_service(&dir, "service process started");
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let status = service_control_handler::register(SERVICE_NAME, move |control| match control {
        ServiceControl::Stop => {
            let _ = stop_tx.send(());
            ServiceControlHandlerResult::NoError
        }
        ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
        _ => ServiceControlHandlerResult::NotImplemented,
    })?;

    let running = |state| ServiceStatus {
        service_type: ServiceType::OWN_PROCESS,
        current_state: state,
        controls_accepted: ServiceControlAccept::STOP,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    };
    status.set_service_status(running(ServiceState::Running))?;

    let result = run_agent(&dir, stop_rx);
    match &result {
        Ok(()) => log_service(&dir, "service stopped cleanly"),
        Err(error) => log_service(&dir, &format!("service failed: {error}")),
    }

    let mut stopped = running(ServiceState::Stopped);
    if result.is_err() {
        stopped.exit_code = ServiceExitCode::Win32(1);
    }
    status.set_service_status(stopped)?;
    Ok(())
}

fn run_agent(dir: &Path, stop_rx: mpsc::Receiver<()>) -> Result<(), String> {
    let cfg = AgentConfig::load(&dir.join("agent.toml"))
        .map_err(|e| format!("loading agent.toml: {e}"))?;
    let identity =
        Identity::load_or_create(dir, &cfg.name).map_err(|e| format!("loading identity: {e}"))?;
    let runtime =
        tokio::runtime::Runtime::new().map_err(|e| format!("creating Tokio runtime: {e}"))?;
    runtime.block_on(async move {
        let server =
            Server::bind(cfg, &identity).map_err(|e| format!("binding QUIC server: {e}"))?;
        let handle = server.handle();
        let addr = handle
            .local_addr()
            .map_err(|e| format!("reading listen address: {e}"))?;
        log_service(dir, &format!("QUIC server listening on {addr}"));
        let server_task = tokio::spawn(server.run());
        tokio::task::spawn_blocking(move || {
            let _ = stop_rx.recv();
            handle.shutdown();
        })
        .await
        .map_err(|e| format!("waiting for stop request: {e}"))?;
        server_task
            .await
            .map_err(|e| format!("QUIC server task failed: {e}"))?;
        Ok(())
    })
}

/// Append a timestamped diagnostic, keeping the active log and one rotated file.
/// Logging is best-effort so a log disk/permission issue cannot prevent service startup.
fn log_service(dir: &Path, message: &str) {
    let Ok(()) = fs::create_dir_all(dir) else {
        return;
    };
    let path = dir.join(LOG_NAME);
    let archive = dir.join(LOG_ARCHIVE_NAME);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let record = format!("{timestamp} {message}\n");

    if fs::metadata(&path)
        .is_ok_and(|metadata| metadata.len() + record.len() as u64 > MAX_LOG_BYTES)
    {
        let _ = fs::remove_file(&archive);
        if fs::rename(&path, &archive).is_err() {
            return;
        }
    }
    let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) else {
        return;
    };
    let _ = file.write_all(record.as_bytes());
}

/// Register the service to start automatically, running `serve` on `dir`.
pub fn install(dir: &Path) -> Result<(), String> {
    let manager =
        ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CREATE_SERVICE)
            .map_err(|e| e.to_string())?;
    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from("fcopy agent"),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: std::env::current_exe().map_err(|e| e.to_string())?,
        launch_arguments: vec![
            OsString::from("--dir"),
            dir.as_os_str().to_owned(),
            OsString::from("service"),
            OsString::from("run"),
        ],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    manager
        .create_service(&info, ServiceAccess::CHANGE_CONFIG)
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Stop (if running) and remove the service.
pub fn uninstall() -> Result<(), String> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT)
        .map_err(|e| e.to_string())?;
    let service = manager
        .open_service(SERVICE_NAME, ServiceAccess::STOP | ServiceAccess::DELETE)
        .map_err(|e| e.to_string())?;
    let _ = service.stop();
    service.delete().map_err(|e| e.to_string())
}

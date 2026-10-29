//! Windows service support: install, remove, and run under the Service Control Manager.
//!
//! Not run on this development machine (installing needs an elevated shell),
//! so treat this module as compiled but not yet exercised by a test.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::mpsc;
use std::time::Duration;

use windows_service::service::{
    ServiceAccess, ServiceControl, ServiceControlAccept, ServiceErrorControl, ServiceExitCode, ServiceInfo, ServiceStartType,
    ServiceState, ServiceStatus, ServiceType,
};
use windows_service::service_control_handler::{self, ServiceControlHandlerResult};
use windows_service::service_dispatcher;
use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};
use windows_service::{define_windows_service, Result as WinResult};

use fc_remote::config::AgentConfig;
use fc_remote::server::Server;
use fc_remote::tls::Identity;

/// The name the Service Control Manager knows the service by.
pub const SERVICE_NAME: &str = "fcopy-agent";

/// Set before the dispatcher starts; the service entry point reads it.
static STATE_DIR: OnceLock<PathBuf> = OnceLock::new();

define_windows_service!(ffi_service_main, service_main);

/// Hand this process to the Service Control Manager. Blocks until the service stops.
pub fn run(dir: PathBuf) -> Result<(), String> {
    let _ = STATE_DIR.set(dir);
    service_dispatcher::start(SERVICE_NAME, ffi_service_main).map_err(|e| e.to_string())
}

fn service_main(_args: Vec<OsString>) {
    // With no console there is nowhere useful to print, so errors just end the service.
    let _ = run_service();
}

fn run_service() -> WinResult<()> {
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

    let dir = STATE_DIR.get().cloned().unwrap_or_else(fc_remote::config::default_agent_dir);
    if let Ok(cfg) = AgentConfig::load(&dir.join("agent.toml")) {
        if let Ok(identity) = Identity::load_or_create(&dir, &cfg.name) {
            if let Ok(runtime) = tokio::runtime::Runtime::new() {
                runtime.block_on(async {
                    if let Ok(server) = Server::bind(cfg, &identity) {
                        let handle = server.handle();
                        tokio::spawn(server.run());
                        let _ = tokio::task::spawn_blocking(move || {
                            let _ = stop_rx.recv();
                            handle.shutdown();
                        })
                        .await;
                    }
                });
            }
        }
    }

    status.set_service_status(running(ServiceState::Stopped))?;
    Ok(())
}

/// Register the service to start automatically, running `serve` on `dir`.
pub fn install(dir: &Path) -> Result<(), String> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CREATE_SERVICE).map_err(|e| e.to_string())?;
    let info = ServiceInfo {
        name: OsString::from(SERVICE_NAME),
        display_name: OsString::from("fcopy agent"),
        service_type: ServiceType::OWN_PROCESS,
        start_type: ServiceStartType::AutoStart,
        error_control: ServiceErrorControl::Normal,
        executable_path: std::env::current_exe().map_err(|e| e.to_string())?,
        launch_arguments: vec![OsString::from("--dir"), dir.as_os_str().to_owned(), OsString::from("service"), OsString::from("run")],
        dependencies: vec![],
        account_name: None, // LocalSystem
        account_password: None,
    };
    manager.create_service(&info, ServiceAccess::CHANGE_CONFIG).map_err(|e| e.to_string())?;
    Ok(())
}

/// Stop (if running) and remove the service.
pub fn uninstall() -> Result<(), String> {
    let manager = ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).map_err(|e| e.to_string())?;
    let service = manager
        .open_service(SERVICE_NAME, ServiceAccess::STOP | ServiceAccess::DELETE)
        .map_err(|e| e.to_string())?;
    let _ = service.stop();
    service.delete().map_err(|e| e.to_string())
}

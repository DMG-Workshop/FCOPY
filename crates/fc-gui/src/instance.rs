//! One queue per client configuration directory, with authenticated loopback handoff.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::Startup;

const MAX_MESSAGE: usize = 1024 * 1024;
const MAX_PENDING: usize = 32;
const IO_TIMEOUT: Duration = Duration::from_secs(2);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Endpoint {
    port: u16,
    pid: u32,
    token: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    token: String,
    startup: Startup,
}

pub enum Connection {
    Primary(Instance),
    Forwarded,
}

struct Window {
    context: egui::Context,
    #[cfg(windows)]
    handle: isize,
}

pub struct Instance {
    // Keep ownership until the listener has stopped, including during unwinding.
    _lock: File,
    receiver: mpsc::Receiver<Startup>,
    context: Arc<Mutex<Option<Window>>>,
    pending: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}

pub fn connect(directory: &Path, startup: &Startup) -> io::Result<Connection> {
    std::fs::create_dir_all(directory)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join("desktop-instance.lock"))?;
    let endpoint_path = directory.join("desktop-instance.toml");
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        match fs4::FileExt::try_lock(&lock) {
            Ok(()) => {
                let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
                listener.set_nonblocking(true)?;
                let endpoint = Endpoint {
                    port: listener.local_addr()?.port(),
                    pid: std::process::id(),
                    token: fc_remote::crypto::generate_access_key(),
                };
                let text = toml::to_string(&endpoint).map_err(io::Error::other)?;
                fc_remote::config::write_private(&endpoint_path, text.as_bytes())?;
                return Instance::start(lock, listener, endpoint.token).map(Connection::Primary);
            }
            Err(fs4::TryLockError::WouldBlock) => {}
            Err(error) => return Err(error.into()),
        }
        // A primary can hold the lock before it has published its endpoint.
        let endpoint = std::fs::read_to_string(&endpoint_path)
            .and_then(|text| toml::from_str::<Endpoint>(&text).map_err(io::Error::other));
        let error = match endpoint {
            Ok(endpoint) => {
                let address = SocketAddr::from((Ipv4Addr::LOCALHOST, endpoint.port));
                match TcpStream::connect_timeout(&address, Duration::from_millis(200)) {
                    Ok(mut stream) => {
                        allow_foreground(endpoint.pid);
                        // Never retry after sending: a lost acknowledgement must not duplicate a job.
                        forward(&mut stream, endpoint.token, startup.clone())?;
                        return Ok(Connection::Forwarded);
                    }
                    Err(error) => error,
                }
            }
            Err(error) => error,
        };
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                error.kind(),
                format!("Existing FCOPY is not responding: {error}"),
            ));
        }
        thread::sleep(Duration::from_millis(25));
    }
}

impl Instance {
    fn start(lock: File, listener: TcpListener, token: String) -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(MAX_PENDING);
        let context = Arc::new(Mutex::new(None::<Window>));
        let pending = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_context = context.clone();
        let worker_pending = pending.clone();
        let worker_stop = stop.clone();
        let worker = thread::Builder::new()
            .name("desktop-handoff".into())
            .spawn(move || {
                while !worker_stop.load(Ordering::Acquire) {
                    match listener.accept() {
                        Ok((mut stream, _)) => {
                            if let Err(error) = receive(
                                &mut stream,
                                &token,
                                &sender,
                                &worker_pending,
                                &worker_context,
                            ) {
                                eprintln!("FCOPY desktop handoff: {error}");
                            }
                        }
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            thread::sleep(Duration::from_millis(25));
                        }
                        Err(error) => {
                            eprintln!("FCOPY desktop listener failed: {error}");
                            break;
                        }
                    }
                }
            })?;
        Ok(Self {
            _lock: lock,
            receiver,
            context,
            pending,
            stop,
            worker: Some(worker),
        })
    }

    pub fn attach(&self, cc: &eframe::CreationContext<'_>) -> io::Result<()> {
        #[cfg(windows)]
        let handle = {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            match cc
                .window_handle()
                .map_err(|error| io::Error::other(error.to_string()))?
                .as_raw()
            {
                RawWindowHandle::Win32(handle) => handle.hwnd.get(),
                _ => {
                    return Err(io::Error::other(
                        "FCOPY did not receive a Windows window handle",
                    ));
                }
            }
        };
        let window = Window {
            context: cc.egui_ctx.clone(),
            #[cfg(windows)]
            handle,
        };
        let mut context = self.context.lock().expect("desktop context lock");
        if self.pending() > 0 {
            activate(&window);
        }
        *context = Some(window);
        Ok(())
    }

    pub fn pending(&self) -> usize {
        self.pending.load(Ordering::Acquire)
    }

    pub fn try_recv(&self) -> Result<Option<Startup>, String> {
        match self.receiver.try_recv() {
            Ok(startup) => {
                self.pending.fetch_sub(1, Ordering::AcqRel);
                Ok(Some(startup))
            }
            Err(mpsc::TryRecvError::Empty) => Ok(None),
            Err(mpsc::TryRecvError::Disconnected) => Err(
                "The desktop launch listener stopped. Restart FCOPY to accept file-manager launches.".into()
            ),
        }
    }

    pub fn shutdown(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                eprintln!("FCOPY desktop handoff thread panicked");
            }
        }
    }
}

impl Drop for Instance {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn activate(window: &Window) {
    #[cfg(windows)]
    unsafe {
        use windows_sys::Win32::UI::WindowsAndMessaging::{
            FlashWindow, IsIconic, IsWindowVisible, SW_RESTORE, SW_SHOW, SetForegroundWindow,
            ShowWindowAsync,
        };
        let handle = window.handle as windows_sys::Win32::Foundation::HWND;
        // A minimized native window may not render egui viewport commands.
        // Restore it through Win32 first, without blocking on the UI thread.
        if IsIconic(handle) != 0 {
            ShowWindowAsync(handle, SW_RESTORE);
        }
        if IsWindowVisible(handle) == 0 {
            ShowWindowAsync(handle, SW_SHOW);
        }
        if SetForegroundWindow(handle) == 0 {
            eprintln!("FCOPY foreground activation was declined; requesting taskbar attention");
            FlashWindow(handle, 1);
        }
    }
    let context = &window.context;
    context.send_viewport_cmd(egui::ViewportCommand::Visible(true));
    context.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
    context.send_viewport_cmd(egui::ViewportCommand::Focus);
    context.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(
        egui::UserAttentionType::Informational,
    ));
    context.request_repaint();
}

fn receive(
    stream: &mut TcpStream,
    token: &str,
    sender: &mpsc::SyncSender<Startup>,
    pending: &AtomicUsize,
    context: &Mutex<Option<Window>>,
) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let deadline = Instant::now() + IO_TIMEOUT;
    let mut length = [0; 4];
    read_until(stream, &mut length, deadline)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > MAX_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Launch message exceeds 1 MiB",
        ));
    }
    let mut body = vec![0; length];
    read_until(stream, &mut body, deadline)?;
    let text = std::str::from_utf8(&body).map_err(io::Error::other)?;
    let request: Request = toml::from_str(text).map_err(|error| {
        io::Error::other(format!("Invalid launch request: {}", error.message()))
    })?;
    if request.token != token {
        stream.write_all(&[3])?;
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Unauthenticated launch rejected",
        ));
    }
    if request
        .startup
        .sources
        .iter()
        .chain(request.startup.dest.iter())
        .any(|p| !p.is_absolute())
    {
        stream.write_all(&[4])?;
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Launch paths must be absolute",
        ));
    }
    if request.startup != Startup::default() {
        pending.fetch_add(1, Ordering::AcqRel);
        if let Err(error) = sender.try_send(request.startup) {
            pending.fetch_sub(1, Ordering::AcqRel);
            stream.write_all(&[2])?;
            return Err(io::Error::other(format!(
                "Launch inbox unavailable: {error}"
            )));
        }
    }
    if let Some(context) = context.lock().expect("desktop context lock").as_ref() {
        activate(context);
    }
    stream.write_all(&[1])
}

fn read_until(stream: &mut TcpStream, mut buffer: &mut [u8], deadline: Instant) -> io::Result<()> {
    while !buffer.is_empty() {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "Launch message timed out"))?;
        stream.set_read_timeout(Some(remaining))?;
        match stream.read(buffer) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Incomplete launch message",
                ));
            }
            Ok(count) => buffer = &mut buffer[count..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn forward(stream: &mut TcpStream, token: String, startup: Startup) -> io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let body = toml::to_string(&Request { token, startup }).map_err(io::Error::other)?;
    if body.len() > MAX_MESSAGE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Too many selected paths (launch exceeds 1 MiB)",
        ));
    }
    stream.write_all(&(body.len() as u32).to_be_bytes())?;
    stream.write_all(body.as_bytes())?;
    let mut response = [0];
    stream.read_exact(&mut response).map_err(|error| io::Error::new(
        error.kind(), format!("Launch acknowledgement was lost; check the existing window before retrying: {error}")
    ))?;
    match response[0] {
        1 => Ok(()),
        2 => Err(io::Error::other(
            "The existing window has too many pending launches or is closing",
        )),
        3 => Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "The existing window rejected launch authentication",
        )),
        _ => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "The existing window rejected the launch",
        )),
    }
}

fn allow_foreground(pid: u32) {
    #[cfg(windows)]
    {
        // The user-launched secondary grants its foreground permission to the primary.
        if unsafe { windows_sys::Win32::UI::WindowsAndMessaging::AllowSetForegroundWindow(pid) }
            == 0
        {
            eprintln!(
                "FCOPY could not grant foreground permission: {}",
                io::Error::last_os_error()
            );
        }
    }
    #[cfg(not(windows))]
    let _ = pid;
}

pub fn report_startup_error(message: &str) {
    eprintln!("{message}");
    #[cfg(all(windows, not(debug_assertions)))]
    {
        let text: Vec<u16> = message.encode_utf16().chain(Some(0)).collect();
        let title: Vec<u16> = "FCOPY".encode_utf16().chain(Some(0)).collect();
        unsafe {
            windows_sys::Win32::UI::WindowsAndMessaging::MessageBoxW(
                std::ptr::null_mut(),
                text.as_ptr(),
                title.as_ptr(),
                windows_sys::Win32::UI::WindowsAndMessaging::MB_OK
                    | windows_sys::Win32::UI::WindowsAndMessaging::MB_ICONERROR,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn primary(directory: &Path) -> Instance {
        match connect(directory, &Startup::default()).unwrap() {
            Connection::Primary(instance) => instance,
            Connection::Forwarded => panic!("Expected primary"),
        }
    }

    #[test]
    fn second_launch_delivers_paths_and_options_without_a_second_owner() {
        let directory = tempfile::tempdir().unwrap();
        let first = primary(directory.path());
        let startup = Startup {
            sources: vec![
                directory.path().join("space and \u{00e9}.txt"),
                directory.path().join("other"),
            ],
            dest: Some(directory.path().join("destination")),
            verify: true,
            limit: Some(20_000_000),
            ..Default::default()
        };
        assert!(matches!(
            connect(directory.path(), &startup).unwrap(),
            Connection::Forwarded
        ));
        assert_eq!(first.pending(), 1);
        assert_eq!(first.try_recv().unwrap(), Some(startup));
        assert_eq!(first.pending(), 0);
        assert!(matches!(
            connect(directory.path(), &Startup::default()).unwrap(),
            Connection::Forwarded
        ));
        assert_eq!(first.try_recv().unwrap(), None);
        drop(first);
        // A stale endpoint is replaced only after taking ownership.
        drop(primary(directory.path()));
    }

    #[test]
    fn unauthenticated_relative_and_oversized_requests_do_not_enter_the_inbox() {
        let directory = tempfile::tempdir().unwrap();
        let first = primary(directory.path());
        let endpoint: Endpoint = toml::from_str(
            &std::fs::read_to_string(directory.path().join("desktop-instance.toml")).unwrap(),
        )
        .unwrap();
        let address = (Ipv4Addr::LOCALHOST, endpoint.port);
        let mut stream = TcpStream::connect(address).unwrap();
        assert!(forward(&mut stream, "wrong".into(), Startup::default()).is_err());
        let mut stream = TcpStream::connect(address).unwrap();
        assert!(
            forward(
                &mut stream,
                endpoint.token,
                Startup {
                    sources: vec!["relative.txt".into()],
                    ..Default::default()
                }
            )
            .is_err()
        );
        let mut stream = TcpStream::connect(address).unwrap();
        stream.set_read_timeout(Some(IO_TIMEOUT)).unwrap();
        stream
            .write_all(&((MAX_MESSAGE + 1) as u32).to_be_bytes())
            .unwrap();
        assert!(stream.read_exact(&mut [0]).is_err());
        assert_eq!(first.pending(), 0);
        assert_eq!(first.try_recv().unwrap(), None);
        assert!(matches!(
            connect(directory.path(), &Startup::default()).unwrap(),
            Connection::Forwarded
        ));
    }

    #[test]
    fn inbox_is_bounded_and_rejection_does_not_create_another_queue() {
        let directory = tempfile::tempdir().unwrap();
        let first = primary(directory.path());
        let startup = Startup {
            sources: vec![directory.path().join("selected.txt")],
            ..Default::default()
        };
        for _ in 0..MAX_PENDING {
            assert!(matches!(
                connect(directory.path(), &startup).unwrap(),
                Connection::Forwarded
            ));
        }
        assert!(connect(directory.path(), &startup).is_err());
        assert_eq!(first.pending(), MAX_PENDING);
        assert!(matches!(
            connect(directory.path(), &Startup::default()).unwrap(),
            Connection::Forwarded
        ));
        assert_eq!(first.pending(), MAX_PENDING);
        first.try_recv().unwrap().unwrap();
        assert!(matches!(
            connect(directory.path(), &startup).unwrap(),
            Connection::Forwarded
        ));
    }

    #[test]
    fn launch_paths_are_resolved_in_the_senders_working_directory() {
        let mut startup = Startup {
            sources: vec!["source.txt".into()],
            dest: Some("destination".into()),
            ..Default::default()
        };
        startup.absolute_paths().unwrap();
        assert_eq!(
            startup.sources,
            vec![std::env::current_dir().unwrap().join("source.txt")]
        );
        assert_eq!(
            startup.dest,
            Some(std::env::current_dir().unwrap().join("destination"))
        );
    }

    #[test]
    fn a_slow_sender_cannot_extend_the_total_read_deadline() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let mut writer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (mut reader, _) = listener.accept().unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let sender_stop = stop.clone();
        let sender = thread::spawn(move || {
            while !sender_stop.load(Ordering::Acquire) {
                if writer.write_all(b"x").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
        });
        let started = Instant::now();
        let result = read_until(
            &mut reader,
            &mut [0; 1024],
            started + Duration::from_millis(150),
        );
        stop.store(true, Ordering::Release);
        sender.join().unwrap();
        assert!(result.is_err());
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    #[test]
    fn simultaneous_launches_elect_exactly_one_primary() {
        let directory = tempfile::tempdir().unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let directory = directory.path().to_owned();
                let barrier = barrier.clone();
                thread::spawn(move || {
                    barrier.wait();
                    connect(&directory, &Startup::default()).unwrap()
                })
            })
            .collect();
        let connections: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(
            connections
                .iter()
                .filter(|c| matches!(c, Connection::Primary(_)))
                .count(),
            1
        );
        let primary = connections
            .iter()
            .find_map(|c| match c {
                Connection::Primary(primary) => Some(primary),
                Connection::Forwarded => None,
            })
            .unwrap();
        assert_eq!(primary.pending(), 0);
    }

    // Invoked by the process test with a private temporary directory.
    #[test]
    fn process_probe() {
        let Some(directory) = std::env::var_os("FCOPY_INSTANCE_TEST_DIRECTORY") else {
            return;
        };
        let directory = std::path::PathBuf::from(directory);
        if std::env::var_os("FCOPY_INSTANCE_TEST_HOLD").is_some() {
            let _primary = primary(&directory);
            std::fs::write(directory.join("ready"), b"ready").unwrap();
            thread::sleep(Duration::from_secs(30));
        } else {
            let startup = Startup {
                sources: vec![directory.join("child selection.txt")],
                ..Default::default()
            };
            assert!(matches!(
                connect(&directory, &startup).unwrap(),
                Connection::Forwarded
            ));
        }
    }

    #[test]
    fn real_process_handoff_and_recovery_after_abrupt_owner_exit() {
        struct Child(std::process::Child);
        impl Drop for Child {
            fn drop(&mut self) {
                if self.0.try_wait().unwrap().is_none() {
                    self.0.kill().unwrap();
                    self.0.wait().unwrap();
                }
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let first = primary(directory.path());
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "instance::tests::process_probe", "--nocapture"])
            .env("FCOPY_INSTANCE_TEST_DIRECTORY", directory.path())
            .env_remove("FCOPY_INSTANCE_TEST_HOLD");
        assert!(command.status().unwrap().success());
        assert_eq!(
            first.try_recv().unwrap().unwrap().sources,
            vec![directory.path().join("child selection.txt")]
        );
        drop(first);
        let mut child = Child(
            command
                .env("FCOPY_INSTANCE_TEST_HOLD", "1")
                .spawn()
                .unwrap(),
        );
        let deadline = Instant::now() + STARTUP_TIMEOUT;
        while !directory.path().join("ready").exists() {
            assert!(Instant::now() < deadline, "Child primary did not start");
            thread::sleep(Duration::from_millis(25));
        }
        assert!(matches!(
            connect(directory.path(), &Startup::default()).unwrap(),
            Connection::Forwarded
        ));
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        drop(primary(directory.path()));
    }
}

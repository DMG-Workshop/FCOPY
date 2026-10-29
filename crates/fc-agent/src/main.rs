//! `fcopy-agent`: receives files from fcopy clients over QUIC.
//!
//! ```text
//! fcopy-agent init --name nas [--port 47800]      create the identity and agent.toml
//! fcopy-agent serve                               run in the foreground until Ctrl-C
//! fcopy-agent fingerprint                         print this agent's identity
//! fcopy-agent share add backup /srv/backup [--read-only]
//! fcopy-agent user add alice --shares backup      password from a prompt or FCOPY_PASSWORD
//! fcopy-agent unattended enable scripts --shares backup   prints the access key once
//! fcopy-agent allow add 192.168.1.0/24
//! ```
//!
//! The state directory is `--dir`, or `FCOPY_AGENT_DIR`, or the platform
//! default from `fc_remote::config::default_agent_dir`.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};

use fc_remote::config::{AccountKind, AgentConfig, Share, default_agent_dir};
use fc_remote::crypto::generate_access_key;
use fc_remote::protocol::DEFAULT_PORT;
use fc_remote::server::Server;
use fc_remote::tls::Identity;

/// Shortest interactive password the agent accepts.
const MIN_PASSWORD: usize = 8;

#[derive(Parser)]
#[command(name = "fcopy-agent", version, about = "Receive files from fcopy clients over QUIC")]
struct Cli {
    /// State directory. Defaults to FCOPY_AGENT_DIR or the platform location.
    #[arg(long, global = true, value_name = "DIR")]
    dir: Option<PathBuf>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Create the identity and a config file. Refuses to overwrite one.
    Init {
        /// Name shown to clients.
        #[arg(long)]
        name: String,
        /// UDP port to listen on.
        #[arg(long, default_value_t = DEFAULT_PORT)]
        port: u16,
    },
    /// Run in the foreground until Ctrl-C.
    Serve,
    /// Print the identity fingerprint, to compare with the client.
    Fingerprint,
    /// Manage shares: the folders clients may write into.
    Share {
        #[command(subcommand)]
        cmd: ShareCmd,
    },
    /// Manage interactive accounts (a person types a password).
    User {
        #[command(subcommand)]
        cmd: UserCmd,
    },
    /// Manage unattended accounts (a long key for scripts).
    Unattended {
        #[command(subcommand)]
        cmd: UnattendedCmd,
    },
    /// Manage the addresses allowed to connect.
    Allow {
        #[command(subcommand)]
        cmd: AllowCmd,
    },
    /// Windows service: install, remove, or run under the Service Control Manager.
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
    },
}

#[derive(Subcommand)]
enum ShareCmd {
    /// Add or replace a share.
    Add {
        name: String,
        path: PathBuf,
        #[arg(long)]
        read_only: bool,
    },
    /// Remove a share.
    Del { name: String },
    List,
}

#[derive(Subcommand)]
enum UserCmd {
    /// Add or replace an account. The password is read from a prompt, or from FCOPY_PASSWORD.
    Add {
        name: String,
        /// Shares this account may use. Omit for all shares.
        #[arg(long, value_delimiter = ',')]
        shares: Option<Vec<String>>,
    },
    /// Remove an account.
    Del { name: String },
    List,
}

#[derive(Subcommand)]
enum UnattendedCmd {
    /// Create an access key for scripts. The key is printed once and not stored.
    Enable {
        name: String,
        #[arg(long, value_delimiter = ',')]
        shares: Option<Vec<String>>,
    },
}

#[derive(Subcommand)]
enum ServiceCmd {
    /// Register the Windows service to start with the machine.
    Install,
    /// Remove the Windows service.
    Uninstall,
    /// Run under the Service Control Manager. Started by Windows, not by hand.
    Run,
}

#[derive(Subcommand)]
enum AllowCmd {
    /// Allow an address or CIDR range.
    Add { cidr: String },
    /// Stop allowing an address or CIDR range.
    Del { cidr: String },
    List,
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let dir = cli.dir.clone().or_else(|| std::env::var_os("FCOPY_AGENT_DIR").map(PathBuf::from)).unwrap_or_else(default_agent_dir);
    match run(&dir, cli.cmd) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("fcopy-agent: {e}");
            ExitCode::FAILURE
        }
    }
}

fn config_path(dir: &Path) -> PathBuf {
    dir.join("agent.toml")
}

fn load_config(dir: &Path) -> Result<AgentConfig, String> {
    AgentConfig::load(&config_path(dir)).map_err(|e| format!("cannot read {}: {e} (run `init` first?)", config_path(dir).display()))
}

fn save_config(dir: &Path, cfg: &AgentConfig) -> Result<(), String> {
    cfg.save(&config_path(dir)).map_err(|e| e.to_string())
}

fn hostname() -> String {
    ["COMPUTERNAME", "HOSTNAME"].iter().find_map(|v| std::env::var(v).ok().filter(|s| !s.is_empty())).unwrap_or_else(|| "fcopy-agent".into())
}

/// The password from FCOPY_PASSWORD, or typed twice at a prompt.
fn read_password() -> Result<String, String> {
    if let Ok(p) = std::env::var("FCOPY_PASSWORD") {
        return Ok(p);
    }
    let first = rpassword::prompt_password("Password: ").map_err(|e| e.to_string())?;
    let second = rpassword::prompt_password("Repeat password: ").map_err(|e| e.to_string())?;
    if first != second {
        return Err("the two passwords do not match".into());
    }
    Ok(first)
}

fn run(dir: &Path, cmd: Cmd) -> Result<(), String> {
    match cmd {
        Cmd::Init { name, port } => {
            if config_path(dir).exists() {
                return Err(format!("{} already exists; remove it to start over", config_path(dir).display()));
            }
            let identity = Identity::load_or_create(dir, &hostname()).map_err(|e| e.to_string())?;
            let mut cfg = AgentConfig::new(name);
            cfg.listen = std::net::SocketAddr::from(([0, 0, 0, 0], port));
            save_config(dir, &cfg)?;
            println!("created {}", dir.display());
            println!("fingerprint: {}", identity.fingerprint());
            println!("next: add a share (`share add`) and an account (`user add`), then `serve`");
            Ok(())
        }
        Cmd::Serve => serve(dir),
        Cmd::Fingerprint => {
            let identity = Identity::load_or_create(dir, &hostname()).map_err(|e| e.to_string())?;
            println!("{}", identity.fingerprint());
            Ok(())
        }
        Cmd::Share { cmd } => share(dir, cmd),
        Cmd::User { cmd } => user(dir, cmd),
        Cmd::Unattended { cmd } => unattended(dir, cmd),
        Cmd::Allow { cmd } => allow(dir, cmd),
        Cmd::Service { cmd } => service(dir, cmd),
    }
}

fn serve(dir: &Path) -> Result<(), String> {
    let cfg = load_config(dir)?;
    let identity = Identity::load_or_create(dir, &cfg.name).map_err(|e| e.to_string())?;
    let runtime = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    runtime.block_on(async {
        let server = Server::bind(cfg, &identity).map_err(|e| e.to_string())?;
        let handle = server.handle();
        let addr = handle.local_addr().map_err(|e| e.to_string())?;
        println!("listening on {addr}");
        println!("fingerprint: {}", handle.fingerprint());
        let stop = handle.clone();
        ctrlc::set_handler(move || stop.shutdown()).map_err(|e| e.to_string())?;
        server.run().await;
        println!("stopped");
        Ok(())
    })
}

fn share(dir: &Path, cmd: ShareCmd) -> Result<(), String> {
    let mut cfg = load_config(dir)?;
    match cmd {
        ShareCmd::Add { name, path, read_only } => {
            cfg.shares.retain(|s| s.name != name);
            cfg.shares.push(Share { name: name.clone(), path, writable: !read_only });
            save_config(dir, &cfg)?;
            println!("share '{name}' saved");
        }
        ShareCmd::Del { name } => {
            let before = cfg.shares.len();
            cfg.shares.retain(|s| s.name != name);
            if cfg.shares.len() == before {
                return Err(format!("no share named '{name}'"));
            }
            save_config(dir, &cfg)?;
            println!("share '{name}' removed");
        }
        ShareCmd::List => {
            for s in &cfg.shares {
                println!("{}\t{}\t{}", s.name, if s.writable { "read-write" } else { "read-only" }, s.path.display());
            }
        }
    }
    Ok(())
}

fn user(dir: &Path, cmd: UserCmd) -> Result<(), String> {
    let mut cfg = load_config(dir)?;
    match cmd {
        UserCmd::Add { name, shares } => {
            let password = read_password()?;
            if password.chars().count() < MIN_PASSWORD {
                return Err(format!("password must be at least {MIN_PASSWORD} characters"));
            }
            cfg.set_account(&name, AccountKind::Interactive, &password, shares).map_err(|e| e.to_string())?;
            save_config(dir, &cfg)?;
            println!("account '{name}' saved");
        }
        UserCmd::Del { name } => {
            let before = cfg.accounts.len();
            cfg.accounts.retain(|a| a.name != name);
            if cfg.accounts.len() == before {
                return Err(format!("no account named '{name}'"));
            }
            save_config(dir, &cfg)?;
            println!("account '{name}' removed");
        }
        UserCmd::List => {
            for a in cfg.accounts.iter().filter(|a| a.kind == AccountKind::Interactive) {
                println!("{}\t{}", a.name, shares_label(a.shares.as_deref()));
            }
        }
    }
    Ok(())
}

fn unattended(dir: &Path, cmd: UnattendedCmd) -> Result<(), String> {
    let mut cfg = load_config(dir)?;
    match cmd {
        UnattendedCmd::Enable { name, shares } => {
            let key = generate_access_key();
            cfg.set_account(&name, AccountKind::Unattended, &key, shares).map_err(|e| e.to_string())?;
            save_config(dir, &cfg)?;
            println!("access key for '{name}' (shown once; store it now):");
            println!("{key}");
        }
    }
    Ok(())
}

fn allow(dir: &Path, cmd: AllowCmd) -> Result<(), String> {
    let mut cfg = load_config(dir)?;
    match cmd {
        AllowCmd::Add { cidr } => {
            if !cfg.allow.contains(&cidr) {
                cfg.allow.push(cidr.clone());
            }
            save_config(dir, &cfg)?; // validates the address
            println!("allowed {cidr}");
        }
        AllowCmd::Del { cidr } => {
            let before = cfg.allow.len();
            cfg.allow.retain(|a| a != &cidr);
            if cfg.allow.len() == before {
                return Err(format!("{cidr} is not in the allow list"));
            }
            save_config(dir, &cfg)?;
            println!("removed {cidr}");
        }
        AllowCmd::List => {
            for a in &cfg.allow {
                println!("{a}");
            }
        }
    }
    Ok(())
}

fn shares_label(shares: Option<&[String]>) -> String {
    match shares {
        None => "all shares".into(),
        Some(list) => list.join(","),
    }
}

#[cfg(windows)]
mod winsvc;

fn service(dir: &Path, cmd: ServiceCmd) -> Result<(), String> {
    #[cfg(windows)]
    {
        match cmd {
            ServiceCmd::Install => {
                winsvc::install(dir)?;
                println!("service '{}' installed; it starts with Windows", winsvc::SERVICE_NAME);
                Ok(())
            }
            ServiceCmd::Uninstall => {
                winsvc::uninstall()?;
                println!("service '{}' removed", winsvc::SERVICE_NAME);
                Ok(())
            }
            ServiceCmd::Run => winsvc::run(dir.to_path_buf()),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (dir, cmd);
        Err("the service commands are for Windows; on Linux use the systemd unit, on macOS the launchd plist".into())
    }
}

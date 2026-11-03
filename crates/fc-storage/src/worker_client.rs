//! The application side of the worker protocol. It starts the worker, sends one sealed
//! plan on its stdin, and collects the events it streams back on stdout. Nothing here
//! writes to a disk; the worker does that.
//!
//! The worker must run as root. The app starts it through `pkexec`, which asks the
//! administrator to authenticate for each operation. See [`pkexec_command`].

use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};

use crate::protocol::{self, Envelope, KEY_LEN, ProtocolError, WorkerEvent};

#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    #[error("cannot start the worker: {0}")]
    Spawn(io::Error),
    #[error("talking to the worker failed: {0}")]
    Io(io::Error),
    #[error("the plan cannot be sent: {0}")]
    Plan(ProtocolError),
    #[error("the worker sent a line that is not an event: {0}")]
    BadEvent(ProtocolError),
    #[error("the worker must be given by its absolute path")]
    NotAbsolute,
}

/// What one run of the worker produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub events: Vec<WorkerEvent>,
    /// `Some(0)` on success. `None` when the worker was killed by a signal.
    pub exit_code: Option<i32>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The three input lines the worker reads: the key, the MAC, and the body.
/// A body with a newline in it cannot be framed, so it is refused here.
pub fn encode_input(key: &[u8; KEY_LEN], env: &Envelope) -> Result<String, ProtocolError> {
    let body = std::str::from_utf8(&env.body).map_err(|_| ProtocolError::Malformed)?;
    if body.contains('\n') {
        return Err(ProtocolError::Malformed);
    }
    Ok(format!("{}\n{}\n{}\n", hex(key), hex(&env.mac), body))
}

/// The command that runs `worker` through `pkexec`, so the administrator is asked to
/// authenticate. `worker` must be an absolute path, because polkit checks the program
/// it is asked to run.
pub fn pkexec_command(worker: &Path) -> Result<Command, ClientError> {
    if !worker.is_absolute() {
        return Err(ClientError::NotAbsolute);
    }
    let mut command = Command::new("pkexec");
    command.arg(worker);
    Ok(command)
}

/// Starts the worker described by `command`, sends one sealed plan, and collects every
/// event until the worker exits. Stderr goes to the caller's stderr.
pub fn run_worker(
    mut command: Command,
    key: &[u8; KEY_LEN],
    env: &Envelope,
) -> Result<Run, ClientError> {
    let input = encode_input(key, env).map_err(ClientError::Plan)?;
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(ClientError::Spawn)?;

    {
        // Dropping stdin at the end of this block closes it, so the worker sees the end.
        let mut stdin = child.stdin.take().expect("stdin was piped");
        stdin.write_all(input.as_bytes()).map_err(ClientError::Io)?;
    }

    let stdout = child.stdout.take().expect("stdout was piped");
    let mut events = Vec::new();
    for line in BufReader::new(stdout).lines() {
        let line = line.map_err(ClientError::Io)?;
        if line.trim().is_empty() {
            continue;
        }
        events.push(protocol::decode_event(&line).map_err(ClientError::BadEvent)?);
    }
    let status = child.wait().map_err(ClientError::Io)?;
    Ok(Run {
        events,
        exit_code: status.code(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_input_is_three_lines_key_mac_and_body() {
        let env = Envelope {
            body: b"{\"a\":1}".to_vec(),
            mac: [0xab; KEY_LEN],
        };
        let input = encode_input(&[0x01; KEY_LEN], &env).unwrap();
        let lines: Vec<&str> = input.lines().collect();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], "01".repeat(KEY_LEN));
        assert_eq!(lines[1], "ab".repeat(KEY_LEN));
        assert_eq!(lines[2], "{\"a\":1}");
    }

    #[test]
    fn a_body_with_a_newline_is_refused() {
        let env = Envelope {
            body: b"{\n}".to_vec(),
            mac: [0; KEY_LEN],
        };
        assert_eq!(
            encode_input(&[0; KEY_LEN], &env),
            Err(ProtocolError::Malformed)
        );
    }

    #[test]
    fn pkexec_needs_an_absolute_worker_path() {
        assert!(matches!(
            pkexec_command(Path::new("fcopy-storage-worker")),
            Err(ClientError::NotAbsolute)
        ));
        let cmd = pkexec_command(Path::new("/usr/libexec/fcopy/fcopy-storage-worker")).unwrap();
        assert_eq!(cmd.get_program(), "pkexec");
    }
}

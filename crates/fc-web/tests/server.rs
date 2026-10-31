//! The production binary, real TLS, durable accounts and paused recovery across process restarts.

use std::io::BufRead;
use std::net::SocketAddr;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use fc_remote::tls::PinnedVerifier;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
use tokio_rustls::TlsConnector;

struct Server {
    child: Child,
    output: Option<std::thread::JoinHandle<()>>,
    address: SocketAddr,
    fingerprint: String,
}

impl Drop for Server {
    fn drop(&mut self) {
        if self.child.try_wait().unwrap().is_none() {
            self.child.kill().unwrap();
            self.child.wait().unwrap();
        }
        if let Some(output) = self.output.take() {
            output.join().unwrap();
        }
    }
}

impl Server {
    fn start(directory: &Path) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_fcopy-web"))
            .arg("--config")
            .arg(directory.join("config"))
            .arg("--root")
            .arg(directory.join("data"))
            .args(["--listen", "127.0.0.1:0", "--no-torrents"])
            .arg("--admin-password-file")
            .arg(directory.join("password"))
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (sender, receiver) = std::sync::mpsc::channel();
        let output = std::thread::spawn(move || {
            for line in std::io::BufReader::new(stdout).lines() {
                if sender.send(line.unwrap()).is_err() {
                    break;
                }
            }
        });
        let mut server = Self {
            child,
            output: Some(output),
            address: "127.0.0.1:0".parse().unwrap(),
            fingerprint: String::new(),
        };
        while server.fingerprint.is_empty() || server.address.port() == 0 {
            let line = receiver
                .recv_timeout(Duration::from_secs(15))
                .expect("server startup");
            if let Some(address) = line.strip_prefix("HTTPS listening on ") {
                server.address = address.parse().unwrap();
            }
            if let Some(fingerprint) = line.strip_prefix("Certificate fingerprint: ") {
                server.fingerprint = fingerprint.into();
            }
        }
        server
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        cookie: Option<&str>,
        body: &str,
    ) -> (String, String, String) {
        let verifier = PinnedVerifier::new(Some(self.fingerprint.clone()));
        let config = rustls::ClientConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_protocol_versions(&[&rustls::version::TLS13])
        .unwrap()
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
        let tcp = tokio::net::TcpStream::connect(self.address).await.unwrap();
        let mut stream = TlsConnector::from(Arc::new(config))
            .connect(ServerName::IpAddress(self.address.ip().into()), tcp)
            .await
            .unwrap();
        let cookie = cookie
            .map(|value| format!("Cookie: {value}\r\n"))
            .unwrap_or_default();
        let request = format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\
             Content-Type: application/json\r\nx-fcopy-request: 1\r\n{cookie}Content-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let response = async {
            let mut reader = tokio::io::BufReader::new(stream);
            let mut status = String::new();
            reader.read_line(&mut status).await.unwrap();
            let mut length = 0;
            let mut cookie = String::new();
            loop {
                let mut header = String::new();
                assert!(reader.read_line(&mut header).await.unwrap() > 0);
                if header == "\r\n" {
                    break;
                }
                if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length: ") {
                    length = value.trim().parse::<usize>().unwrap();
                }
                if header.to_ascii_lowercase().starts_with("set-cookie: ") {
                    cookie = header["set-cookie: ".len()..]
                        .split(';')
                        .next()
                        .unwrap()
                        .trim()
                        .into();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).await.unwrap();
            (status, cookie, String::from_utf8(body).unwrap())
        };
        tokio::time::timeout(Duration::from_secs(5), response)
            .await
            .unwrap()
    }
}

#[tokio::test]
async fn production_server_preserves_login_identity_and_paused_jobs_after_restart() {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir(directory.path().join("data")).unwrap();
    std::fs::write(directory.path().join("password"), "server-test-password\n").unwrap();
    let source = directory.path().join("data/source");
    let destination = directory.path().join("data/output");
    std::fs::write(&source, b"server recovery bytes").unwrap();
    let server = Server::start(directory.path());
    let fingerprint = server.fingerprint.clone();
    let login = r#"{"user":"admin","password":"server-test-password"}"#;
    let (status, cookie, _) = server.request("POST", "/api/auth/login", None, login).await;
    assert!(status.contains("200 OK"), "{status}");
    let (status, _, _) = server
        .request("POST", "/api/queue/pause", Some(&cookie), "{}")
        .await;
    assert!(status.contains("204"), "{status}");
    let body = serde_json::json!({"sources": [source], "dest": destination}).to_string();
    let (status, _, _) = server
        .request("POST", "/api/transfers", Some(&cookie), &body)
        .await;
    assert!(status.contains("202"), "{status}");
    drop(server);
    std::fs::remove_file(directory.path().join("password")).unwrap();
    let server = Server::start(directory.path());
    assert_eq!(server.fingerprint, fingerprint);
    let (status, cookie, _) = server.request("POST", "/api/auth/login", None, login).await;
    assert!(status.contains("200 OK"), "{status}");
    let (status, _, body) = server
        .request("GET", "/api/recovery", Some(&cookie), "")
        .await;
    assert!(status.contains("200 OK"), "{status}");
    let recovery: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(recovery["jobs"].as_array().unwrap().len(), 1);
    assert_eq!(recovery["jobs"][0]["state"], "paused");
    assert!(!destination.exists());
}

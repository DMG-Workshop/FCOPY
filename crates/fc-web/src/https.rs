//! TLS listener for the console. Plain HTTP is not served at all: the
//! sign-in password and the session cookie only ever travel over TLS.
//!
//! Each connection's handshake runs in its own task with a timeout, so a
//! client that stalls the handshake can't hold up other clients.

use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

/// How long a client may take to finish the TLS handshake.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

pub struct TlsListener {
    local: SocketAddr,
    ready: mpsc::Receiver<(TlsStream<TcpStream>, SocketAddr)>,
}

impl TlsListener {
    /// Bind `addr` and serve TLS with `config`. Must be called inside a Tokio runtime.
    pub async fn bind(addr: SocketAddr, config: rustls::ServerConfig) -> io::Result<Self> {
        let tcp = TcpListener::bind(addr).await?;
        let local = tcp.local_addr()?;
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let (tx, ready) = mpsc::channel(64);
        tokio::spawn(async move {
            loop {
                if tx.is_closed() {
                    break; // the server has stopped
                }
                let (sock, peer) = match tcp.accept().await {
                    Ok(v) => v,
                    Err(_) => {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    }
                };
                let (acceptor, tx) = (acceptor.clone(), tx.clone());
                tokio::spawn(async move {
                    if let Ok(Ok(stream)) = tokio::time::timeout(HANDSHAKE_TIMEOUT, acceptor.accept(sock)).await {
                        let _ = tx.send((stream, peer)).await;
                    }
                });
            }
        });
        Ok(Self { local, ready })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local
    }
}

impl axum::serve::Listener for TlsListener {
    type Io = TlsStream<TcpStream>;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        // The accept task holds the sender until the listener is dropped, so this never ends early.
        self.ready.recv().await.expect("accept task runs as long as the listener")
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        Ok(self.local)
    }
}

/// The client's address, as handlers read it through `ConnectInfo<PeerAddr>`.
/// A newtype is needed because axum only supplies `SocketAddr` for its own TCP listener.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PeerAddr(pub SocketAddr);

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, TlsListener>> for PeerAddr {
    fn connect_info(stream: axum::serve::IncomingStream<'_, TlsListener>) -> Self {
        PeerAddr(*stream.remote_addr())
    }
}

impl axum::extract::connect_info::Connected<axum::serve::IncomingStream<'_, TcpListener>> for PeerAddr {
    fn connect_info(stream: axum::serve::IncomingStream<'_, TcpListener>) -> Self {
        PeerAddr(*stream.remote_addr())
    }
}

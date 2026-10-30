//! TLS 1.3 identities and certificate pinning.
//!
//! There's no public CA for "the PC under the desk", so trust works like SSH:
//!
//! * Each agent generates a self-signed certificate once and keeps it
//!   (`identity.der` + `identity.key`, key file mode 0600). Its SHA-256
//!   fingerprint is the host's identity, shown by `fcopy-agent init` and in
//!   mDNS announcements.
//! * The client **pins** fingerprints per host. On first contact with an
//!   unknown host it shows the fingerprint and asks (trust on first use) *before
//!   any credentials are sent*. A changed fingerprint is a hard failure,
//!   just like SSH's "REMOTE HOST IDENTIFICATION HAS CHANGED".
//!
//! Why pin before authenticating: an impostor that completed TLS could not
//! learn the password (SCRAM never sends it), but it *could* collect a proof
//! and run an offline dictionary attack on it. Pinning closes that door.

use std::fs;
use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{
    CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime, pem::PemObject,
};
use rustls::{DigitallySignedStruct, SignatureScheme};

use crate::crypto::fingerprint;
use crate::protocol::ALPN;

/// An agent's long-term TLS identity.
pub struct Identity {
    pub cert: CertificateDer<'static>,
    key: PrivatePkcs8KeyDer<'static>,
}

impl std::fmt::Debug for Identity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Identity({})", self.fingerprint())
    }
}

impl Identity {
    pub fn generate(hostname: &str) -> io::Result<Self> {
        let ck =
            rcgen::generate_simple_self_signed(vec![hostname.to_string(), "fcopy-agent".into()])
                .map_err(io::Error::other)?;
        Ok(Self {
            cert: ck.cert.der().clone(),
            key: PrivatePkcs8KeyDer::from(ck.signing_key.serialize_der()),
        })
    }

    /// Load `dir/identity.{der,key}`, creating them on first run.
    pub fn load_or_create(dir: &Path, hostname: &str) -> io::Result<Self> {
        let (cert_path, key_path) = (dir.join("identity.der"), dir.join("identity.key"));
        if cert_path.exists() && key_path.exists() {
            return Ok(Self {
                cert: CertificateDer::from(fs::read(&cert_path)?),
                key: PrivatePkcs8KeyDer::from(fs::read(&key_path)?),
            });
        }
        fs::create_dir_all(dir)?;
        let id = Self::generate(hostname)?;
        crate::config::write_private(&key_path, id.key.secret_pkcs8_der())?;
        fs::write(&cert_path, id.cert.as_ref())?;
        Ok(id)
    }

    pub fn fingerprint(&self) -> String {
        fingerprint(&self.cert)
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn transport() -> quinn::TransportConfig {
    let mut t = quinn::TransportConfig::default();
    // Keep NAT bindings alive and notice dead peers quickly.
    t.keep_alive_interval(Some(Duration::from_secs(4)));
    t.max_idle_timeout(Some(
        Duration::from_secs(20)
            .try_into()
            .expect("valid idle timeout"),
    ));
    // One stream per file: allow plenty in flight for small-file batches.
    t.max_concurrent_bidi_streams(64u32.into());
    t.max_concurrent_uni_streams(0u32.into());
    // Bigger windows for high bandwidth-delay paths (LAN at 10 Gb/s, WAN at 100 ms).
    t.stream_receive_window((16u32 * 1024 * 1024).into());
    t.receive_window((64u32 * 1024 * 1024).into());
    t.send_window(64 * 1024 * 1024);
    t
}

/// QUIC server configuration: TLS 1.3 only, our ALPN, connection migration on.
pub fn server_config(id: &Identity) -> io::Result<quinn::ServerConfig> {
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(io::Error::other)?
        .with_no_client_auth()
        .with_single_cert(
            vec![id.cert.clone()],
            PrivateKeyDer::Pkcs8(id.key.clone_key()),
        )
        .map_err(io::Error::other)?;
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto =
        quinn::crypto::rustls::QuicServerConfig::try_from(tls).map_err(io::Error::other)?;
    let mut cfg = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    cfg.transport_config(Arc::new(transport()));
    // A client whose Wi-Fi → Ethernet switch changes its address keeps its session.
    cfg.migration(true);
    Ok(cfg)
}

/// Accepts exactly the pinned fingerprint, or (if `pinned` is `None`) any
/// certificate, recording what it saw so the caller can ask the user.
#[derive(Debug)]
pub struct PinnedVerifier {
    pinned: Option<String>,
    seen: Mutex<Option<String>>,
    provider: Arc<CryptoProvider>,
}

impl PinnedVerifier {
    pub fn new(pinned: Option<String>) -> Arc<Self> {
        Arc::new(Self {
            pinned,
            seen: Mutex::new(None),
            provider: provider(),
        })
    }

    /// Fingerprint presented in the last handshake.
    pub fn seen(&self) -> Option<String> {
        self.seen.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fp = fingerprint(end_entity);
        *self.seen.lock().unwrap_or_else(|e| e.into_inner()) = Some(fp.clone());
        match &self.pinned {
            Some(p) if *p == fp => Ok(ServerCertVerified::assertion()),
            Some(p) => Err(rustls::Error::General(format!(
                "HOST IDENTITY CHANGED: expected {p}, server presented {fp}. \
                 This could be an attack, or the agent was reinstalled."
            ))),
            // Unknown host: let the handshake finish; the client asks the user
            // to confirm the fingerprint before sending anything.
            None => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        // The handshake signature must still be valid for the presented key:
        // pinning replaces the CA chain, not the proof of key possession.
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// QUIC client configuration using a [`PinnedVerifier`].
pub fn client_config(verifier: Arc<PinnedVerifier>) -> io::Result<quinn::ClientConfig> {
    let mut tls = rustls::ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(io::Error::other)?
        .dangerous() // custom verifier = pinning instead of WebPKI; see module docs
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    tls.alpn_protocols = vec![ALPN.to_vec()];
    let crypto =
        quinn::crypto::rustls::QuicClientConfig::try_from(tls).map_err(io::Error::other)?;
    let mut cfg = quinn::ClientConfig::new(Arc::new(crypto));
    cfg.transport_config(Arc::new(transport()));
    Ok(cfg)
}

/// TLS 1.3 for the web console, using this agent's identity. The ALPN is
/// HTTP/1.1, which is what the console speaks. QUIC uses [`server_config`].
pub fn https_server_config(id: &Identity) -> io::Result<rustls::ServerConfig> {
    https_server_config_for(
        vec![id.cert.clone()],
        PrivateKeyDer::Pkcs8(id.key.clone_key()),
    )
}

/// Load a browser-trusted certificate chain and unencrypted private key from
/// PEM files for the web console. Returns the TLS config and the leaf SHA-256
/// fingerprint used by the sign-in page. The private key file should be
/// readable only by the account running the web server.
pub fn https_server_config_from_pem(
    certificate_chain: &Path,
    private_key: &Path,
) -> io::Result<(rustls::ServerConfig, String)> {
    let certs = CertificateDer::pem_file_iter(certificate_chain)
        .map_err(|e| io::Error::other(format!("reading {}: {e}", certificate_chain.display())))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| io::Error::other(format!("parsing {}: {e}", certificate_chain.display())))?;
    let leaf = certs.first().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "certificate chain contains no certificates",
        )
    })?;
    let fingerprint = fingerprint(leaf);
    let key = PrivateKeyDer::from_pem_file(private_key)
        .map_err(|e| io::Error::other(format!("reading {}: {e}", private_key.display())))?;
    let config = https_server_config_for(certs, key)?;
    Ok((config, fingerprint))
}

fn https_server_config_for(
    certs: Vec<CertificateDer<'static>>,
    key: PrivateKeyDer<'static>,
) -> io::Result<rustls::ServerConfig> {
    let mut tls = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])
        .map_err(io::Error::other)?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(io::Error::other)?;
    tls.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(tls)
}

#[cfg(test)]
mod https_pem_tests {
    use super::*;

    #[test]
    fn loads_certificate_chain_and_key_from_pem() {
        let dir = tempfile::tempdir().unwrap();
        let pair = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let cert_path = dir.path().join("fullchain.pem");
        let key_path = dir.path().join("key.pem");
        std::fs::write(&cert_path, pair.cert.pem()).unwrap();
        std::fs::write(&key_path, pair.signing_key.serialize_pem()).unwrap();

        let (config, fp) = https_server_config_from_pem(&cert_path, &key_path).unwrap();
        assert_eq!(fp, fingerprint(pair.cert.der()));
        assert_eq!(config.alpn_protocols, vec![b"http/1.1".to_vec()]);
    }

    #[test]
    fn rejects_pem_without_a_certificate() {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("empty.pem");
        let key_path = dir.path().join("key.pem");
        std::fs::write(&cert_path, "not a certificate").unwrap();
        std::fs::write(&key_path, "not a key").unwrap();
        assert!(https_server_config_from_pem(&cert_path, &key_path).is_err());
    }
}

//! LAN discovery over mDNS, so a PC can find paired agents without typing an address.
//!
//! An agent announces `_fcopy._udp.local.` with two TXT records: `fp`, its
//! certificate fingerprint, and `ver`, the protocol version. A client browses for
//! that type and lists what it finds. Discovery only suggests peers. It never
//! decides trust: connecting still checks the fingerprint the user confirmed.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};

use crate::protocol::PROTOCOL_VERSION;

/// The DNS-SD service type every fcopy agent announces.
pub const SERVICE_TYPE: &str = "_fcopy._udp.local.";

#[derive(Debug, thiserror::Error)]
pub enum DiscoveryError {
    #[error("mDNS is not available: {0}")]
    Daemon(String),
    #[error("could not announce this agent: {0}")]
    Announce(String),
}

/// An agent found on the LAN. Trust is decided separately, at pairing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    /// The name the agent announces, usually its configured name.
    pub name: String,
    pub host: String,
    pub port: u16,
    pub addresses: Vec<IpAddr>,
    /// Its certificate fingerprint, as announced. Compare it with what the user confirmed.
    pub fingerprint: String,
}

/// Keeps an agent announced while alive. Dropping it withdraws the announcement.
pub struct Advertisement {
    daemon: ServiceDaemon,
    fullname: String,
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        let _ = self.daemon.unregister(&self.fullname);
        let _ = self.daemon.shutdown();
    }
}

/// Announce an agent on every local network interface.
pub fn advertise(name: &str, port: u16, fingerprint: &str) -> Result<Advertisement, DiscoveryError> {
    let daemon = ServiceDaemon::new().map_err(|e| DiscoveryError::Daemon(e.to_string()))?;
    let host = format!("{}.local.", sanitize_host(name));
    let properties: HashMap<String, String> =
        [("fp".to_string(), fingerprint.to_string()), ("ver".to_string(), PROTOCOL_VERSION.to_string())].into();
    let info = ServiceInfo::new(SERVICE_TYPE, name, &host, "", port, properties)
        .map_err(|e| DiscoveryError::Announce(e.to_string()))?
        .enable_addr_auto();
    let fullname = info.get_fullname().to_string();
    daemon.register(info).map_err(|e| DiscoveryError::Announce(e.to_string()))?;
    Ok(Advertisement { daemon, fullname })
}

/// Listen for agents for `timeout` and return each one found once.
pub fn browse(timeout: Duration) -> Result<Vec<Peer>, DiscoveryError> {
    let daemon = ServiceDaemon::new().map_err(|e| DiscoveryError::Daemon(e.to_string()))?;
    let rx = daemon.browse(SERVICE_TYPE).map_err(|e| DiscoveryError::Daemon(e.to_string()))?;
    let deadline = Instant::now() + timeout;
    let mut found: Vec<Peer> = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            break;
        }
        match rx.recv_timeout(left) {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                let txt = &info.txt_properties;
                let fingerprint = txt.get_property_val_str("fp").map(str::to_string);
                let version = txt.get_property_val_str("ver");
                // Only agents that speak our protocol and announce a fingerprint are listed.
                if let (Some(fp), Some(v)) = (fingerprint, version) {
                    if v == PROTOCOL_VERSION.to_string() {
                        let peer = Peer {
                            name: instance_name(info.get_fullname()),
                            host: info.get_hostname().trim_end_matches('.').to_string(),
                            port: info.get_port(),
                            addresses: info.get_addresses().iter().map(|a| a.to_ip_addr()).collect(),
                            fingerprint: fp,
                        };
                        if !found.iter().any(|p| p.name == peer.name && p.host == peer.host) {
                            found.push(peer);
                        }
                    }
                }
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    let _ = daemon.shutdown();
    Ok(found)
}

/// The instance name from a full name like `nas._fcopy._udp.local.`.
fn instance_name(fullname: &str) -> String {
    let suffix = format!(".{SERVICE_TYPE}");
    fullname.strip_suffix(&suffix).unwrap_or(fullname).replace("\\.", ".")
}

/// A DNS label from a display name: letters, digits and hyphens only.
fn sanitize_host(name: &str) -> String {
    let cleaned: String = name.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
    let trimmed = cleaned.trim_matches('-');
    if trimmed.is_empty() { "fcopy-agent".into() } else { trimmed.chars().take(63).collect() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_names_come_out_of_full_names() {
        assert_eq!(instance_name("nas._fcopy._udp.local."), "nas");
        assert_eq!(instance_name("my.nas._fcopy._udp.local."), "my.nas");
    }

    #[test]
    fn host_labels_are_valid_dns_labels() {
        assert_eq!(sanitize_host("My NAS #2"), "My-NAS--2");
        assert_eq!(sanitize_host("!!!"), "fcopy-agent");
        assert!(sanitize_host(&"x".repeat(100)).len() <= 63);
    }
}

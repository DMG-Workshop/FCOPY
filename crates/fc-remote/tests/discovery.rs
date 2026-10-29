//! mDNS on the local network: an announced agent can be found by browsing.

use std::time::Duration;

use fc_remote::discovery::{advertise, browse};

#[test]
fn an_announced_agent_is_found_by_browsing() {
    let fingerprint = "SHA256:discovery-test-fingerprint";
    let name = format!("fcopy-test-{}", std::process::id());
    let _ad = advertise(&name, 47999, fingerprint).expect("announce");

    let peers = browse(Duration::from_secs(5)).expect("browse");
    let found = peers.iter().find(|p| p.fingerprint == fingerprint).unwrap_or_else(|| panic!("not found among {peers:?}"));
    assert_eq!(found.name, name);
    assert_eq!(found.port, 47999);
    assert!(!found.addresses.is_empty(), "an address is announced");
}

#[test]
fn withdrawn_agents_are_not_listed_again() {
    let fingerprint = "SHA256:withdrawn-test-fingerprint";
    let name = format!("fcopy-gone-{}", std::process::id());
    {
        let _ad = advertise(&name, 47998, fingerprint).expect("announce");
        std::thread::sleep(Duration::from_millis(500));
    }
    // Browsing after the announcement is withdrawn sees nothing for this name.
    std::thread::sleep(Duration::from_millis(500));
    let peers = browse(Duration::from_secs(3)).expect("browse");
    assert!(!peers.iter().any(|p| p.fingerprint == fingerprint), "{peers:?}");
}

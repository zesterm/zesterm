//! Finding peers on the local network.
//!
//! **Skeleton — WS-H owns the implementation.** The trait is here so
//! `zest-daemon` can be written against it before mDNS exists, and so a test
//! fleet can be assembled from a static list without any network at all.

use crate::{MeshError, Peer};

/// A source of peers.
///
/// Deliberately a *snapshot plus changes* rather than a one-shot scan: laptops
/// sleep, networks change, and a fleet listing that was accurate at launch and
/// never again is worse than none, because it looks authoritative.
pub trait Discovery: Send + Sync {
    /// Everything known right now.
    fn peers(&self) -> Vec<Peer>;

    /// Begin discovering. Idempotent.
    fn start(&mut self) -> Result<(), MeshError>;

    /// Stop, releasing any sockets.
    fn stop(&mut self);
}

/// A fixed set of peers, from config or a test.
///
/// Not a placeholder to be deleted: hosts that mDNS cannot reach — a different
/// VLAN, a machine on a VPN, a box whose network blocks multicast — are
/// configured by hand, so a static source is part of the shipping design.
#[derive(Debug, Default, Clone)]
pub struct StaticDiscovery {
    peers: Vec<Peer>,
}

impl StaticDiscovery {
    #[must_use]
    pub fn new(peers: Vec<Peer>) -> Self {
        Self { peers }
    }
}

impl Discovery for StaticDiscovery {
    fn peers(&self) -> Vec<Peer> {
        self.peers.clone()
    }

    fn start(&mut self) -> Result<(), MeshError> {
        Ok(())
    }

    fn stop(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Endpoint, Reachability};
    use zest_proto::HostId;

    #[test]
    fn a_static_fleet_needs_no_network() {
        // What lets the daemon and its tests be written before mDNS exists.
        let peer = Peer {
            host: HostId::from_bytes([9; 32]),
            label: "build-box".into(),
            endpoints: vec![Endpoint {
                host: HostId::from_bytes([9; 32]),
                reachability: Reachability::Lan,
                address: "10.0.0.5:7717".into(),
            }],
        };
        let mut d = StaticDiscovery::new(vec![peer]);
        d.start().expect("static discovery cannot fail");
        assert_eq!(d.peers().len(), 1);
        assert_eq!(d.peers()[0].label, "build-box");
    }
}

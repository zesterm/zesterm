//! Several sources of peers, merged into one fleet.
//!
//! Not a fallback chain — a union. The same Mac is routinely both a
//! hand-configured tunnel endpoint and a machine on the local link, and the
//! point is that it appears **once**, carrying both routes, so that
//! `Peer::best_endpoint` can prefer the LAN. "The LAN beats the tunnel when
//! both are available" is therefore a property of this merge rather than of
//! anything in the mDNS code.
//!
//! # Why a failing layer does not fail the listing
//!
//! [`LayeredDiscovery::start`] returns an error only when *every* layer failed.
//! A machine whose network blocks multicast must still see the hosts its user
//! typed in by hand — that is the entire reason
//! [`StaticDiscovery`](super::StaticDiscovery) ships rather than being a test
//! fixture, and a start that gave up on the first failure would quietly undo it.

use std::collections::BTreeMap;

use zest_proto::HostId;

use super::{Discovery, Peer};
use crate::MeshError;

pub struct LayeredDiscovery {
    layers: Vec<Box<dyn Discovery>>,
}

impl LayeredDiscovery {
    #[must_use]
    pub fn new(layers: Vec<Box<dyn Discovery>>) -> Self {
        Self { layers }
    }

    #[must_use]
    pub fn with(mut self, layer: impl Discovery + 'static) -> Self {
        self.layers.push(Box::new(layer));
        self
    }
}

impl Default for LayeredDiscovery {
    fn default() -> Self {
        Self::new(Vec::new())
    }
}

impl std::fmt::Debug for LayeredDiscovery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LayeredDiscovery")
            .field("layers", &self.layers.len())
            .finish()
    }
}

impl Discovery for LayeredDiscovery {
    /// Union by [`HostId`].
    ///
    /// Endpoints concatenate, deduplicated on `(reachability, address)`, and
    /// keep the order the layers were given in — earlier layers are more
    /// trusted. The label is the first non-empty one, so a name the user typed
    /// into config beats one scraped off the wire.
    fn peers(&self) -> Vec<Peer> {
        let mut merged: BTreeMap<HostId, Peer> = BTreeMap::new();
        for layer in &self.layers {
            for peer in layer.peers() {
                match merged.get_mut(&peer.host) {
                    Some(existing) => {
                        if existing.label.is_empty() {
                            existing.label = peer.label;
                        }
                        for endpoint in peer.endpoints {
                            let duplicate = existing.endpoints.iter().any(|e| {
                                e.reachability == endpoint.reachability
                                    && e.address == endpoint.address
                            });
                            if !duplicate {
                                existing.endpoints.push(endpoint);
                            }
                        }
                    }
                    None => {
                        merged.insert(peer.host, peer);
                    }
                }
            }
        }
        merged.into_values().collect()
    }

    fn start(&mut self) -> Result<(), MeshError> {
        let mut first_error = None;
        let mut any_started = false;
        for layer in &mut self.layers {
            match layer.start() {
                Ok(()) => any_started = true,
                Err(e) => {
                    tracing::warn!(error = %e, "a discovery layer did not start");
                    first_error.get_or_insert(e);
                }
            }
        }
        match first_error {
            // Every layer failed, which genuinely is "discovery is down".
            Some(e) if !any_started => Err(e),
            _ => Ok(()),
        }
    }

    fn stop(&mut self) {
        for layer in &mut self.layers {
            layer.stop();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::StaticDiscovery;
    use crate::{Endpoint, Reachability};

    fn host(seed: u8) -> HostId {
        HostId::from_bytes([seed; 32])
    }

    fn peer(seed: u8, label: &str, reachability: Reachability, address: &str) -> Peer {
        Peer {
            host: host(seed),
            label: label.into(),
            endpoints: vec![Endpoint {
                host: host(seed),
                reachability,
                address: address.into(),
            }],
        }
    }

    /// A layer that is present and cannot start — a network that blocks
    /// multicast, an interface that has not come up.
    #[derive(Default)]
    struct BrokenDiscovery;

    impl Discovery for BrokenDiscovery {
        fn peers(&self) -> Vec<Peer> {
            Vec::new()
        }
        fn start(&mut self) -> Result<(), MeshError> {
            Err(MeshError::Discovery("no multicast on this link".into()))
        }
        fn stop(&mut self) {}
    }

    #[test]
    fn the_lan_route_wins_over_the_configured_tunnel_for_the_same_host() {
        let configured = StaticDiscovery::new(vec![peer(
            9,
            "mac",
            Reachability::Cloud,
            "mac.example.com:443",
        )]);
        let found = StaticDiscovery::new(vec![peer(9, "mac", Reachability::Lan, "10.0.0.5:7717")]);
        let merged = LayeredDiscovery::default().with(configured).with(found);

        let peers = merged.peers();
        assert_eq!(
            peers.len(),
            1,
            "one machine reachable two ways is one machine, not two rows"
        );
        assert_eq!(
            peers[0].best_endpoint().map(|e| e.reachability),
            Some(Reachability::Lan),
            "sitting at the desk with both machines on one switch, routing \
             keystrokes out to Cloudflare and back would add ~60ms to something \
             that should be imperceptible"
        );
    }

    #[test]
    fn a_host_configured_by_hand_and_found_on_the_lan_appears_once() {
        let merged = LayeredDiscovery::default()
            .with(StaticDiscovery::new(vec![peer(
                9,
                "mac",
                Reachability::Lan,
                "10.0.0.5:7717",
            )]))
            .with(StaticDiscovery::new(vec![peer(
                9,
                "mac",
                Reachability::Lan,
                "10.0.0.5:7717",
            )]));
        assert_eq!(
            merged.peers()[0].endpoints.len(),
            1,
            "the same route learned twice is still one route; duplicates would \
             make a dial retry the address that just failed"
        );
    }

    #[test]
    fn a_configured_label_beats_one_scraped_off_the_wire() {
        let mut configured = peer(9, "", Reachability::Cloud, "mac.example.com:443");
        configured.label.clear();
        let merged = LayeredDiscovery::default()
            .with(StaticDiscovery::new(vec![configured]))
            .with(StaticDiscovery::new(vec![peer(
                9,
                "andy-mac",
                Reachability::Lan,
                "10.0.0.5:7717",
            )]));
        assert_eq!(
            merged.peers()[0].label, "andy-mac",
            "an empty label must not win just because its layer came first, or a \
             fleet listing shows a blank row for a machine it can name"
        );
    }

    #[test]
    fn a_hand_configured_host_survives_mdns_being_unavailable() {
        let mut merged = LayeredDiscovery::default()
            .with(StaticDiscovery::new(vec![peer(
                9,
                "vpn-box",
                Reachability::Cloud,
                "vpn.example.com:443",
            )]))
            .with(BrokenDiscovery);

        merged
            .start()
            .expect("one layer failing is not discovery failing");
        assert_eq!(
            merged.peers().len(),
            1,
            "hosts mDNS cannot reach — another VLAN, a VPN, a network blocking \
             multicast — are exactly the ones configured by hand, so losing them \
             when mDNS fails loses them precisely when they matter"
        );
    }

    #[test]
    fn discovery_is_down_only_when_every_layer_is_down() {
        let mut merged = LayeredDiscovery::default()
            .with(BrokenDiscovery)
            .with(BrokenDiscovery);
        assert!(
            merged.start().is_err(),
            "if nothing can start, reporting success leaves the caller to wonder \
             why the fleet is permanently empty"
        );
    }

    #[test]
    fn a_layered_fleet_with_no_layers_is_empty_rather_than_an_error() {
        let mut merged = LayeredDiscovery::default();
        merged
            .start()
            .expect("a fleet of nothing is a legitimate configuration");
        assert!(merged.peers().is_empty());
    }
}

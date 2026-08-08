//! Finding peers on the local network.
//!
//! The trait is here so `zest-daemon` can be written against it, and so a test
//! fleet can be assembled from a static list without any network at all.
//!
//! # The three pieces, and why they are separate
//!
//! [`txt`] and [`roster`] are pure: a wire format and a state machine, with no
//! socket and no mDNS type in any signature. [`mdns`] is a thin adapter that
//! turns a responder's events into the [`roster`]'s vocabulary and pumps a
//! thread. [`layered`] merges sources.
//!
//! The split is not tidiness. Almost everything that can go wrong in discovery
//! is a *rule* — which record to skip, which address cannot be dialled, what a
//! host that stopped announcing should look like — and rules tested through a
//! socket are rules tested on whichever network the test happened to run on.
//! CI runners have no reliable multicast and no second machine, so anything
//! that needed one would be a test that gets muted within a week.

use crate::{MeshError, Peer};

pub mod layered;
pub mod roster;
pub mod txt;

#[cfg(feature = "lan")]
pub mod mdns;

/// How present a peer is.
///
/// Not a field on [`Peer`]: `Peer` is a frozen contract that `zest-daemon` and
/// the clients are already built against, and presence is a property of *how* a
/// peer was learned rather than of the peer itself. A host configured by hand
/// is [`Presence::Unseen`] forever, and that is correct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// Advertising on the link right now.
    Online,
    /// Was advertising and stopped. The laptop closed its lid.
    Away,
    /// Configured by hand and never observed. Normal for a host on a VPN.
    Unseen,
    /// Advertising, and its port refuses. The record outlived the daemon.
    ///
    /// The state a SIGKILLed or crashed host leaves behind (#22): nothing can
    /// intercept those deaths to send a goodbye, and mDNS caches keep the
    /// record for up to 75 minutes — a row that resolves fine and refuses
    /// every dial. Distinct from [`Presence::Away`] because a user acts
    /// differently on each: an away laptop will come back by itself; an
    /// unreachable one has a daemon to restart.
    Unreachable,
}

/// A peer plus what discovery knows about it that [`Peer`] has no room for.
///
/// `endpoints.is_empty()` already tells a listing to grey a row out. This tells
/// it *why*, which is the difference between "your laptop is asleep" and "this
/// machine has never been seen" — two things a user can act on differently.
#[derive(Debug, Clone)]
pub struct PeerRecord {
    pub peer: Peer,
    pub presence: Presence,
    /// `None` if never seen. An `Instant` rather than a wall clock, so a
    /// machine that resyncs its time does not make a peer look hours stale.
    pub last_seen: Option<std::time::Instant>,
}

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

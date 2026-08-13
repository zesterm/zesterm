//! Finding the other machines, and choosing how to reach them.
//!
//! The types in this file are the frozen seam between `zest-daemon` (WS-F) and
//! the discovery and cloud work (WS-H). See `docs/CONTRACTS.md`.
//!
//! # Where to start reading
//!
//! [`identity`] first: a `HostId` *is* a machine's Ed25519 public key, and
//! everything else here — which peer a record belongs to, which peer is
//! filtered out of its own fleet listing, and later which peer is allowed a
//! shell — is asking a question about one. [`discovery`] then answers "which
//! hosts exist and how do I reach them", and [`keystore`] is where the private
//! half sleeps when the process is not running.
//!
//! # Why a transport trait rather than "a WebSocket"
//!
//! The Mac sitting on the same desk and the Mac reached from a train are the
//! same host over two very different paths — roughly 0.3ms across the LAN
//! against 60ms or more out through Cloudflare and back. Sending a keystroke to
//! the next room via another continent is the kind of thing that makes a tool
//! feel wrong in a way users cannot articulate, so the LAN path is not an
//! optimization to add later; it is the normal case at a desk.
//!
//! Both paths carry identical `zest-proto` messages. Only the bytes' route
//! differs, which is what keeps identity, authorization and — later — end-to-end
//! encryption written exactly once.

use std::fmt;

use zest_proto::HostId;

pub mod attest;
pub mod discovery;
pub mod enroll;
pub mod identity;
pub mod keystore;
pub mod pairing;
pub mod secure;
pub mod trust;

/// How a peer was found, and therefore how much it should be trusted before
/// anything else has proved it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Reachability {
    /// Same machine. A named pipe or a unix socket.
    ///
    /// Still speaks the full protocol rather than a shortcut: the GUI app is a
    /// client of its own daemon, and one attach path is the whole point.
    /// → ADR-007.
    Loopback,
    /// Same network, direct.
    Lan,
    /// Through the tunnel.
    Cloud,
}

impl Reachability {
    /// Rough round trip, for choosing between paths and for honest UI.
    #[must_use]
    pub const fn typical_rtt_ms(self) -> u32 {
        match self {
            Self::Loopback => 0,
            Self::Lan => 1,
            Self::Cloud => 60,
        }
    }
}

/// Somewhere a host can be reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub host: HostId,
    pub reachability: Reachability,
    /// Transport-specific: a pipe name, a `host:port`, or a tunnel hostname.
    pub address: String,
}

/// A host in the fleet, with every way currently known to reach it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
    pub host: HostId,
    /// Human name — "andy-mac", "build-box".
    pub label: String,
    /// Every known route, cheapest first.
    pub endpoints: Vec<Endpoint>,
}

impl Peer {
    /// The endpoint to dial.
    ///
    /// Cheapest wins, and "cheapest" is by route rather than by measurement: a
    /// LAN path that is up is always better than a tunnel, and probing both to
    /// find out costs more than it saves.
    #[must_use]
    pub fn best_endpoint(&self) -> Option<&Endpoint> {
        self.endpoints.iter().min_by_key(|e| e.reachability)
    }
}

impl fmt::Display for Peer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.label, self.host.short())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MeshError {
    #[error("no route to {0}")]
    Unreachable(String),
    #[error("discovery failed: {0}")]
    Discovery(String),
    #[error("{0} is not enrolled in this fleet")]
    NotEnrolled(String),
    /// This machine could not read or create its own identity.
    ///
    /// Separate from [`MeshError::BadSignature`] because the two demand
    /// opposite responses: this one is an operator problem and should stop
    /// startup loudly, where a peer failing to prove itself is a security event
    /// to log and rate-limit.
    #[error("identity unavailable: {0}")]
    Identity(String),
    /// A peer claimed an id and could not prove it.
    #[error("{0} did not prove its identity")]
    BadSignature(String),
    /// The record of who is trusted could not be read or written.
    ///
    /// Separate from [`MeshError::Identity`] because it is a *different*
    /// operator problem: the host knows who it is and cannot remember who it
    /// has agreed to serve. Serving the LAN anyway would mean re-prompting for
    /// every device on every reconnect.
    #[error("trust store: {0}")]
    Store(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer_with(routes: &[Reachability]) -> Peer {
        Peer {
            host: HostId::from_bytes([1; 32]),
            label: "mac".into(),
            endpoints: routes
                .iter()
                .map(|r| Endpoint {
                    host: HostId::from_bytes([1; 32]),
                    reachability: *r,
                    address: "x".into(),
                })
                .collect(),
        }
    }

    #[test]
    fn the_lan_beats_the_tunnel_when_both_are_available() {
        // The case this exists for: sitting at the desk with both machines on
        // one switch. Going out to Cloudflare and back would add ~60ms to
        // something that should be imperceptible.
        let p = peer_with(&[Reachability::Cloud, Reachability::Lan]);
        assert_eq!(p.best_endpoint().map(|e| e.reachability), Some(Reachability::Lan));
    }

    #[test]
    fn loopback_beats_everything() {
        let p = peer_with(&[Reachability::Cloud, Reachability::Lan, Reachability::Loopback]);
        assert_eq!(p.best_endpoint().map(|e| e.reachability), Some(Reachability::Loopback));
    }

    #[test]
    fn a_peer_with_no_route_is_not_an_error_here() {
        // An asleep laptop is normal, not exceptional. It should appear in the
        // fleet listing greyed out rather than vanish or raise something.
        let p = peer_with(&[]);
        assert!(p.best_endpoint().is_none());
    }
}

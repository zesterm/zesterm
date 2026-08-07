//! Who is out there, and how sure we are.
//!
//! A plain state machine over observations, with no sockets and no locking —
//! the locking is [`super::mdns::MdnsDiscovery`]'s problem. That separation is
//! the reason every rule below can be tested without a network, including the
//! ones that only ever fire on real hardware, like a laptop closing its lid.
//!
//! # An asleep laptop is normal, not exceptional
//!
//! Half the value of a fleet view is knowing which machines are *off*. So a
//! host that stops advertising keeps its row and loses its routes: `peers()`
//! still returns it, with `endpoints` empty, which is exactly the state
//! `Peer::best_endpoint` already reports as `None` and a listing renders greyed
//! out. It does not vanish, and it does not raise an error.
//!
//! [`PeerRecord`] carries what `Peer` has no room for — how present a host is,
//! and when it was last heard from. `Peer` is a frozen contract, so this is a
//! new type beside it rather than a field inside it.

use std::collections::{BTreeMap, HashMap};
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use zest_proto::HostId;

use super::txt::{instance_of, Advertisement};
use super::{Peer, PeerRecord, Presence};
use crate::{Endpoint, Reachability};

/// How long a host may go unheard before it is treated as away.
///
/// Longer than the usual re-announcement interval, and load-bearing because
/// mDNS goodbyes are best-effort single packets: a laptop whose lid closes
/// abruptly, or a machine yanked off Wi-Fi, sends nothing at all. Without a
/// sweep it would sit in the listing as `Online` forever, offering endpoints
/// that no longer connect.
pub const PRESENCE_TIMEOUT: Duration = Duration::from_secs(90);

/// The most routes one host is given.
///
/// A machine with six VPN tunnels should not produce a six-way dial fan-out.
const MAX_ENDPOINTS: usize = 4;

/// One resolved advertisement, in our vocabulary rather than the mDNS library's.
///
/// This type is the seam that keeps every rule below socket-free: the adapter
/// in [`super::mdns`] builds one from a `ServiceEvent` in about ten lines, and
/// nothing downstream of here has ever heard of mDNS.
#[derive(Debug, Clone)]
pub struct ResolvedRecord {
    pub fullname: String,
    pub txt: Vec<(String, String)>,
    pub port: u16,
    pub addresses: Vec<IpAddr>,
}

/// Something happened on the link.
#[derive(Debug, Clone)]
pub enum Observation {
    Resolved(ResolvedRecord),
    /// A goodbye carries only the fullname — there is no TXT record on a removal
    /// packet, which is why the roster indexes by fullname as well as by host.
    Removed { fullname: String },
}

#[derive(Debug)]
pub struct Roster {
    local: HostId,
    /// A `BTreeMap` so that `peers()` has a stable order. An unstable fleet
    /// listing that reshuffles on every poll is unusable, and it would be a
    /// `HashMap`'s fault rather than the UI's.
    by_host: BTreeMap<HostId, PeerRecord>,
    by_fullname: HashMap<String, HostId>,
    saw_self: bool,
}

impl Roster {
    #[must_use]
    pub fn new(local: HostId) -> Self {
        Self {
            local,
            by_host: BTreeMap::new(),
            by_fullname: HashMap::new(),
            saw_self: false,
        }
    }

    /// Fold one observation in.
    ///
    /// Returns whether anything a viewer would notice changed, so a caller can
    /// skip waking a UI for a re-announcement that said nothing new.
    pub fn apply(&mut self, observation: Observation, now: Instant) -> bool {
        match observation {
            Observation::Resolved(record) => self.resolved(record, now),
            Observation::Removed { fullname } => self.removed(&fullname),
        }
    }

    fn resolved(&mut self, record: ResolvedRecord, now: Instant) -> bool {
        let fallback = instance_of(&record.fullname).to_string();
        let lookup = |key: &str| {
            record
                .txt
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        let Some(advertised) = Advertisement::parse(lookup, &fallback) else {
            // Not one of ours, or malformed. Link noise, not an error.
            return false;
        };

        // Our own advertisement, heard back through multicast loopback. Worth
        // recording rather than merely dropping: "nobody can see me" and
        // "nobody is out there" produce identical empty listings and need
        // completely different fixes.
        if advertised.host == self.local {
            let first = !self.saw_self;
            self.saw_self = true;
            return first;
        }

        let endpoints = endpoints_for(advertised.host, &record.addresses, record.port);
        let peer = Peer {
            host: advertised.host,
            label: advertised.label,
            endpoints,
        };

        self.by_fullname.insert(record.fullname, advertised.host);
        match self.by_host.get_mut(&advertised.host) {
            Some(existing) => {
                let changed =
                    existing.peer != peer || existing.presence != Presence::Online;
                existing.peer = peer;
                existing.presence = Presence::Online;
                existing.last_seen = Some(now);
                changed
            }
            None => {
                self.by_host.insert(
                    advertised.host,
                    PeerRecord {
                        peer,
                        presence: Presence::Online,
                        last_seen: Some(now),
                    },
                );
                true
            }
        }
    }

    fn removed(&mut self, fullname: &str) -> bool {
        // Note what is *not* here: the fullname stays in the index. The same
        // instance re-appearing has to map back to the same row.
        let Some(host) = self.by_fullname.get(fullname).copied() else {
            return false;
        };
        let Some(record) = self.by_host.get_mut(&host) else {
            return false;
        };
        if record.presence == Presence::Away && record.peer.endpoints.is_empty() {
            return false;
        }
        record.peer.endpoints.clear();
        record.presence = Presence::Away;
        true
    }

    /// Grey out anything not heard from in `max_age`.
    ///
    /// Takes `now` rather than reading the clock, so the rule is testable
    /// without a test that sleeps.
    pub fn sweep(&mut self, now: Instant, max_age: Duration) -> bool {
        let mut changed = false;
        for record in self.by_host.values_mut() {
            if record.presence != Presence::Online {
                continue;
            }
            let stale = record
                .last_seen
                .is_none_or(|seen| now.duration_since(seen) >= max_age);
            if stale {
                record.peer.endpoints.clear();
                record.presence = Presence::Away;
                changed = true;
            }
        }
        changed
    }

    #[must_use]
    pub fn peers(&self) -> Vec<Peer> {
        self.by_host.values().map(|r| r.peer.clone()).collect()
    }

    #[must_use]
    pub fn records(&self) -> Vec<PeerRecord> {
        self.by_host.values().cloned().collect()
    }

    /// Whether our own advertisement came back to us.
    ///
    /// The most useful diagnostic this crate has: `false` alongside an empty
    /// peer list means our multicast is not reaching the link — a firewall —
    /// where `true` alongside an empty list means nothing else is out there.
    #[must_use]
    pub const fn saw_self(&self) -> bool {
        self.saw_self
    }
}

/// Turn resolved addresses into endpoints, ordered so the first is the one to
/// dial.
///
/// Ordering is mandatory rather than tidy: the resolver hands addresses back
/// unordered, every endpoint here is `Reachability::Lan`, and `best_endpoint`
/// picks with `min_by_key`, which returns the *first* of several equal minima.
/// The vec order therefore is the dial preference.
pub(crate) fn endpoints_for(host: HostId, addrs: &[IpAddr], port: u16) -> Vec<Endpoint> {
    let mut usable: Vec<IpAddr> = addrs.iter().copied().filter(|ip| dialable(*ip)).collect();
    usable.sort_by_key(|ip| (rank(*ip), ip.to_string()));
    usable.dedup();
    usable
        .into_iter()
        .take(MAX_ENDPOINTS)
        .map(|ip| Endpoint {
            host,
            reachability: Reachability::Lan,
            // `SocketAddr`'s formatting, never `format!("{ip}:{port}")`: the
            // former brackets IPv6 so the string parses back, where the latter
            // yields `fd00::1:7717`, which parses as a *different address*.
            address: SocketAddr::new(ip, port).to_string(),
        })
        .collect()
}

/// Whether an address is worth putting in front of a caller.
fn dialable(ip: IpAddr) -> bool {
    match ip {
        // Loopback is `Reachability::Loopback`'s job and comes from the local
        // socket path. A peer's `127.0.0.1` is our `127.0.0.1`.
        IpAddr::V4(v4) => !v4.is_loopback() && !v4.is_unspecified(),
        IpAddr::V6(v6) => {
            // Link-local IPv6 is dropped outright. `Endpoint.address` is a
            // `String` with no room for a scope id, and `fe80::1:7717` without
            // `%en0` does not connect — an endpoint that cannot be dialled is
            // worse than no endpoint, because `best_endpoint` hands it to a
            // caller who then fails somewhere far from here.
            !v6.is_loopback() && !v6.is_unspecified() && !is_unicast_link_local(v6)
        }
    }
}

fn rank(ip: IpAddr) -> u8 {
    match ip {
        IpAddr::V4(v4) if v4.is_link_local() => 2,
        IpAddr::V4(_) => 0,
        IpAddr::V6(_) => 1,
    }
}

/// `fe80::/10`. Hand-rolled because `Ipv6Addr::is_unicast_link_local` is still
/// unstable.
fn is_unicast_link_local(ip: std::net::Ipv6Addr) -> bool {
    let segments = ip.segments();
    (segments[0] & 0xffc0) == 0xfe80
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discovery::txt::{Advertisement, SERVICE_TYPE};

    fn host(seed: u8) -> HostId {
        HostId::from_bytes([seed; 32])
    }

    fn record(seed: u8, label: &str, addrs: &[&str]) -> ResolvedRecord {
        let id = host(seed);
        let name = super::super::txt::instance_name(label, id);
        ResolvedRecord {
            fullname: format!("{name}.{SERVICE_TYPE}"),
            txt: Advertisement::new(id, label).to_pairs(),
            port: 7717,
            addresses: addrs.iter().map(|a| a.parse().expect("test address")).collect(),
        }
    }

    fn roster() -> Roster {
        Roster::new(host(0xaa))
    }

    #[test]
    fn a_resolved_service_becomes_a_peer_with_a_lan_route() {
        let mut r = roster();
        assert!(
            r.apply(
                Observation::Resolved(record(9, "build-box", &["10.0.0.5"])),
                Instant::now()
            ),
            "the first sighting of a host is a change worth waking a listing for"
        );
        let peers = r.peers();
        assert_eq!(peers.len(), 1, "one advertisement, one peer");
        assert_eq!(
            peers[0].best_endpoint().map(|e| e.address.as_str()),
            Some("10.0.0.5:7717"),
            "a host that resolved with an address must be dialable, or discovery \
             has found something nobody can use"
        );
    }

    #[test]
    fn a_sleeping_laptop_keeps_its_row_and_loses_its_routes() {
        let mut r = roster();
        let seen = record(9, "laptop", &["10.0.0.5"]);
        r.apply(Observation::Resolved(seen.clone()), Instant::now());
        r.apply(
            Observation::Removed {
                fullname: seen.fullname.clone(),
            },
            Instant::now(),
        );

        let peers = r.peers();
        assert_eq!(
            peers.len(),
            1,
            "an asleep laptop is normal, not exceptional: it greys out rather \
             than vanishing, because half the value of a fleet view is knowing \
             which machines are off"
        );
        assert!(
            peers[0].best_endpoint().is_none(),
            "a host that is away must offer no route, or a caller dials an \
             address that stopped answering"
        );
        assert_eq!(
            r.records()[0].presence,
            Presence::Away,
            "`Away` and `Unseen` are different things a user can act on"
        );
    }

    #[test]
    fn a_laptop_that_wakes_up_gets_its_routes_back() {
        let mut r = roster();
        let seen = record(9, "laptop", &["10.0.0.5"]);
        r.apply(Observation::Resolved(seen.clone()), Instant::now());
        r.apply(
            Observation::Removed {
                fullname: seen.fullname.clone(),
            },
            Instant::now(),
        );
        r.apply(Observation::Resolved(seen), Instant::now());

        assert_eq!(r.peers().len(), 1, "the same machine, not a second row");
        assert!(
            r.peers()[0].best_endpoint().is_some(),
            "a machine that came back must be dialable again without a restart"
        );
        assert_eq!(r.records()[0].presence, Presence::Online);
    }

    #[test]
    fn a_host_that_stops_announcing_greys_out_without_a_goodbye() {
        let mut r = roster();
        let start = Instant::now();
        r.apply(Observation::Resolved(record(9, "laptop", &["10.0.0.5"])), start);

        assert!(
            r.sweep(start + PRESENCE_TIMEOUT, PRESENCE_TIMEOUT),
            "mDNS goodbyes are best-effort single packets, so a lid that closed \
             abruptly sends nothing and only the sweep notices"
        );
        assert!(
            r.peers()[0].endpoints.is_empty(),
            "a host left `Online` forever offers endpoints that no longer connect"
        );
    }

    #[test]
    fn a_host_heard_from_recently_is_not_swept_away() {
        let mut r = roster();
        let start = Instant::now();
        r.apply(Observation::Resolved(record(9, "laptop", &["10.0.0.5"])), start);
        assert!(
            !r.sweep(start + Duration::from_secs(1), PRESENCE_TIMEOUT),
            "a sweep that greys out a host between two of its announcements makes \
             the listing flicker"
        );
    }

    #[test]
    fn a_goodbye_for_a_service_we_never_resolved_is_ignored() {
        let mut r = roster();
        assert!(
            !r.apply(
                Observation::Removed {
                    fullname: format!("stranger.{SERVICE_TYPE}")
                },
                Instant::now()
            ),
            "a goodbye for a record we skipped must not create a phantom row"
        );
        assert!(r.peers().is_empty());
    }

    #[test]
    fn the_local_host_is_filtered_out_of_its_own_fleet_listing() {
        let mut r = roster();
        r.apply(
            Observation::Resolved(record(0xaa, "me", &["10.0.0.1"])),
            Instant::now(),
        );
        assert!(
            r.peers().is_empty(),
            "a machine listing itself as a peer would offer to attach to its own \
             daemon over the network instead of over loopback"
        );
    }

    #[test]
    fn hearing_our_own_advertisement_proves_we_are_visible() {
        let mut r = roster();
        r.apply(
            Observation::Resolved(record(0xaa, "me", &["10.0.0.1"])),
            Instant::now(),
        );
        assert!(
            r.saw_self(),
            "an empty listing means two different things — a firewall eating our \
             multicast, or nothing else being out there — and they need \
             completely different fixes"
        );
    }

    #[test]
    fn a_renamed_host_updates_its_row_instead_of_gaining_one() {
        let mut r = roster();
        r.apply(
            Observation::Resolved(record(9, "old-name", &["10.0.0.5"])),
            Instant::now(),
        );
        r.apply(
            Observation::Resolved(record(9, "new-name", &["10.0.0.5"])),
            Instant::now(),
        );
        assert_eq!(
            r.peers().len(),
            1,
            "identity is the key, not the label; renaming a machine does not \
             produce a second machine"
        );
        assert_eq!(r.peers()[0].label, "new-name");
    }

    #[test]
    fn two_hosts_come_back_in_a_stable_order() {
        let forward = {
            let mut r = roster();
            r.apply(Observation::Resolved(record(1, "a", &["10.0.0.1"])), Instant::now());
            r.apply(Observation::Resolved(record(9, "b", &["10.0.0.2"])), Instant::now());
            r.peers()
        };
        let reverse = {
            let mut r = roster();
            r.apply(Observation::Resolved(record(9, "b", &["10.0.0.2"])), Instant::now());
            r.apply(Observation::Resolved(record(1, "a", &["10.0.0.1"])), Instant::now());
            r.peers()
        };
        assert_eq!(
            forward, reverse,
            "a fleet listing that reshuffles on every poll is unusable, and the \
             ordering has to come from the roster rather than from whichever \
             packet arrived first"
        );
    }

    #[test]
    fn link_local_ipv6_is_dropped_because_it_cannot_be_dialled_without_a_scope() {
        let endpoints = endpoints_for(host(9), &["fe80::1".parse().unwrap()], 7717);
        assert!(
            endpoints.is_empty(),
            "`Endpoint.address` has no room for `%en0`, so a scopeless `fe80::` \
             address is one that discovery found and nothing can connect to — \
             worse than no endpoint, because `best_endpoint` hands it out"
        );
    }

    #[test]
    fn a_loopback_address_never_becomes_a_lan_route() {
        let endpoints = endpoints_for(host(9), &["127.0.0.1".parse().unwrap()], 7717);
        assert!(
            endpoints.is_empty(),
            "a peer's 127.0.0.1 is our 127.0.0.1; loopback is a different \
             `Reachability` reached a different way"
        );
    }

    #[test]
    fn an_ipv6_endpoint_is_bracketed_so_it_parses_back_as_a_socket_address() {
        let endpoints = endpoints_for(host(9), &["fd00::5".parse().unwrap()], 7717);
        assert_eq!(endpoints.len(), 1);
        assert!(
            endpoints[0].address.parse::<SocketAddr>().is_ok(),
            "unbracketed, `fd00::5:7717` parses as a different address entirely: {}",
            endpoints[0].address
        );
    }

    #[test]
    fn the_first_endpoint_is_the_one_best_endpoint_picks() {
        let addrs: Vec<IpAddr> = ["169.254.1.1", "fd00::5", "10.0.0.5"]
            .iter()
            .map(|a| a.parse().unwrap())
            .collect();
        let endpoints = endpoints_for(host(9), &addrs, 7717);
        let peer = Peer {
            host: host(9),
            label: "mac".into(),
            endpoints,
        };
        assert_eq!(
            peer.best_endpoint().map(|e| e.address.as_str()),
            Some("10.0.0.5:7717"),
            "every mDNS endpoint is `Lan`, so `min_by_key` breaks the tie by \
             position — the vec order *is* the dial preference, and a routable \
             v4 address should beat an APIPA one"
        );
    }

    #[test]
    fn one_host_never_produces_a_dial_fan_out() {
        let addrs: Vec<IpAddr> = (1..=8)
            .map(|i| format!("10.0.0.{i}").parse().unwrap())
            .collect();
        assert!(
            endpoints_for(host(9), &addrs, 7717).len() <= MAX_ENDPOINTS,
            "a machine with six VPN tunnels should not make every connection \
             attempt six connection attempts"
        );
    }

    #[test]
    fn a_peer_list_is_a_snapshot_and_not_a_live_view() {
        let mut r = roster();
        r.apply(Observation::Resolved(record(9, "mac", &["10.0.0.5"])), Instant::now());
        let snapshot = r.peers();
        r.apply(
            Observation::Removed {
                fullname: record(9, "mac", &["10.0.0.5"]).fullname,
            },
            Instant::now(),
        );
        assert!(
            !snapshot[0].endpoints.is_empty(),
            "`peers()` returns owned clones, so a caller iterating one while the \
             discovery thread mutates the roster must not see it change underneath"
        );
    }
}

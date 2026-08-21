//! What a machine in the fleet is, and the one rule that picks how to reach it.
//!
//! Two things, deliberately small: [`FleetHost`], which is what is *known*
//! about a machine, and [`best_route`], which turns that into a [`HostRoute`]
//! or into `None`. Both are pure. Nothing here browses, advertises, dials,
//! spawns a thread or holds a lock.
//!
//! # Why it is a crate and not a module
//!
//! The rule lived in `zest-app` (issue #250 gathered it there from three
//! places), and `zest-app` imports `winit` and `wgpu`. The MCP server is a
//! *client* of the daemon (ADR-015) and its `check-deps` boundary forbids both
//! by name — so an agent asking "which machines can I reach" had no way to ask
//! the question the window already answers, and the alternative was a second
//! copy of a rule whose whole point is that every surface agrees.
//!
//! `HostRoute::dialer` deliberately did **not** come along. Constructing a
//! transport needs a keychain, a control-plane client and a TLS stack, and
//! each consumer already has its own opinion about all three; what they need
//! to share is the *decision*, not the sockets. That split is the same one
//! `route.rs` made when the rule was gathered, and it is what let this move be
//! a rename rather than a rewrite.
//!
//! # `FleetHost` is built by its consumers, not handed down
//!
//! `zest-app` fills these rows from mDNS and from the account listing.
//! `zest-mcp` will fill them from the same two sources without the app's
//! engine — no threads, no dirty latch, no event-loop proxy. So every field is
//! `pub` and the struct has no private state: a consumer that can name a
//! machine can describe one. That is a constraint to preserve rather than an
//! accident.

use zest_mesh::discovery::Presence;
use zest_proto::{HostId, SessionInfo};

/// What is known about one host's session list.
#[derive(Debug, Clone, Default)]
pub enum SessionsState {
    /// Never asked.
    #[default]
    Unknown,
    /// A listing is on its way.
    Fetching,
    Fresh(Vec<SessionInfo>),
    /// The dial or the listing failed, carrying why.
    ///
    /// The message is written and not yet drawn: the fleet card's "could not
    /// reach" row is #249's next item. Kept rather than dropped because the
    /// state machine is what it exists for, and a `Failed` with nothing in it
    /// would have to be widened again by the commit that renders it.
    Failed(String),
}

/// One row of the fleet, ready for the picker.
#[derive(Clone)]
pub struct FleetHost {
    pub host: HostId,
    pub label: String,
    pub presence: Presence,
    /// This is the machine the window is running on.
    pub local: bool,
    /// The best address to dial, when one is known.
    pub address: Option<String>,
    /// How the best endpoint reaches the host — loopback, LAN, or tunnel.
    /// `None` when nothing is advertised (the synthesized local row).
    pub reachability: Option<zest_mesh::Reachability>,
    /// The prober's last measured round trip to that address, milliseconds.
    /// Measured, not `typical_rtt_ms` — the status bar prints this, and an
    /// honest number is the difference between UI and decoration.
    pub rtt_ms: Option<f32>,
    pub sessions: SessionsState,
    /// The account lists this machine. The durable fact (ROADMAP WS-G:
    /// enrolment is the spine, discovery decorates) — an enrolled host stays
    /// in the listing when mDNS has never heard of it.
    pub enrolled: bool,
    /// What this machine says it can offer: its os, its arch, its default
    /// shell, and its own profiles (#262).
    ///
    /// **`None` is the ordinary state, not an error.** Every reachable host is
    /// *asked* since #265, which is what makes this reachable at all — but the
    /// answer is absent until that host's first listing lands, and stays absent
    /// for a daemon that predates the field or one nothing can reach. All three
    /// read the same way on purpose: nobody has told us anything, so nothing is
    /// drawn. A consumer that treats `None` as "it has no profiles" would show
    /// an empty group for a machine whose watcher simply has not connected yet.
    pub offer: Option<zest_proto::HostOffer>,
    /// The relay had proof of a parked control link when the listing was
    /// fetched — so this machine is reachable through the tunnel even when
    /// nothing on this network has ever heard of it.
    ///
    /// Kept beside `presence` rather than folded into it, and that is the
    /// whole design: `Presence` is `zest_mesh`'s word for what *discovery*
    /// observed, and minting `Online` there for a machine mDNS never saw would
    /// send the prober off to dial a LAN address that does not exist. Read
    /// them together through [`FleetHost::is_online`].
    pub relay_online: bool,
}

impl FleetHost {
    /// Can anything reach this machine right now, by any route?
    ///
    /// The one place the rule lives, because it had five callers and every one
    /// of them spelled it `local || presence == Online` — which is exactly the
    /// expression that made #237 possible. A card, a sidebar count, a picker
    /// row and a settings pill must agree, and the way to make them agree is
    /// to give them one function rather than one convention.
    ///
    /// LAN evidence and tunnel evidence are OR'd rather than ranked: they are
    /// answers to the same question from two mechanisms, and a machine on the
    /// desk with a parked relay link is reachable twice over. Which route is
    /// *preferred* is [`best_route`]'s decision, and it still prefers the LAN.
    #[must_use]
    pub fn is_online(&self) -> bool {
        self.local || self.presence == Presence::Online || self.relay_online
    }
}

/// How a client dials the daemon a tab lives on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostRoute {
    /// This machine's daemon socket.
    LocalSocket(String),
    /// Another machine's daemon at `host:port` (`--attach`, or a LAN
    /// advertisement).
    Tcp(String),
    /// An enrolled machine, through the deployed relay (#190's last leg).
    ///
    /// Carries the origin and the host, never a ticket and never the
    /// token: tickets are 30-second single-use so the dialler mints one per
    /// dial, and the token is read from the credential store per dial too —
    /// on the dial's own worker thread, never the event loop — which is
    /// what makes a signed-out app stop at the next redial with
    /// `SignedOut` instead of riding a credential captured at click time.
    Relay { host: HostId, relay_origin: String },
}

impl HostRoute {
    #[must_use]
    pub fn is_local(&self) -> bool {
        matches!(self, HostRoute::LocalSocket(_))
    }

    /// The address to remember for a restore, when the route has one.
    ///
    /// Only `Tcp` does: a local socket is re-derived from
    /// `default_socket_path()` on the way back, and a relay route is minted
    /// from the account rather than from anything worth persisting.
    #[must_use]
    pub fn dial_hint(&self) -> Option<String> {
        match self {
            HostRoute::Tcp(addr) => Some(addr.clone()),
            HostRoute::LocalSocket(_) | HostRoute::Relay { .. } => None,
        }
    }
}

/// The best way to open a session on this host right now, or `None`.
///
/// Three facts beyond the host itself, because this is pure: the caller's own
/// route (what "local" means for *this* client — a `--attach`ed window's
/// "local" card is a remote machine), the account's relay origin, and whether
/// the caller is signed in.
///
/// **LAN beats relay when both exist.** They are not ranked by preference so
/// much as by evidence: an address off an mDNS record that discovery currently
/// calls `Online` has been seen this minute and costs a TCP connect, while the
/// relay leg is a keychain read, a ticket mint and a TLS handshake.
///
/// The gate on `Presence::Online` earns its keep against exactly one state,
/// and it is worth naming which. `Away` clears its own endpoints upstream —
/// both paths in `Roster` that set it empty the list first, and a roster test
/// pins that "a host that is away must offer no route" — so an `Away` host
/// has no address for this to reject. `Unreachable` is the opposite and is the
/// case: it is set on a host that *is* advertising, whose port refused a dial
/// (#22's dead-daemon-behind-a-live-record), so the address is still there and
/// still wrong. Without the gate that stale address would outrank a live
/// tunnel to the same machine.
///
/// `None` is honest and load-bearing: a card with no route takes no hit
/// region, because an affordance that must fail is worse than none. The web
/// client states the same rule from the other side — it lists a machine it
/// cannot dial and disables the row, rather than dropping it, so a fleet does
/// not appear to shrink whenever the network hiccups.
#[must_use]
pub fn best_route(
    host: &FleetHost,
    local: Option<&HostRoute>,
    relay_origin: Option<&str>,
    signed_in: bool,
) -> Option<HostRoute> {
    if host.local {
        return local.cloned();
    }
    if host.presence == Presence::Online {
        if let Some(addr) = host.address.clone() {
            return Some(HostRoute::Tcp(addr));
        }
    }
    // The last resort, and the leg that makes an enrolled-but-unseen host
    // reachable at all: through the relay the account names. Gated on the
    // caller's own signed-in state rather than on a keychain read — this runs
    // per card per chrome rebuild, and the dialler reads the real token per
    // dial anyway.
    if host.enrolled && signed_in {
        if let Some(origin) = relay_origin {
            return Some(HostRoute::Relay { host: host.host, relay_origin: origin.to_string() });
        }
    }
    None
}

/// Ready-made hosts for tests — this crate's and every consumer's.
///
/// **The default [`fleet`](fixture::fleet) contains two machines that share a
/// display label**, and that is the whole point. Five times now a lookup keyed
/// on the label where it meant the [`HostId`] — the launcher's group map, its
/// provenance lookup, `LinkKind` resolution, a placeholder fallback that could
/// match the local row, and the accent slot table (#304 has the ledger) — and
/// every one was invisible in every test, because no test fleet contained a
/// duplicate. One machine, or several with distinct names, behaves identically
/// under either keying. Two laptops both called `mac` is not exotic; a test
/// fleet where that never happens is the reason the bug kept coming back.
///
/// So: build fleet tests on [`fixture::fleet`] unless the test is *about*
/// something else, and a lookup that quietly keys on the label fails loudly
/// instead of shipping. Hand-rolling a `FleetHost` in a test is how the next
/// duplicate-label bug stays invisible — go through [`fixture::host`] /
/// [`fixture::local`] and mutate the row for the fact under test.
///
/// Not `#[cfg(test)]`, deliberately: a test-gated item does not exist in the
/// library consumers compile against, and the consumers' tests (`zest-app`'s
/// fleet, launcher and presence suites) are exactly who this is for. The crate
/// is workspace-internal and nothing in a shipping binary calls these, so the
/// linker drops them.
pub mod fixture {
    use super::{FleetHost, SessionsState};
    use zest_mesh::discovery::Presence;
    use zest_proto::HostId;

    /// A remote machine, online on the LAN: id `[id; 32]`, an address of its
    /// own (`10.0.0.<id>:7717`) and a measured rtt. Mutate the returned row
    /// for anything a test needs to differ.
    #[must_use]
    pub fn host(id: u8, label: &str) -> FleetHost {
        FleetHost {
            host: HostId::from_bytes([id; 32]),
            label: label.into(),
            presence: Presence::Online,
            local: false,
            address: Some(format!("10.0.0.{id}:7717")),
            reachability: Some(zest_mesh::Reachability::Lan),
            rtt_ms: Some(0.4),
            sessions: SessionsState::Unknown,
            offer: None,
            enrolled: false,
            relay_online: false,
        }
    }

    /// The window's own machine, shaped like the row `snapshot` synthesizes:
    /// no address (loopback is not dialled by address) and no rtt.
    #[must_use]
    pub fn local(id: u8, label: &str) -> FleetHost {
        FleetHost {
            host: HostId::from_bytes([id; 32]),
            label: label.into(),
            presence: Presence::Online,
            local: true,
            address: None,
            reachability: Some(zest_mesh::Reachability::Loopback),
            rtt_ms: None,
            sessions: SessionsState::Unknown,
            offer: None,
            enrolled: false,
            relay_online: false,
        }
    }

    /// The default test fleet: the local row (`studio`, id 1) and **two remote
    /// machines both labelled `mac`** (ids 2 and 3). Anything keyed on the
    /// label conflates the last two; anything keyed on the id tells them
    /// apart. A test that resolves "the second mac" reaches for `fleet[2]` —
    /// the rows are in id order, and a fixture test pins the trap so nobody
    /// "fixes" it by renaming one.
    #[must_use]
    pub fn fleet() -> Vec<FleetHost> {
        vec![local(1, "studio"), host(2, "mac"), host(3, "mac")]
    }
}

#[cfg(test)]
mod tests {
    //! The truth table, moved here with the rule it describes.
    //!
    //! It is the reason the split is worth anything: `best_route` is pure over
    //! facts, so every case is a `cargo test` rather than a two-machine ritual
    //! -- and now it runs for every consumer rather than only the one that
    //! happened to own the file.

    use super::*;
    use zest_mesh::discovery::Presence;
    use zest_proto::HostId;

    fn host(local: bool, presence: Presence, address: Option<&str>, enrolled: bool) -> FleetHost {
        // Routing reads presence, address and enrolment; the shared fixture
        // supplies the rest of the row so this file never hand-rolls one.
        let mut h =
            if local { fixture::local(1, "studio") } else { fixture::host(2, "forge") };
        h.presence = presence;
        h.address = address.map(str::to_string);
        h.reachability = None;
        h.rtt_ms = None;
        h.enrolled = enrolled;
        h
    }

    #[test]
    fn the_default_fixture_fleet_keeps_its_duplicate_label() {
        // The load-bearing assertion of the whole fixture module: the two
        // `mac` rows ARE the fixture's value (#304 — five label-keyed lookups,
        // each invisible against a fleet of distinct names). Renaming one to
        // make some test's expectations tidier would quietly turn every test
        // built on `fixture::fleet()` back into one that cannot tell label
        // keying from id keying.
        let fleet = fixture::fleet();
        assert_eq!(
            fleet.iter().filter(|h| !h.local && h.label == "mac").count(),
            2,
            "two remote machines share one display label, on purpose"
        );
        let (a, b) = (&fleet[1], &fleet[2]);
        assert_ne!(a.host, b.host, "same label, different machines");
        assert_ne!(a.address, b.address, "and different addresses — they are both real");
        assert!(fleet[0].local && fleet.iter().filter(|h| h.local).count() == 1);
    }

    const RELAY: Option<&str> = Some("wss://relay.example");

    #[test]
    fn the_local_host_takes_the_windows_own_route_whatever_it_is() {
        // "Local" is the window's, not the machine's: a `--attach`ed window's
        // own route is a Tcp one, and its "this machine" card must ride it
        // rather than synthesizing a loopback socket it cannot reach.
        let socket = HostRoute::LocalSocket("/tmp/zest.sock".into());
        assert_eq!(
            best_route(&host(true, Presence::Unseen, None, false), Some(&socket), None, false),
            Some(socket.clone()),
            "presence and enrolment say nothing about the window's own daemon"
        );
        let attached = HostRoute::Tcp("10.0.0.7:7717".into());
        assert_eq!(
            best_route(&host(true, Presence::Online, None, true), Some(&attached), RELAY, true),
            Some(attached),
            "a --attach'ed window's local card is a remote machine, and keeps its route"
        );
        assert_eq!(
            best_route(&host(true, Presence::Online, None, false), None, RELAY, true),
            None,
            "no route of our own is no route at all — never a relay leg to ourselves"
        );
    }

    #[test]
    fn an_online_advertisement_beats_the_relay() {
        // Evidence, not preference: an address off a record discovery calls
        // Online has been seen this minute and costs one TCP connect, while
        // the relay leg is a keychain read, a ticket mint and a handshake.
        let h = host(false, Presence::Online, Some("10.0.0.7:7717"), true);
        assert_eq!(
            best_route(&h, None, RELAY, true),
            Some(HostRoute::Tcp("10.0.0.7:7717".into())),
            "enrolled and signed in, but the LAN answer is right there"
        );
    }

    #[test]
    fn an_enrolled_host_the_lan_cannot_see_gets_the_relay() {
        // The hole #250 exists to close. Every arm here used to answer None
        // everywhere except the fleet card.
        let h = host(false, Presence::Unseen, None, true);
        assert_eq!(
            best_route(&h, None, RELAY, true),
            Some(HostRoute::Relay {
                host: HostId::from_bytes([2; 32]),
                relay_origin: "wss://relay.example".into()
            }),
            "discovery has never heard of it; the account has, and the relay can reach it"
        );

        // An advertising host whose port refuses (#22's Unreachable) keeps its
        // address — that is the whole state, a dead daemon behind a live
        // record — and it is exactly the host the tunnel is the answer for.
        // This is the one case the `Presence::Online` gate exists for.
        let refusing = host(false, Presence::Unreachable, Some("10.0.0.7:7717"), true);
        assert!(
            matches!(best_route(&refusing, None, RELAY, true), Some(HostRoute::Relay { .. })),
            "a stale address must not outrank a live tunnel"
        );

        // `Away` reaches the same answer by a shorter road: `Roster` clears a
        // record's endpoints on the way into that state, so there is no
        // address for the gate to reject. Built with `None` deliberately — the
        // pair (Away, Some(addr)) cannot occur, and a test that constructs it
        // is pinning a state the roster forbids.
        let asleep = host(false, Presence::Away, None, true);
        assert!(matches!(best_route(&asleep, None, RELAY, true), Some(HostRoute::Relay { .. })));
    }

    #[test]
    fn no_evidence_is_no_route_and_that_is_the_honest_answer() {
        // A card with no route takes no hit region, so each of these is a
        // click that does not happen rather than one that fails.
        let unseen = host(false, Presence::Unseen, None, false);
        assert_eq!(best_route(&unseen, None, RELAY, true), None, "not enrolled, never seen");

        let enrolled = host(false, Presence::Unseen, None, true);
        assert_eq!(
            best_route(&enrolled, None, RELAY, false),
            None,
            "enrolled but signed out: nothing can mint a ticket"
        );
        assert_eq!(
            best_route(&enrolled, None, None, true),
            None,
            "signed in to a deployment with no relay: honestly unreachable"
        );

        // The DNS-SD trap (an instance with an empty address set) reads as
        // Online with nothing to dial — and falls through to the relay when
        // the account has one, rather than minting a Tcp route to nowhere.
        let empty = host(false, Presence::Online, None, true);
        assert!(matches!(best_route(&empty, None, RELAY, true), Some(HostRoute::Relay { .. })));
        assert_eq!(best_route(&empty, None, None, true), None);
    }

    #[test]
    fn only_a_tcp_route_is_worth_remembering() {
        // Restore re-derives the socket path and mints the relay leg from the
        // account; an address learned from an advertisement is the one fact
        // that has to be written down.
        assert_eq!(
            HostRoute::Tcp("10.0.0.7:7717".into()).dial_hint().as_deref(),
            Some("10.0.0.7:7717")
        );
        assert_eq!(HostRoute::LocalSocket("/tmp/s".into()).dial_hint(), None);
        assert_eq!(
            HostRoute::Relay {
                host: HostId::from_bytes([2; 32]),
                relay_origin: "wss://relay.example".into()
            }
            .dial_hint(),
            None
        );
    }
}

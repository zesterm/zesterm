//! How this window reaches a machine — the transport, and the one rule that
//! picks it.
//!
//! [`HostRoute`] carries what a dial needs; [`best_route`] decides which one a
//! given [`FleetHost`] gets. The rule lived in three places before this module
//! (issue #250): `App::best_route` for the fleet screen's cards, an inline
//! `host.address.map(HostRoute::Tcp)` in the ⌘K picker, and a third derivation
//! in `launch::resolve_host` for profile launches. Only the first had ever
//! learned about the relay, so an enrolled machine that is not on this LAN —
//! the case the relay exists for — could be opened by clicking its card and by
//! nothing else.
//!
//! Split the way the rest of the app splits: [`best_route`] is pure over facts,
//! so the truth table is a `cargo test` rather than a two-machine ritual, and
//! `HostRoute::dialer` keeps the sockets.

use crate::fleet::FleetHost;

/// How this window dials the daemon a tab lives on.
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
    Relay { host: zest_proto::HostId, relay_origin: String },
}

/// The best way to open a session on this host right now, or `None`.
///
/// Three facts beyond the host itself, because this is pure: the window's own
/// route (what "local" means for *this* window — a `--attach`ed window's
/// "local" card is a remote machine), the account's relay origin, and whether
/// the app is signed in.
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
/// region, because an affordance that must fail is worse than none.
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
    if host.presence == zest_mesh::discovery::Presence::Online {
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
            return Some(HostRoute::Relay {
                host: host.host,
                relay_origin: origin.to_string(),
            });
        }
    }
    None
}

impl HostRoute {
    pub fn is_local(&self) -> bool {
        matches!(self, HostRoute::LocalSocket(_))
    }

    /// The address to remember for a restore, when the route has one.
    ///
    /// Only `Tcp` does: a local socket is re-derived from
    /// `default_socket_path()` on the way back, and a relay route is minted
    /// from the account rather than from anything worth persisting.
    pub fn dial_hint(&self) -> Option<String> {
        match self {
            HostRoute::Tcp(addr) => Some(addr.clone()),
            HostRoute::LocalSocket(_) | HostRoute::Relay { .. } => None,
        }
    }

    pub fn dialer(&self) -> crate::remote::Dialer {
        match self {
            HostRoute::LocalSocket(path) => {
                let path = path.clone();
                Box::new(move || {
                    let a = crate::daemon::find_or_spawn(&path, crate::app::DAEMON_START_TIMEOUT)
                        .map_err(|e| crate::remote::RemoteError::Io(e.to_string()))?;
                    Ok((a.read, a.write))
                })
            }
            HostRoute::Tcp(addr) => {
                let addr = addr.clone();
                Box::new(move || {
                    let stream = std::net::TcpStream::connect(&addr)
                        .map_err(|e| crate::remote::RemoteError::Io(e.to_string()))?;
                    // A terminal's writes are keystrokes: small, latency-bound,
                    // never worth coalescing.
                    let _ = stream.set_nodelay(true);
                    let read = stream
                        .try_clone()
                        .map_err(|e| crate::remote::RemoteError::Io(e.to_string()))?;
                    Ok((
                        Box::new(read) as Box<dyn std::io::Read + Send>,
                        Box::new(stream) as Box<dyn std::io::Write + Send>,
                    ))
                })
            }
            HostRoute::Relay { host, relay_origin } => {
                let host = *host;
                let origin = relay_origin.clone();
                Box::new(move || {
                    use crate::remote::RemoteError;
                    // Every dial — every redial — runs the whole ladder
                    // fresh: token from the store, ticket from the control
                    // plane, TLS to the relay. This closure runs on the tab
                    // worker or the reconnect supervisor, so the keychain
                    // and two network round trips stay off the event loop.
                    let mint = || {
                        let token =
                            crate::cloud::stored_app_token(&zest_mesh::keystore::OsKeyStore)
                                .map_err(|e| crate::cloud::CloudError::Transport(e.to_string()))?
                                .ok_or(crate::cloud::CloudError::SignedOut)?;
                        let api = crate::cloud::HttpsAccountApi::new(
                            zest_daemon::enroll::DEFAULT_CONTROL_PLANE,
                            zest_cloud::tls::Roots::Platform,
                        )
                        .map_err(|e| crate::cloud::CloudError::Transport(e.to_string()))?;
                        crate::cloud::mint_ticket(&api, &token, host)
                    };
                    // The daemon's own relay diallers, reused whole: the TLS
                    // one's read poll arrangement is the trap `zest_cloud::
                    // tls::READ_POLL` documents, and the plaintext one is
                    // loopback-only by `RelayOrigin::parse`'s rule — a
                    // `wrangler dev` relay for the edit-run loop. The `cut`
                    // is dropped: `RemoteSession`'s supervisor owns this
                    // link's lifecycle through read errors, and has no
                    // handshake watchdog of its own to arm it from.
                    let parsed = zest_daemon::relay::RelayOrigin::parse(&origin)
                        .map_err(|e| RemoteError::Io(e.to_string()))?;
                    let connect = || {
                        let dial = if parsed.tls {
                            zest_daemon::relay::tls_dialler(zest_cloud::tls::Roots::Platform)
                        } else {
                            zest_daemon::relay::plaintext_dialler()
                        };
                        let wire = dial(&parsed.host, parsed.port)?;
                        Ok(crate::cloud::RelayLeg { reader: wire.reader, writer: wire.writer })
                    };
                    crate::cloud::relay_dial(host, &parsed.host_header(), &mint, &connect)
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zest_mesh::discovery::Presence;
    use zest_proto::HostId;

    fn host(local: bool, presence: Presence, address: Option<&str>, enrolled: bool) -> FleetHost {
        FleetHost {
            host: HostId::from_bytes([if local { 1 } else { 2 }; 32]),
            label: if local { "studio".into() } else { "forge".into() },
            presence,
            local,
            address: address.map(str::to_string),
            reachability: None,
            rtt_ms: None,
            sessions: crate::fleet::SessionsState::Unknown,
            enrolled,
            relay_online: false,
        }
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

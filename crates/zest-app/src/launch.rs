//! Cross-host profile launch (design §12, issue #175): the pure decisions
//! the event loop and the launch worker share.
//!
//! Three seams, all testable without a window, a socket or a fleet:
//! which machine a profile's `host` label names ([`resolve_host`]), what
//! command line the session runs ([`launch_command`]), and when a failing
//! dial stops retrying and settles the tab ([`verdict_after`]). The worker
//! and the chrome both read these instead of re-deriving them, so "what the
//! row promised" and "what the launch did" cannot drift.

use std::time::Duration;

use zest_proto::HostId;

use crate::fleet::FleetHost;
use crate::route::{best_route, HostRoute};

/// Where a profile launch should dial, resolved against the fleet snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostTarget {
    /// The window's own machine: the profile pinned no host, or pinned the
    /// host this window runs on. Launches ride the window's existing route.
    Local,
    /// A remote host and the transport that reaches it. Dialled per tab;
    /// presence deliberately does not gate this — a sleeping host still gets
    /// a connecting tab and the worker's retries, not a refusal (§12: "a
    /// profile whose host is asleep still launches").
    ///
    /// The route rather than a bare address, so a launch reaches an enrolled
    /// machine through the relay exactly as a fleet card does (#250). One
    /// variant rather than a second `Relay` arm: the worker calls
    /// `route.dialer()` and does not care which transport answered.
    Remote { host: HostId, route: HostRoute },
    /// No dialable route: a label the fleet has never seen, or a host with
    /// no evidence of any kind — no advertisement, no enrolment, or an
    /// account with no relay. Still a launch — the tab goes up connecting
    /// and settles failed with this message — never a panic and never a
    /// silent log line.
    Unroutable { error: String },
}

/// Resolve a profile's `host` label against the fleet.
///
/// Labels are mDNS display names, so the match is ASCII case-insensitive —
/// a profile hand-written as `Forge` must find the host advertising `forge`.
/// ASCII only, deliberately: a Unicode fold needs a table this crate does
/// not carry, and a host label that differs only by non-ASCII case is a
/// hostname nobody can type reliably anyway.
///
/// The three trailing arguments are [`best_route`]'s, passed through rather
/// than re-derived: before #250 this function knew only `FleetHost::address`,
/// so a profile pinned to an enrolled machine that mDNS could not see
/// resolved `Unroutable` while its fleet card opened a shell immediately.
#[must_use]
pub fn resolve_host(
    label: Option<&str>,
    fleet: &[FleetHost],
    local: Option<&HostRoute>,
    relay_origin: Option<&str>,
    signed_in: bool,
) -> HostTarget {
    let Some(label) = label.map(str::trim).filter(|l| !l.is_empty()) else {
        return HostTarget::Local;
    };
    match fleet.iter().find(|h| h.label.eq_ignore_ascii_case(label)) {
        Some(h) if h.local => HostTarget::Local,
        Some(h) => match best_route(h, local, relay_origin, signed_in) {
            Some(route) => HostTarget::Remote { host: h.host, route },
            None => HostTarget::Unroutable {
                error: format!("no way to reach host '{label}' right now"),
            },
        },
        None => HostTarget::Unroutable { error: format!("host '{label}' is not in the fleet") },
    }
}

/// The target for a host the *user* picked, rather than one a profile named.
///
/// The `ask_host` flow (design §12): the picker was opened to choose a
/// host-agnostic profile's machine, and a host row is the choice. The route is
/// already decided — the picker built it — so the only question left is the one
/// [`resolve_host`] answers for a label, asked by id instead.
///
/// **"Local" is the fleet row's word, never the route variant's.** A
/// `--attach`ed window's own route is a `Tcp` one, so `route.is_local()` — the
/// obvious spelling, and the one this replaced — sends a launch on the window's
/// already-proven route down the cold path: a connecting placeholder tab and up
/// to [`MAX_DIALS`] retries to reach the daemon it is currently talking to.
#[must_use]
pub fn resolve_picked_host(host: HostId, route: HostRoute, fleet: &[FleetHost]) -> HostTarget {
    match fleet.iter().find(|h| h.host == host) {
        Some(h) if h.local => HostTarget::Local,
        // Not in the snapshot is not a refusal: the picker drew a row for it a
        // moment ago and handed us a route it built. A roster that has since
        // swept the record must not turn a click into nothing.
        _ => HostTarget::Remote { host, route },
    }
}

/// The command line a launched session runs.
///
/// Precedence, outermost first: the profile's own `command` (which
/// `resolve_profile` has already folded through Defaults, so a profile
/// without one inherits `profiles.defaults.command`), then the machine's
/// rule for "no command": the resolved local shell for a local launch,
/// empty — meaning *the far host's* default shell — for a remote one. The
/// local command line sent remotely would ask a Mac to run this machine's
/// PowerShell.
#[must_use]
pub fn launch_command(
    profile_command: Option<String>,
    local: bool,
    configured_shell: Option<&str>,
) -> String {
    match profile_command {
        Some(c) => c,
        None if local => configured_shell.unwrap_or_default().to_string(),
        None => String::new(),
    }
}

/// How many times the worker dials before the tab settles failed.
///
/// Three: enough to ride out a host mid-wake or a transient refusal,
/// bounded so an unreachable machine reads as failed in seconds rather
/// than being retried into an appearance of hanging.
pub const MAX_DIALS: u32 = 3;

/// What one failed dial means for the connecting tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DialVerdict {
    /// Dial again after this pause.
    RetryAfter(Duration),
    /// Settle the tab failed, carrying the last error.
    GiveUp,
}

/// The connecting→failed decision, pure: given how many dials have now
/// failed, retry (with a bounded backoff) or give up. A successful dial
/// never comes here — the tab settles live the moment one attach succeeds.
#[must_use]
pub fn verdict_after(failures: u32) -> DialVerdict {
    if failures >= MAX_DIALS {
        DialVerdict::GiveUp
    } else {
        // 500ms, then 1.5s: ~2s worst case before an honest failure, long
        // enough for a daemon restarting mid-launch to come back.
        DialVerdict::RetryAfter(Duration::from_millis(500 * 3u64.pow(failures - 1)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zest_mesh::discovery::Presence;

    fn host(label: &str, local: bool, address: Option<&str>) -> FleetHost {
        FleetHost {
            host: HostId::from_bytes([if local { 1 } else { 2 }; 32]),
            label: label.to_string(),
            presence: Presence::Online,
            local,
            address: address.map(str::to_string),
            reachability: None,
            rtt_ms: None,
            sessions: crate::fleet::SessionsState::Unknown,
            enrolled: false,
            relay_online: false,
        }
    }

    /// The common case: a LAN fleet, no account. The route rule's own truth
    /// table lives in `route.rs`; these pin what *label resolution* does with
    /// its answer.
    fn on_lan(label: Option<&str>, fleet: &[FleetHost]) -> HostTarget {
        resolve_host(label, fleet, None, None, false)
    }

    #[test]
    fn no_label_and_the_local_label_both_stay_local() {
        // The v1 behaviour, preserved: a profile that pins nothing rides the
        // window's route, and pinning the window's own machine is the same
        // thing said explicitly.
        let fleet = [host("studio", true, None), host("forge", false, Some("10.0.0.7:7717"))];
        assert_eq!(on_lan(None, &fleet), HostTarget::Local);
        assert_eq!(on_lan(Some("studio"), &fleet), HostTarget::Local);
        assert_eq!(
            on_lan(Some(""), &fleet),
            HostTarget::Local,
            "an empty label is the file's spelling of unset, like profiles.rs reads it"
        );
    }

    #[test]
    fn a_remote_label_resolves_to_its_advertised_address() {
        let fleet = [host("studio", true, None), host("forge", false, Some("10.0.0.7:7717"))];
        assert_eq!(
            on_lan(Some("forge"), &fleet),
            HostTarget::Remote {
                host: HostId::from_bytes([2; 32]),
                route: HostRoute::Tcp("10.0.0.7:7717".to_string())
            }
        );
        assert_eq!(
            on_lan(Some("FORGE"), &fleet),
            on_lan(Some("forge"), &fleet),
            "labels are display names; case must not decide which machine runs a shell"
        );
    }

    #[test]
    fn a_profile_pinned_to_an_enrolled_host_off_this_lan_launches_through_the_relay() {
        // The hole #250 closes, and the one that reads as a broken feature:
        // this exact host's fleet card opened a shell immediately (the card
        // was the only caller that knew about the relay), while the profile
        // pinned to it put up a connecting tab that settled failed with "host
        // 'forge' advertises no address to dial" — a message about the LAN
        // for a machine nobody was trying to reach over the LAN.
        let mut forge = host("forge", false, None);
        forge.presence = Presence::Unseen;
        forge.enrolled = true;
        let fleet = [host("studio", true, None), forge];

        assert_eq!(
            resolve_host(Some("forge"), &fleet, None, Some("wss://relay.example"), true),
            HostTarget::Remote {
                host: HostId::from_bytes([2; 32]),
                route: HostRoute::Relay {
                    host: HostId::from_bytes([2; 32]),
                    relay_origin: "wss://relay.example".to_string(),
                },
            }
        );
        // And signed out it is honestly unroutable rather than silently local.
        assert!(matches!(
            resolve_host(Some("forge"), &fleet, None, Some("wss://relay.example"), false),
            HostTarget::Unroutable { .. }
        ));
    }

    #[test]
    fn a_picked_host_is_local_by_the_rosters_word_not_by_its_routes_shape() {
        // `--attach`: the window's own route is a Tcp one, and the machine it
        // reaches is the fleet's `local` row. Asking `route.is_local()` — the
        // obvious spelling — answers false, which sends an ask_host launch on
        // an already-proven route down the connecting-tab-and-retries path to
        // dial the daemon it is currently talking to.
        let attached = HostRoute::Tcp("10.0.0.7:7717".to_string());
        let mut here = host("forge", true, Some("10.0.0.7:7717"));
        here.host = HostId::from_bytes([7; 32]);
        let fleet = [here, host("pi", false, Some("10.0.0.9:7717"))];

        assert!(
            !attached.is_local(),
            "precondition: the window's own route really is a Tcp one here"
        );
        assert_eq!(
            resolve_picked_host(HostId::from_bytes([7; 32]), attached.clone(), &fleet),
            HostTarget::Local,
            "the row says this is our machine; the launch rides the window's route"
        );

        // Any other row is remote, carrying whatever route the picker built.
        assert_eq!(
            resolve_picked_host(HostId::from_bytes([2; 32]), attached.clone(), &fleet),
            HostTarget::Remote { host: HostId::from_bytes([2; 32]), route: attached.clone() }
        );

        // And a host the roster has swept since the row was drawn is still a
        // launch: the picker handed us a route it built a moment ago, and
        // turning that click into nothing would be the worse answer.
        assert_eq!(
            resolve_picked_host(HostId::from_bytes([9; 32]), attached.clone(), &[]),
            HostTarget::Remote { host: HostId::from_bytes([9; 32]), route: attached }
        );
    }

    #[test]
    fn an_unknown_label_is_a_launch_that_will_fail_not_a_panic() {
        // The §12 rule this item exists for: the tab still goes up, in a
        // connecting state, and settles failed carrying this message — a
        // typo'd host must never be a silent warn! or a crash.
        let fleet = [host("studio", true, None)];
        let HostTarget::Unroutable { error } = on_lan(Some("gone"), &fleet) else {
            panic!("an unknown label must resolve Unroutable");
        };
        assert!(error.contains("gone"), "the error names the label: {error}");

        // A host found but with an empty address set (the DNS-SD trap) and
        // nothing else to go on is the same shape: present in the listing,
        // nothing to dial.
        let fleet = [host("sleepy", false, None)];
        let HostTarget::Unroutable { error } = on_lan(Some("sleepy"), &fleet) else {
            panic!("a host with no route must resolve Unroutable");
        };
        assert!(error.contains("sleepy"), "the error names the label: {error}");
    }

    #[test]
    fn command_precedence_is_profile_then_defaults_then_the_hosts_rule() {
        // Pinned end to end: the profile table's fold (profile > Defaults)
        // happens in resolve_profile, and this seam owns the tail — the
        // resolved local shell locally, empty (the far host picks) remotely.
        let table = |text: &str| -> toml::Table { text.parse().expect("valid toml") };
        let mut root = toml::Table::new();
        let mut profiles = toml::Table::new();
        profiles.insert("defaults".into(), toml::Value::Table(table("command = \"zsh -l\"")));
        profiles.insert("own".into(), toml::Value::Table(table("command = \"wsl.exe\"")));
        profiles.insert("bare".into(), toml::Value::Table(toml::Table::new()));
        root.insert("profiles".into(), toml::Value::Table(profiles));

        let resolved = |name: &str| zest_config::profiles::resolve_profile(&root, name).meta.command;
        assert_eq!(
            launch_command(resolved("own"), false, Some("pwsh -NoLogo")),
            "wsl.exe",
            "the profile's own command outranks everything"
        );
        assert_eq!(
            launch_command(resolved("bare"), false, Some("pwsh -NoLogo")),
            "zsh -l",
            "a command-less profile inherits Defaults' command"
        );

        // Neither profile nor Defaults set one: the host's rule.
        assert_eq!(
            launch_command(None, true, Some("pwsh -NoLogo")),
            "pwsh -NoLogo",
            "local: the resolved shell, exactly what ⌘T runs"
        );
        assert_eq!(
            launch_command(None, false, Some("pwsh -NoLogo")),
            "",
            "remote: empty, so the far host picks its own shell"
        );
        assert_eq!(launch_command(None, true, None), "", "no shell configured: the daemon's default");
    }

    #[test]
    fn the_dial_loop_is_bounded_and_backs_off() {
        // Three tries, growing pauses, then a settled failure — the worker
        // reads this verbatim, so the bound lives in one tested place.
        let DialVerdict::RetryAfter(first) = verdict_after(1) else {
            panic!("one failure retries");
        };
        let DialVerdict::RetryAfter(second) = verdict_after(2) else {
            panic!("two failures still retry");
        };
        assert!(second > first, "the backoff grows: {first:?} then {second:?}");
        assert!(
            second <= Duration::from_secs(5),
            "and stays bounded — a launch is not a background reconnect"
        );
        assert_eq!(verdict_after(MAX_DIALS), DialVerdict::GiveUp, "the third failure settles");
        assert_eq!(verdict_after(MAX_DIALS + 1), DialVerdict::GiveUp, "and it stays settled");
    }
}

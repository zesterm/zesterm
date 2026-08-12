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

/// Where a profile launch should dial, resolved against the fleet snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostTarget {
    /// The window's own machine: the profile pinned no host, or pinned the
    /// host this window runs on. Launches ride the window's existing route.
    Local,
    /// A remote host with an advertised address. Dialled per tab; presence
    /// deliberately does not gate this — a sleeping host still gets a
    /// connecting tab and the worker's retries, not a refusal (§12: "a
    /// profile whose host is asleep still launches").
    Remote { host: HostId, addr: String },
    /// No dialable route: a label the fleet has never seen, or a host
    /// advertising an empty address set (the DNS-SD instance-name trap).
    /// Still a launch — the tab goes up connecting and settles failed with
    /// this message — never a panic and never a silent log line.
    Unroutable { error: String },
}

/// Resolve a profile's `host` label against the fleet.
///
/// Labels are mDNS display names, so the match is case-insensitive — a
/// profile hand-written as `Forge` must find the host advertising `forge`.
#[must_use]
pub fn resolve_host(label: Option<&str>, fleet: &[FleetHost]) -> HostTarget {
    let Some(label) = label.map(str::trim).filter(|l| !l.is_empty()) else {
        return HostTarget::Local;
    };
    match fleet.iter().find(|h| h.label.eq_ignore_ascii_case(label)) {
        Some(h) if h.local => HostTarget::Local,
        Some(h) => match &h.address {
            Some(addr) => HostTarget::Remote { host: h.host, addr: addr.clone() },
            None => HostTarget::Unroutable {
                error: format!("host '{label}' advertises no address to dial"),
            },
        },
        None => HostTarget::Unroutable { error: format!("host '{label}' is not in the fleet") },
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
        }
    }

    #[test]
    fn no_label_and_the_local_label_both_stay_local() {
        // The v1 behaviour, preserved: a profile that pins nothing rides the
        // window's route, and pinning the window's own machine is the same
        // thing said explicitly.
        let fleet = [host("studio", true, None), host("forge", false, Some("10.0.0.7:7717"))];
        assert_eq!(resolve_host(None, &fleet), HostTarget::Local);
        assert_eq!(resolve_host(Some("studio"), &fleet), HostTarget::Local);
        assert_eq!(
            resolve_host(Some(""), &fleet),
            HostTarget::Local,
            "an empty label is the file's spelling of unset, like profiles.rs reads it"
        );
    }

    #[test]
    fn a_remote_label_resolves_to_its_advertised_address() {
        let fleet = [host("studio", true, None), host("forge", false, Some("10.0.0.7:7717"))];
        assert_eq!(
            resolve_host(Some("forge"), &fleet),
            HostTarget::Remote {
                host: HostId::from_bytes([2; 32]),
                addr: "10.0.0.7:7717".to_string()
            }
        );
        assert_eq!(
            resolve_host(Some("FORGE"), &fleet),
            resolve_host(Some("forge"), &fleet),
            "labels are display names; case must not decide which machine runs a shell"
        );
    }

    #[test]
    fn an_unknown_label_is_a_launch_that_will_fail_not_a_panic() {
        // The §12 rule this item exists for: the tab still goes up, in a
        // connecting state, and settles failed carrying this message — a
        // typo'd host must never be a silent warn! or a crash.
        let fleet = [host("studio", true, None)];
        let HostTarget::Unroutable { error } = resolve_host(Some("gone"), &fleet) else {
            panic!("an unknown label must resolve Unroutable");
        };
        assert!(error.contains("gone"), "the error names the label: {error}");

        // A host found but with an empty address set (the DNS-SD trap) is
        // the same shape: present in the listing, nothing to dial.
        let fleet = [host("sleepy", false, None)];
        assert!(matches!(resolve_host(Some("sleepy"), &fleet), HostTarget::Unroutable { .. }));
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

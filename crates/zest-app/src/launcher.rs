//! The + launcher menu's rows (design §1): what this window can launch,
//! built as data.
//!
//! The picker discipline: rows and their actions are parallel lists built in
//! one pass, so index `n` means the same thing to the renderer and the input
//! path by construction. Pure — no window, no GPU — which is what lets the
//! degradation and ordering rules live under `cargo test`.
//!
//! **Grouped by the machine that will run the row, since #268.** Not by where
//! the profile is *defined*: a profile in this laptop's config pinned to
//! `forge` and one `forge` publishes itself both launch on `forge`, so both
//! belong under it. That is §2's argument for the vertical sidebar — "which
//! machine" is structural rather than a badge — applied to the menu.

use std::collections::BTreeMap;

use zest_config::profiles;
use zest_config::Settings;
use zest_proto::HostId;

use crate::chrome::model::{tab_accent, AccentChoice, LauncherRow};
use crate::fleet::FleetHost;
use crate::tabs::ProfileIdentity;

/// Which profile a row launches, and whose definition it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileRef {
    /// A profile in this machine's own config. Resolved through the local
    /// cascade; may pin any host.
    Local(String),
    /// A profile another machine published (#262).
    ///
    /// Carries the host id rather than a label because the label is a display
    /// name and two machines may share one; the id is what the launch dials
    /// and what `expect_host` pins.
    Remote { host: HostId, name: String },
}

/// What one launcher row does, parallel to the drawn rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LauncherAction {
    /// Launch this profile — locally defined (its `host` key decides where,
    /// issue #175) or published by the machine it names (#268).
    Launch(ProfileRef),
    /// The synthetic row of an empty profiles table: a plain default shell —
    /// exactly what ⌘T spawns.
    LaunchDefault,
    /// Open the fleet picker to choose the machine, carrying the highlighted
    /// row so ⇧⏎ runs *that* profile somewhere else.
    ///
    /// `None` only when nothing actionable is highlighted. Before #268 this
    /// carried nothing at all, so ⇧⏎ discarded the row it was invoked on and
    /// opened a plain shell — the menu's one dead affordance.
    RunOnHost(Option<ProfileRef>),
    /// Open the Profiles tab.
    ManageProfiles,
    /// A divider or a group header; Enter does nothing and the selection skips it.
    None,
}

/// `Settings::profiles` re-rooted as the `toml::Table` the resolver walks —
/// the same shape `ProfileIdentity::resolve` builds. Crate-visible for the
/// profiles editor (`profiles_ui`), which resolves the same way the launcher
/// does — one encoding, one place.
pub(crate) fn profiles_root(settings: &Settings) -> toml::Table {
    let mut table = toml::Table::new();
    for (key, profile) in &settings.profiles {
        table.insert(key.clone(), toml::Value::Table(profile.clone()));
    }
    let mut root = toml::Table::new();
    root.insert("profiles".into(), toml::Value::Table(table));
    root
}

/// A profile's launch metadata — command, host, ask_host, directory —
/// resolved through Defaults, exactly as the rows here were built. The
/// launch path reads this so what a row *promised* (its command line, its
/// host chip) and what launching it *does* come from one resolution.
#[must_use]
pub fn profile_meta(settings: &Settings, name: &str) -> profiles::ProfileMeta {
    profiles::resolve_profile(&profiles_root(settings), name).meta
}

/// How the groups sort and, equally, what makes two of them the same group.
///
/// `(rank, label, host)`: rank puts `Any` first and this machine second, the
/// label keeps the fleet alphabetical, and **the id is what separates two
/// machines that happen to share a display name**. A label is a display name —
/// `ProfileRef::Remote` carries an id for exactly this reason — so keying a
/// group on the label alone would merge two machines' launch targets under one
/// header, and pressing a row would run a command on whichever of them the map
/// happened to hold.
type GroupKey = (u8, String, Option<HostId>);

/// One machine's worth of launch targets, before the rows are flattened.
struct Group {
    order: GroupKey,
    label: String,
    sub: String,
    online: bool,
    /// `(display name, command, accent, ProfileRef)`.
    entries: Vec<(String, String, AccentChoice, ProfileRef)>,
}

/// Where a profile's rows belong.
enum Bucket {
    /// `ask_host`: it pins nothing, so no machine owns it.
    Any,
    /// This window's own machine.
    Here,
    /// A machine in the fleet, by id.
    Host(HostId),
    /// A `host = "..."` naming a machine the fleet has never heard of. Kept
    /// as its own group rather than folded into `Here`, because the launch
    /// will go looking for that label and fail — a row filed under "this
    /// machine" that runs somewhere else, or nowhere, is the lie the grouping
    /// exists to prevent.
    Unknown(String),
}

/// Build the menu: launch targets grouped by the machine that will run them,
/// then the divider and the two actions.
///
/// The default row — what `⏎` runs — is a profile literally named `default`
/// when one exists, else the first profile in map order (`Settings::profiles`
/// is a `BTreeMap`, so alphabetical — deterministic, if arbitrary), else the
/// synthetic shell row: the menu never renders empty. The reserved `defaults`
/// table is hidden (`list_profiles` already filters it) — launching the layer
/// every profile falls through to is meaningless.
///
/// `fleet` carries what each machine published (#262, fetched by #265). A host
/// that has told us nothing contributes no group rather than an empty one:
/// that covers an older daemon, a machine nothing can reach, and one whose
/// watcher simply has not connected yet, and all three should read the same —
/// nobody has said anything, so nothing is drawn.
#[must_use]
pub fn build_rows(
    settings: &Settings,
    fleet: &[FleetHost],
    fallback_command: &str,
    active_profile: Option<&str>,
    manage_chord: String,
) -> (Vec<LauncherRow>, Vec<LauncherAction>) {
    let root = profiles_root(settings);
    let mut names = profiles::list_profiles(&root);
    if let Some(i) = names.iter().position(|n| n == "default") {
        let name = names.remove(i);
        names.insert(0, name);
    }

    let local_label = fleet.iter().find(|h| h.local).map(|h| h.label.as_str());
    let mut groups: BTreeMap<GroupKey, Group> = BTreeMap::new();

    // This machine's own profiles first, so a name they share with a
    // published one wins: the local definition is the one the user can edit.
    for name in &names {
        let resolved = profiles::resolve_profile(&root, name);
        let bucket = bucket_for(resolved.meta.ask_host, resolved.meta.host.as_deref(), fleet);
        let identity = ProfileIdentity::resolve(settings, name);
        let group = group_for(&mut groups, &bucket, fleet, local_label);
        group.entries.push((
            name.clone(),
            resolved.meta.command.unwrap_or_else(|| fallback_command.to_string()),
            tab_accent(Some(&identity), 0),
            ProfileRef::Local(name.clone()),
        ));
    }

    // Then what each machine published about itself.
    for host in fleet.iter().filter(|h| !h.local) {
        let Some(offer) = host.offer.as_ref() else { continue };
        for profile in &offer.profiles {
            let group = group_for(&mut groups, &Bucket::Host(host.host), fleet, local_label);
            // A local definition pinned here already claimed this name, and it
            // wins: it is the one the user can open the editor on. Two rows
            // spelled the same, launching different commands, is the worse
            // menu by a distance.
            if group.entries.iter().any(|(n, ..)| n == &profile.name) {
                continue;
            }
            let identity = ProfileIdentity::from_published(profile);
            group.entries.push((
                profile.name.clone(),
                shown_command(&profile.command, &offer.default_shell),
                tab_accent(Some(&identity), 0),
                ProfileRef::Remote { host: host.host, name: profile.name.clone() },
            ));
        }
    }

    let mut groups: Vec<Group> = groups.into_values().filter(|g| !g.entries.is_empty()).collect();
    groups.sort_by(|a, b| a.order.cmp(&b.order));

    // Headers whenever there is something to say. A single-machine setup —
    // which is most setups — must not grow chrome to say "this machine" over
    // the only rows there are; but a single group that is *not* this machine
    // is precisely the case worth naming, and dropping its header would leave
    // a menu whose every row runs somewhere else without saying where.
    let headed = groups.len() > 1 || !matches!(groups.first().map(|g| g.order.0), None | Some(1));

    let mut rows = Vec::new();
    let mut actions = Vec::new();
    if groups.is_empty() {
        // The degradation row: an empty profiles table still offers the
        // resolved shell, tagged default, so the menu's contract ("⏎ runs
        // the default") holds before the user has written a single profile.
        rows.push(LauncherRow::Profile {
            name: "Default shell".into(),
            command: fallback_command.to_string(),
            host_label: None,
            default: true,
            digit: Some(1),
            active: false,
            accent: AccentChoice::Host(0),
        });
        actions.push(LauncherAction::LaunchDefault);
    }

    // Digits run 1..=9 across the whole menu, not per group: they are what the
    // keyboard presses, and a number that restarted at each header would name
    // several rows at once.
    let mut digit = 0u8;
    let mut first = true;
    for group in &groups {
        if headed {
            rows.push(LauncherRow::Group {
                label: group.label.clone(),
                sub: group.sub.clone(),
                online: group.online,
            });
            actions.push(LauncherAction::None);
        }
        for (name, command, accent, target) in &group.entries {
            digit += 1;
            rows.push(LauncherRow::Profile {
                name: name.clone(),
                command: command.clone(),
                // The header already says which machine, so the chip would be
                // the same word twice on every row. Without headers there is
                // only one group, and it is this machine.
                host_label: None,
                default: first,
                digit: (digit <= 9).then_some(digit),
                active: matches!(target, ProfileRef::Local(n) if Some(n.as_str()) == active_profile),
                accent: *accent,
            });
            actions.push(LauncherAction::Launch(target.clone()));
            first = false;
        }
    }

    rows.push(LauncherRow::Divider);
    actions.push(LauncherAction::None);
    rows.push(LauncherRow::RunOnHost);
    // Filled by the caller from the highlighted row; the builder does not know
    // where the selection is.
    actions.push(LauncherAction::RunOnHost(None));
    rows.push(LauncherRow::ManageProfiles { chord: manage_chord });
    actions.push(LauncherAction::ManageProfiles);

    (rows, actions)
}

/// What a row promises it will run, when the command may be the far host's own.
///
/// An empty command means *that host's* default shell — the same convention
/// `CreateSession.command` uses — and a client cannot substitute its own, which
/// is how a Mac row ends up promising `pwsh -NoLogo`. So: the far host's shell
/// when it told us one, and an honest phrase when it did not.
///
/// Shared by the launcher row and the connecting tab's provenance line, because
/// they disagreed once: the row read `zsh -l` and the tab it opened read "the
/// host's default shell", for the same launch, three lines apart.
#[must_use]
pub fn shown_command(command: &str, host_default_shell: &str) -> String {
    if !command.is_empty() {
        return command.to_string();
    }
    if host_default_shell.is_empty() {
        return "the host's default shell".to_string();
    }
    host_default_shell.to_string()
}

/// Which machine a locally-defined profile belongs under.
fn bucket_for(ask_host: bool, host: Option<&str>, fleet: &[FleetHost]) -> Bucket {
    if ask_host {
        return Bucket::Any;
    }
    // An empty `host` key is the file's spelling of unset, exactly as
    // `launch::resolve_host` reads it.
    let Some(label) = host.map(str::trim).filter(|l| !l.is_empty()) else {
        return Bucket::Here;
    };
    // Labels are display names, so the match is ASCII case-insensitive — the
    // same rule `resolve_host` uses, because these two must never disagree
    // about which machine a row names.
    match fleet.iter().find(|h| h.label.eq_ignore_ascii_case(label)) {
        Some(h) if h.local => Bucket::Here,
        Some(h) => Bucket::Host(h.host),
        None => Bucket::Unknown(label.to_string()),
    }
}

/// The group a bucket names, created on first use.
fn group_for<'a>(
    groups: &'a mut BTreeMap<GroupKey, Group>,
    bucket: &Bucket,
    fleet: &[FleetHost],
    local_label: Option<&str>,
) -> &'a mut Group {
    let (order, label, sub, online): (GroupKey, _, _, _) = match bucket {
        // First: these belong to no machine, and ⇧⏎ is what resolves them.
        Bucket::Any => {
            ((0, String::new(), None), "any machine".to_string(), String::new(), true)
        }
        Bucket::Here => (
            (1, String::new(), None),
            match local_label {
                Some(l) => format!("this machine \u{b7} {l}"),
                None => "this machine".to_string(),
            },
            String::new(),
            true,
        ),
        Bucket::Host(id) => {
            let host = fleet.iter().find(|h| h.host == *id);
            let label = host.map_or_else(|| id.short(), |h| h.label.clone());
            (
                // The id disambiguates two machines with one display name;
                // the label still leads, so the fleet stays alphabetical.
                (2, label.clone(), Some(*id)),
                label,
                host.map(host_sub).unwrap_or_default(),
                host.is_some_and(FleetHost::is_online),
            )
        }
        // Last, and offline: the fleet has never heard of this label, so the
        // launch will fail. The row still exists — §12 says a launch at a
        // sleeping host is a connecting tab, not a refusal — but the header
        // says not to expect much.
        Bucket::Unknown(label) => {
            ((3, label.clone(), None), label.clone(), "not in the fleet".to_string(), false)
        }
    };
    groups.entry(order.clone()).or_insert_with(|| Group {
        order,
        label,
        sub,
        online,
        entries: Vec::new(),
    })
}

/// A host header's mono sub-label: what it runs, and how we reach it.
fn host_sub(host: &FleetHost) -> String {
    if !host.is_online() {
        return "asleep".to_string();
    }
    let os = host.offer.as_ref().map(|o| o.os.as_str()).filter(|o| !o.is_empty());
    let path = match host.reachability {
        Some(zest_mesh::Reachability::Loopback) => Some("loopback".to_string()),
        Some(zest_mesh::Reachability::Lan) => Some(match host.rtt_ms {
            Some(ms) => format!("LAN \u{b7} {}", crate::chrome::layout::format_ms(ms)),
            None => "LAN".to_string(),
        }),
        Some(zest_mesh::Reachability::Cloud) => Some(match host.rtt_ms {
            Some(ms) => format!("tunnel \u{b7} {}", crate::chrome::layout::format_ms(ms)),
            None => "tunnel".to_string(),
        }),
        None => None,
    };
    match (os, path) {
        (Some(os), Some(path)) => format!("{os} \u{b7} {path}"),
        (Some(os), None) => os.to_string(),
        (None, Some(path)) => path,
        (None, None) => String::new(),
    }
}

/// The action a plain digit runs while the menu is open, over the *action*
/// list (the input path keeps actions, not rows, between passes).
///
/// **Counts launch rows rather than indexing into the list**, since #268 put
/// group headers between them: `d - 1` was right only while profile rows led
/// the list unbroken, and with one header above them every digit ran the row
/// before the one it names. A test pins this against the rows' own drawn
/// `digit` field so the two views cannot drift again.
#[must_use]
pub fn digit_action_index(actions: &[LauncherAction], digit: u8) -> Option<usize> {
    let nth = usize::from(digit.checked_sub(1)?);
    actions
        .iter()
        .enumerate()
        .filter(|(_, a)| {
            matches!(a, LauncherAction::Launch(_) | LauncherAction::LaunchDefault)
        })
        .nth(nth)
        .map(|(i, _)| i)
}

/// The next actionable row in the given direction, or `from` at the edge —
/// the divider is a line, and the keyboard never rests on a line.
#[must_use]
pub fn step(actions: &[LauncherAction], from: usize, down: bool) -> usize {
    let actionable = |i: &usize| !matches!(actions.get(*i), Some(LauncherAction::None) | None);
    if down {
        (from + 1..actions.len()).find(actionable).unwrap_or(from)
    } else {
        (0..from).rev().find(actionable).unwrap_or(from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zest_mesh::discovery::Presence;

    /// The digit mapping as *drawn* — read off the rows' own `digit` field.
    /// The test oracle `digit_action_index` is pinned against, so the input
    /// path and the pixels cannot disagree about what "press 3" means.
    fn digit_row(rows: &[LauncherRow], digit: u8) -> Option<usize> {
        rows.iter().position(
            |row| matches!(row, LauncherRow::Profile { digit: Some(d), .. } if *d == digit),
        )
    }

    fn settings_with(profiles: &[(&str, &str)]) -> Settings {
        let mut s = Settings::default();
        for (name, toml) in profiles {
            s.profiles.insert((*name).to_string(), toml.parse().expect("valid toml"));
        }
        s
    }

    fn host(id: u8, label: &str, local: bool) -> FleetHost {
        FleetHost {
            host: HostId::from_bytes([id; 32]),
            label: label.into(),
            presence: Presence::Online,
            local,
            address: (!local).then(|| "10.0.0.7:7717".to_string()),
            reachability: Some(if local {
                zest_mesh::Reachability::Loopback
            } else {
                zest_mesh::Reachability::Lan
            }),
            rtt_ms: (!local).then_some(0.4),
            sessions: crate::fleet::SessionsState::Unknown,
            offer: None,
            enrolled: false,
            relay_online: false,
        }
    }

    fn offering(mut h: FleetHost, os: &str, profiles: &[(&str, &str)]) -> FleetHost {
        h.offer = Some(zest_proto::HostOffer {
            os: os.into(),
            default_shell: "zsh -l".into(),
            profiles: profiles
                .iter()
                .map(|(name, command)| zest_proto::HostProfile {
                    name: (*name).to_string(),
                    command: (*command).to_string(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        });
        h
    }

    /// The drawn menu as `(group label, [row names])`, headers included.
    fn shape(rows: &[LauncherRow]) -> Vec<(String, Vec<String>)> {
        let mut out: Vec<(String, Vec<String>)> = Vec::new();
        for row in rows {
            match row {
                LauncherRow::Group { label, .. } => out.push((label.clone(), Vec::new())),
                LauncherRow::Profile { name, .. } => {
                    if out.is_empty() {
                        out.push((String::new(), Vec::new()));
                    }
                    out.last_mut().expect("a group").1.push(name.clone());
                }
                _ => {}
            }
        }
        out
    }

    #[test]
    fn another_machines_profiles_appear_under_that_machine() {
        // The reported bug, and the whole point of #268: the `+` menu showed
        // nothing from any host but this one, because a machine's launch
        // targets could not cross the wire at all until #262.
        let settings = settings_with(&[("local-only", "command = \"zsh\"")]);
        let fleet = [
            host(1, "studio", true),
            offering(host(2, "forge", false), "windows", &[("ubuntu", "wsl.exe -d Ubuntu")]),
        ];
        let (rows, actions) = build_rows(&settings, &fleet, "sh", None, String::new());

        assert_eq!(
            shape(&rows),
            vec![
                ("this machine \u{b7} studio".to_string(), vec!["local-only".to_string()]),
                ("forge".to_string(), vec!["ubuntu".to_string()]),
            ],
            "grouped by the machine that will run each row"
        );
        assert!(
            actions.contains(&LauncherAction::Launch(ProfileRef::Remote {
                host: HostId::from_bytes([2; 32]),
                name: "ubuntu".into(),
            })),
            "and the remote row launches the *published* profile, not a local name lookup"
        );
    }

    #[test]
    fn a_local_profile_is_grouped_by_where_it_runs_not_where_it_lives() {
        // The rule that makes "which machine" structural (§2, applied to the
        // menu): a profile in *this* config pinned to forge belongs under
        // forge, beside forge's own — because that is where pressing it runs.
        let settings = settings_with(&[("deploy", "host = \"forge\"\ncommand = \"make ship\"")]);
        let fleet = [
            host(1, "studio", true),
            offering(host(2, "forge", false), "windows", &[("pwsh", "pwsh -NoLogo")]),
        ];
        let (rows, _) = build_rows(&settings, &fleet, "sh", None, String::new());
        assert_eq!(
            shape(&rows),
            vec![("forge".to_string(), vec!["deploy".to_string(), "pwsh".to_string()])],
            "one group, and no `this machine` header for a machine with no rows"
        );
    }

    #[test]
    fn a_name_defined_both_here_and_there_shows_once_and_the_local_one_wins() {
        // Two rows spelled the same, launching different commands, is the
        // worse menu by a distance. The local definition wins because it is
        // the one the user can open the editor on.
        let settings = settings_with(&[("nightly", "host = \"forge\"\ncommand = \"cargo watch\"")]);
        let fleet = [
            host(1, "studio", true),
            offering(host(2, "forge", false), "linux", &[("nightly", "make nightly")]),
        ];
        let (rows, actions) = build_rows(&settings, &fleet, "sh", None, String::new());

        let names: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                LauncherRow::Profile { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(names, ["nightly"], "once, not twice");
        assert_eq!(
            actions[1],
            LauncherAction::Launch(ProfileRef::Local("nightly".into())),
            "and it is the local definition — the editable one"
        );
        match &rows[1] {
            LauncherRow::Profile { command, .. } => {
                assert_eq!(command, "cargo watch", "so the row promises the local command");
            }
            other => panic!("expected a profile row, got {other:?}"),
        }
    }

    #[test]
    fn one_machine_grows_no_headers_at_all() {
        // Most setups are one machine, and the menu must not sprout chrome to
        // say "this machine" over the only rows there are.
        let settings = settings_with(&[("a", ""), ("b", "")]);
        let (rows, _) = build_rows(&settings, &[host(1, "studio", true)], "sh", None, String::new());
        assert!(
            !rows.iter().any(|r| matches!(r, LauncherRow::Group { .. })),
            "no group headers with nothing to distinguish"
        );

        // And no fleet at all — the very first launch, before discovery has
        // said anything — is the same menu.
        let (bare, _) = build_rows(&settings, &[], "sh", None, String::new());
        assert!(!bare.iter().any(|r| matches!(r, LauncherRow::Group { .. })));
    }

    #[test]
    fn a_host_that_has_told_us_nothing_contributes_no_group() {
        // An older daemon, a machine nothing can reach, and one whose watcher
        // has not connected yet all read the same: nobody has said anything,
        // so nothing is drawn. An empty group would claim the machine has no
        // profiles, which is a different and wrong statement.
        let settings = settings_with(&[("here", "")]);
        let fleet = [host(1, "studio", true), host(2, "forge", false)];
        let (rows, _) = build_rows(&settings, &fleet, "sh", None, String::new());
        assert!(
            !rows.iter().any(|r| matches!(r, LauncherRow::Group { .. })),
            "one machine has rows, so there is nothing to distinguish and no headers"
        );
        assert_eq!(
            shape(&rows),
            vec![(String::new(), vec!["here".to_string()])],
            "and forge contributes nothing at all"
        );
    }

    #[test]
    fn ask_host_profiles_lead_in_a_group_of_their_own() {
        // They pin no machine by design, so no machine owns them — and ⇧⏎ (or
        // launching one) is what resolves where they go.
        let settings = settings_with(&[
            ("anywhere", "ask_host = true"),
            ("here", ""),
        ]);
        let fleet = [host(1, "studio", true)];
        let (rows, _) = build_rows(&settings, &fleet, "sh", None, String::new());
        assert_eq!(
            shape(&rows),
            vec![
                ("any machine".to_string(), vec!["anywhere".to_string()]),
                ("this machine \u{b7} studio".to_string(), vec!["here".to_string()]),
            ],
            "first, because a host-agnostic target is not a fact about any machine"
        );
    }

    #[test]
    fn a_pin_the_fleet_has_never_heard_of_is_its_own_group_and_says_so() {
        // §12 keeps the launch — a typo'd host is a connecting tab that
        // settles failed, never a silent refusal — but filing the row under
        // "this machine" would be the lie the grouping exists to prevent.
        let settings = settings_with(&[("ghost", "host = \"nowhere\""), ("here", "")]);
        let fleet = [host(1, "studio", true)];
        let (rows, _) = build_rows(&settings, &fleet, "sh", None, String::new());

        let group = rows
            .iter()
            .find_map(|r| match r {
                LauncherRow::Group { label, sub, online } if label == "nowhere" => {
                    Some((sub.clone(), *online))
                }
                _ => None,
            })
            .expect("a group for the unknown label");
        assert_eq!(group.0, "not in the fleet", "the header says why nothing will happen");
        assert!(!group.1, "and it is not drawn as online");
    }

    #[test]
    fn digits_run_one_to_nine_across_the_whole_menu_not_per_group() {
        // They are what the keyboard presses, so a number that restarted at
        // each header would name several rows at once.
        let settings = settings_with(&[("a", ""), ("b", "")]);
        let fleet = [
            host(1, "studio", true),
            offering(host(2, "forge", false), "linux", &[("c", "x"), ("d", "y")]),
        ];
        let (rows, actions) = build_rows(&settings, &fleet, "sh", None, String::new());

        let digits: Vec<Option<u8>> = rows
            .iter()
            .filter_map(|r| match r {
                LauncherRow::Profile { digit, .. } => Some(*digit),
                _ => None,
            })
            .collect();
        assert_eq!(digits, vec![Some(1), Some(2), Some(3), Some(4)], "continuous across groups");
        for d in 1..=9u8 {
            assert_eq!(
                digit_row(&rows, d),
                digit_action_index(&actions, d),
                "digit {d}: the drawn mapping and the input path's must agree"
            );
        }
    }

    #[test]
    fn only_the_very_first_row_wears_the_default_tag() {
        // "⏎ runs the default" is a header over the whole menu, so exactly one
        // row can answer to it however many groups there are.
        let settings = settings_with(&[("default", ""), ("other", "host = \"forge\"")]);
        let fleet = [
            host(1, "studio", true),
            offering(host(2, "forge", false), "linux", &[("theirs", "x")]),
        ];
        let (rows, actions) = build_rows(&settings, &fleet, "sh", None, String::new());
        let tagged: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                LauncherRow::Profile { name, default: true, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(tagged, ["default"], "one row, and it is the one ⏎ runs");
        assert_eq!(actions[1], LauncherAction::Launch(ProfileRef::Local("default".into())));
    }

    #[test]
    fn an_empty_profiles_table_degrades_to_one_default_row() {
        // The menu never renders empty: with nothing configured it offers
        // the resolved shell, tagged default, numbered 1 — or "⏎ runs the
        // default" is a header above nothing.
        let (rows, actions) =
            build_rows(&Settings::default(), &[], "pwsh -NoLogo", None, String::new());
        match &rows[0] {
            LauncherRow::Profile { name, command, default, digit, .. } => {
                assert_eq!(name, "Default shell");
                assert_eq!(command, "pwsh -NoLogo", "the resolved shell.command is what it runs");
                assert!(default, "the synthetic row is the default — it is all there is");
                assert_eq!(*digit, Some(1));
            }
            other => panic!("row 0 must be the synthetic profile, got {other:?}"),
        }
        assert_eq!(actions[0], LauncherAction::LaunchDefault);
        // The two actions still follow: a menu of one profile is not a menu
        // of one row.
        assert!(matches!(rows[1], LauncherRow::Divider));
        assert!(matches!(rows[2], LauncherRow::RunOnHost));
        assert!(matches!(rows[3], LauncherRow::ManageProfiles { .. }));
    }

    #[test]
    fn the_reserved_defaults_table_is_never_a_launchable_row() {
        let settings = settings_with(&[("defaults", "icon = \"star\""), ("real", "")]);
        let (rows, _) = build_rows(&settings, &[], "sh", None, String::new());
        let names: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                LauncherRow::Profile { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(names, ["real"], "the layer every profile falls through to is not a target");
    }

    #[test]
    fn a_profile_without_a_command_shows_the_resolved_shell() {
        let (rows, _) =
            build_rows(&settings_with(&[("bare", "")]), &[], "pwsh -NoLogo", None, String::new());
        match &rows[0] {
            LauncherRow::Profile { command, .. } => assert_eq!(
                command, "pwsh -NoLogo",
                "an empty command falls through to shell.command, like the launch will"
            ),
            other => panic!("expected a profile row, got {other:?}"),
        }
    }

    #[test]
    fn a_published_profile_with_no_command_promises_the_far_shell_never_ours() {
        // The bug this prevents is a Mac row reading `pwsh -NoLogo` because
        // that is what *this* machine would have run. An empty published
        // command means the far host's own default, and it says which.
        let fleet = [
            host(1, "studio", true),
            offering(host(2, "forge", false), "linux", &[("plain", "")]),
        ];
        let (rows, _) =
            build_rows(&settings_with(&[("here", "")]), &fleet, "pwsh -NoLogo", None, String::new());
        let command = rows
            .iter()
            .find_map(|r| match r {
                LauncherRow::Profile { name, command, .. } if name == "plain" => Some(command),
                _ => None,
            })
            .expect("the published row");
        assert_eq!(command, "zsh -l", "the far host's default shell, which it told us");
    }

    #[test]
    fn two_machines_sharing_a_display_name_stay_two_groups() {
        // A label is a display name — `ProfileRef::Remote` carries an id for
        // exactly this reason — so keying a group on the label alone merges two
        // machines' launch targets under one header, and pressing a row runs a
        // command on whichever of them the map happened to hold. Two laptops
        // both called `mac` is not exotic.
        let fleet = [
            host(1, "studio", true),
            offering(host(2, "mac", false), "macos", &[("build", "make")]),
            offering(host(3, "mac", false), "macos", &[("test", "make test")]),
        ];
        let (rows, actions) =
            build_rows(&settings_with(&[("here", "")]), &fleet, "sh", None, String::new());

        let headers: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                LauncherRow::Group { label, .. } => Some(label.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            headers.iter().filter(|l| **l == "mac").count(),
            2,
            "one header each, however alike they are called: {headers:?}"
        );
        // And each row still names the machine it will actually dial.
        for (id, name) in [(2u8, "build"), (3u8, "test")] {
            assert!(
                actions.contains(&LauncherAction::Launch(ProfileRef::Remote {
                    host: HostId::from_bytes([id; 32]),
                    name: name.into(),
                })),
                "{name} must launch on the machine that published it"
            );
        }
    }

    #[test]
    fn a_row_and_the_tab_it_opens_promise_the_same_command() {
        // These disagreed once, three lines apart: the row read `zsh -l` and
        // the connecting tab it opened read "the host's default shell", for
        // one launch. One function now answers both.
        assert_eq!(shown_command("wsl.exe", "pwsh"), "wsl.exe", "its own command outranks");
        assert_eq!(
            shown_command("", "zsh -l"),
            "zsh -l",
            "empty means the far host's shell, and it told us which"
        );
        assert_eq!(
            shown_command("", ""),
            "the host's default shell",
            "and an honest phrase when it did not — never this machine's shell, \
             which is how a Mac row ends up promising pwsh"
        );
    }

    #[test]
    fn a_group_header_is_never_actionable() {
        // It is context, not a thing to click: the keyboard steps over it and
        // the layout gives it no hit region.
        let fleet = [
            host(1, "studio", true),
            offering(host(2, "forge", false), "linux", &[("theirs", "x")]),
        ];
        let (rows, actions) =
            build_rows(&settings_with(&[("here", "")]), &fleet, "sh", None, String::new());
        for (i, row) in rows.iter().enumerate() {
            if matches!(row, LauncherRow::Group { .. }) {
                assert_eq!(actions[i], LauncherAction::None, "row {i} is a header");
                assert_ne!(step(&actions, i.saturating_sub(1), true), i, "down skips it");
                assert_ne!(step(&actions, i + 1, false), i, "and up does too");
            }
        }
    }

    #[test]
    fn digits_map_to_profile_rows_and_nothing_else() {
        let (rows, actions) =
            build_rows(&settings_with(&[("a", ""), ("b", "")]), &[], "sh", None, String::new());
        assert_eq!(digit_row(&rows, 1), Some(0));
        assert_eq!(digit_row(&rows, 2), Some(1));
        assert_eq!(digit_row(&rows, 3), None, "no third profile, no third digit");
        assert_eq!(digit_action_index(&actions, 0), None, "there is no digit 0");
        // And stepping never rests on the divider.
        let divider =
            rows.iter().position(|r| matches!(r, LauncherRow::Divider)).expect("divider");
        assert_ne!(step(&actions, divider - 1, true), divider, "down skips the line");
        assert_ne!(step(&actions, divider + 1, false), divider, "up skips it too");
    }
}

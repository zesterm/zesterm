//! The + launcher menu's rows (design §1): what this window can launch,
//! built as data.
//!
//! The picker discipline: rows and their actions are parallel lists built in
//! one pass, so index `n` means the same thing to the renderer and the input
//! path by construction. Pure — no window, no GPU — which is what lets the
//! degradation and ordering rules live under `cargo test`.

use zest_config::profiles;
use zest_config::Settings;

use crate::chrome::model::{tab_accent, AccentChoice, LauncherRow};
use crate::tabs::ProfileIdentity;

/// What one launcher row does, parallel to the drawn rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LauncherAction {
    /// Launch this named profile: its command on the host its `host` key
    /// pins (the window's route when it pins none — issue #175).
    Launch(String),
    /// The synthetic row of an empty profiles table: a plain default shell —
    /// exactly what ⌘T spawns.
    LaunchDefault,
    /// Open the fleet picker to choose the machine.
    RunOnHost,
    /// Open the Profiles tab.
    ManageProfiles,
    /// The divider; Enter does nothing and the selection skips it.
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

/// Build the menu: launch targets first (default row leading), then the
/// divider and the two actions.
///
/// The default row — what `⏎` runs — is a profile literally named `default`
/// when one exists, else the first profile in map order (`Settings::profiles`
/// is a `BTreeMap`, so alphabetical — deterministic, if arbitrary), else the
/// synthetic shell row: the menu never renders empty. The reserved `defaults`
/// table is hidden (`list_profiles` already filters it) — launching the layer
/// every profile falls through to is meaningless.
#[must_use]
pub fn build_rows(
    settings: &Settings,
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

    let mut rows = Vec::new();
    let mut actions = Vec::new();
    if names.is_empty() {
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
    for (i, name) in names.iter().enumerate() {
        let resolved = profiles::resolve_profile(&root, name);
        // The accent arrives on the row, resolved with the same truth table
        // the tab chips use — a launcher row should look like the tab it is
        // about to become. Host slot 0 because this builder is pure over
        // Settings and the window's host→slot table lives with the chrome;
        // the host itself is carried as text on the chip, which is the
        // design's rule for origin anyway (text, never colour alone).
        let identity = ProfileIdentity::resolve(settings, name);
        rows.push(LauncherRow::Profile {
            name: name.clone(),
            command: resolved
                .meta
                .command
                .unwrap_or_else(|| fallback_command.to_string()),
            host_label: resolved.meta.host,
            default: i == 0,
            digit: u8::try_from(i + 1).ok().filter(|d| *d <= 9),
            active: active_profile == Some(name.as_str()),
            accent: tab_accent(Some(&identity), 0),
        });
        actions.push(LauncherAction::Launch(name.clone()));
    }

    rows.push(LauncherRow::Divider);
    actions.push(LauncherAction::None);
    rows.push(LauncherRow::RunOnHost);
    actions.push(LauncherAction::RunOnHost);
    rows.push(LauncherRow::ManageProfiles { chord: manage_chord });
    actions.push(LauncherAction::ManageProfiles);

    (rows, actions)
}

/// The action a plain digit runs while the menu is open, over the *action*
/// list (the input path keeps actions, not rows, between passes). Profile
/// rows lead the list numbered from 1 by construction, so digit `d` names
/// index `d - 1` — the kind guard is what keeps a digit past the profiles
/// from running the divider or an action row, and a test pins this against
/// the rows' own drawn `digit` field so the two views cannot drift.
#[must_use]
pub fn digit_action_index(actions: &[LauncherAction], digit: u8) -> Option<usize> {
    let i = usize::from(digit.checked_sub(1)?);
    matches!(
        actions.get(i),
        Some(LauncherAction::Launch(_) | LauncherAction::LaunchDefault)
    )
    .then_some(i)
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

    #[test]
    fn an_empty_profiles_table_degrades_to_one_default_row() {
        // The menu never renders empty: with nothing configured it offers
        // the resolved shell, tagged default, numbered 1 — or "⏎ runs the
        // default" is a header above nothing.
        let (rows, actions) = build_rows(&Settings::default(), "pwsh -NoLogo", None, String::new());
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
    fn a_populated_table_hides_defaults_orders_default_first_and_numbers_nine() {
        // Eleven profiles: `defaults` (reserved, hidden), `default` (leads),
        // then nine more alphabetically — of which only the first eight get
        // digits, because `default` took 1.
        let mut entries: Vec<(String, String)> = (b'a'..=b'i')
            .map(|c| (format!("p{}", c as char), String::new()))
            .collect();
        entries.push(("default".into(), "command = \"zsh -l\"".into()));
        entries.push(("defaults".into(), "icon = \"star\"".into()));
        let borrowed: Vec<(&str, &str)> =
            entries.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
        let (rows, actions) = build_rows(&settings_with(&borrowed), "sh", Some("pb"), "⌘⇧,".into());

        let profiles: Vec<(&str, bool, Option<u8>, bool)> = rows
            .iter()
            .filter_map(|r| match r {
                LauncherRow::Profile { name, default, digit, active, .. } => {
                    Some((name.as_str(), *default, *digit, *active))
                }
                _ => None,
            })
            .collect();
        assert_eq!(profiles.len(), 10, "`defaults` is reserved and never listed");
        assert!(
            profiles.iter().all(|(n, ..)| *n != "defaults"),
            "the reserved parent must not appear as a launchable sibling"
        );
        assert_eq!(
            (profiles[0].0, profiles[0].1),
            ("default", true),
            "a profile literally named `default` leads and carries the tag"
        );
        assert!(
            profiles.iter().skip(1).all(|(_, d, ..)| !d),
            "exactly one row wears the default tag"
        );
        let digits: Vec<Option<u8>> = profiles.iter().map(|(_, _, d, _)| *d).collect();
        assert_eq!(
            digits,
            (1..=9).map(Some).chain([None]).collect::<Vec<_>>(),
            "the first nine rows are numbered; the tenth has no digit to press"
        );
        assert!(
            profiles.iter().any(|(n, _, _, a)| *n == "pb" && *a),
            "the active tab's profile row says so"
        );
        assert_eq!(
            actions[0],
            LauncherAction::Launch("default".into()),
            "row and action lists are parallel"
        );
    }

    #[test]
    fn without_a_default_name_the_first_by_map_order_leads() {
        // Documented choice: no profile named `default` means the first in
        // BTreeMap (alphabetical) order takes the tag — deterministic, if
        // arbitrary, and stable across reloads.
        let (rows, _) = build_rows(
            &settings_with(&[("zulu", ""), ("alpha", "")]),
            "sh",
            None,
            String::new(),
        );
        match &rows[0] {
            LauncherRow::Profile { name, default, .. } => {
                assert_eq!(name, "alpha");
                assert!(default);
            }
            other => panic!("expected a profile row, got {other:?}"),
        }
    }

    #[test]
    fn a_profile_without_a_command_shows_the_resolved_shell() {
        let (rows, _) = build_rows(&settings_with(&[("bare", "")]), "pwsh -NoLogo", None, String::new());
        match &rows[0] {
            LauncherRow::Profile { command, .. } => assert_eq!(
                command, "pwsh -NoLogo",
                "an empty command falls through to shell.command, like the launch will"
            ),
            other => panic!("expected a profile row, got {other:?}"),
        }
    }

    #[test]
    fn host_chips_appear_exactly_when_a_host_is_pinned() {
        // The chip tells the truth about where the row launches (issue
        // #175): a profile with a `host` key carries it — its own, or
        // Defaults' when inherited — and a host-less profile carries none,
        // because a chip naming a machine the launch will not use is the
        // dead-affordance rule inverted.
        let (rows, _) = build_rows(
            &settings_with(&[
                ("defaults", "host = \"studio\""),
                ("pinned", "host = \"forge\""),
                ("unpinned", "host = \"\""),
            ]),
            "sh",
            None,
            String::new(),
        );
        let chip = |name: &str| {
            rows.iter()
                .find_map(|r| match r {
                    LauncherRow::Profile { name: n, host_label, .. } if n == name => {
                        Some(host_label.clone())
                    }
                    _ => None,
                })
                .expect("row exists")
        };
        assert_eq!(chip("pinned").as_deref(), Some("forge"), "its own pin");
        assert_eq!(
            chip("unpinned").as_deref(),
            Some("studio"),
            "an empty host key is unset, so Defaults' pin falls through — the \
             chip must say where the launch actually goes"
        );

        // And with no host anywhere, no chip at all.
        let (rows, _) = build_rows(&settings_with(&[("plain", "")]), "sh", None, String::new());
        assert!(
            matches!(&rows[0], LauncherRow::Profile { host_label: None, .. }),
            "no host key in the whole cascade: the row draws no chip"
        );
    }

    #[test]
    fn digits_map_to_profile_rows_and_nothing_else() {
        let (rows, actions) = build_rows(
            &settings_with(&[("a", ""), ("b", "")]),
            "sh",
            None,
            String::new(),
        );
        assert_eq!(digit_row(&rows, 1), Some(0));
        assert_eq!(digit_row(&rows, 2), Some(1));
        assert_eq!(digit_row(&rows, 3), None, "no third profile, no third digit");
        assert_eq!(digit_row(&rows, 9), None);
        for d in 1..=9u8 {
            assert_eq!(
                digit_row(&rows, d),
                digit_action_index(&actions, d),
                "digit {d}: the drawn mapping and the input path's must agree"
            );
        }
        assert_eq!(digit_action_index(&actions, 0), None, "there is no digit 0");
        // And stepping never rests on the divider.
        let divider = rows.iter().position(|r| matches!(r, LauncherRow::Divider)).expect("divider");
        assert_ne!(step(&actions, divider - 1, true), divider, "down skips the line");
        assert_ne!(step(&actions, divider + 1, false), divider, "up skips it too");
    }
}

//! Merging layers, and remembering where each value came from.
//!
//! Provenance is the part that is easy to skip and expensive to add back. Without
//! it the settings UI can only show a value; with it, it can say *"set by profile
//! `k8s-prod`"* — which is the difference between a user finding the override
//! that is fighting them and filing a bug.
//!
//! Merging happens on `toml::Table`, not on [`Settings`], and that is deliberate.
//! A typed merge cannot tell "the user wrote `opacity = 1.0`" apart from "the
//! user wrote nothing and the default is 1.0", so every layer would appear to set
//! every field.

use std::collections::BTreeMap;

use crate::settings::Settings;

/// Where a value came from. Ordered weakest to strongest.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// Compiled-in default.
    Default,
    /// The user's config file.
    User,
    /// A named profile within the user's config.
    Profile(String),
    /// A `.zesterm.toml` in the working directory, after the user trusted it.
    Workspace,
    /// A command-line flag. Always wins.
    CommandLine,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Default => f.write_str("default"),
            Self::User => f.write_str("config file"),
            Self::Profile(p) => write!(f, "profile `{p}`"),
            Self::Workspace => f.write_str("workspace config"),
            Self::CommandLine => f.write_str("command line"),
        }
    }
}

/// One layer waiting to be merged.
#[derive(Debug, Clone)]
pub struct Layer {
    pub source: Source,
    pub table: toml::Table,
}

impl Layer {
    #[must_use]
    pub fn new(source: Source, table: toml::Table) -> Self {
        Self { source, table }
    }

    /// A layer from TOML text, or the parse error.
    pub fn parse(source: Source, text: &str) -> Result<Self, toml::de::Error> {
        Ok(Self { source, table: text.parse::<toml::Table>()? })
    }
}

/// Settings, plus where every value came from.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub settings: Settings,
    /// Dotted key to the layer that last wrote it. Only non-default entries.
    pub provenance: BTreeMap<String, Source>,
    /// Keys present in the files that the schema does not know.
    ///
    /// Kept rather than discarded so the user can be told about a typo. A key
    /// from a *newer* version looks identical to a typo, which is exactly why
    /// this warns instead of failing.
    pub unknown_keys: Vec<String>,
}

/// Merge layers in order, weakest first.
///
/// The last layer to write a key wins, and is recorded as its source.
#[must_use]
pub fn resolve(layers: &[Layer]) -> Resolved {
    let mut merged = toml::Table::new();
    let mut provenance: BTreeMap<String, Source> = BTreeMap::new();

    for layer in layers {
        merge_into(&mut merged, &layer.table, &layer.source, &mut String::new(), &mut provenance);
    }

    let unknown_keys = unknown_in(&merged);

    // A merged table that will not deserialize means a *type* error -- a string
    // where a number belongs. Falling back to defaults for the whole tree would
    // throw away every good value, so the caller gets the defaults and the error
    // separately and keeps its last-good settings.
    let settings = merged.clone().try_into::<Settings>().unwrap_or_default();

    Resolved { settings, provenance, unknown_keys }
}

/// Recursive table merge.
///
/// Tables merge key by key; anything else replaces wholesale. That distinction
/// matters for `shell.env`: a profile that sets one variable must not have to
/// restate the rest, but a profile that sets `typography.families` means *these*
/// families, not "these appended to the previous ones".
pub(crate) fn merge_into(
    dst: &mut toml::Table,
    src: &toml::Table,
    source: &Source,
    path: &mut String,
    provenance: &mut BTreeMap<String, Source>,
) {
    for (key, value) in src {
        let len = path.len();
        if !path.is_empty() {
            path.push('.');
        }
        path.push_str(key);

        match value {
            // Groups always recurse, including the first time one is seen.
            // Inserting the whole table instead would be correct in value and
            // wrong in provenance: the group would be recorded as set, and none
            // of the fields inside it would be recorded at all.
            toml::Value::Table(incoming) if is_mergeable(path.as_str()) => {
                let slot = dst
                    .entry(key.clone())
                    .or_insert_with(|| toml::Value::Table(toml::Table::new()));
                // A layer that put a non-table here loses to a real group; the
                // alternative is dropping the group entirely over a type error
                // somewhere further up the cascade.
                if !slot.is_table() {
                    *slot = toml::Value::Table(toml::Table::new());
                }
                if let Some(existing) = slot.as_table_mut() {
                    merge_into(existing, incoming, source, path, provenance);
                }
            }
            _ => {
                dst.insert(key.clone(), value.clone());
                provenance.insert(path.clone(), source.clone());
            }
        }

        path.truncate(len);
    }
}

/// Whether a table at this path merges key-by-key or replaces wholesale.
///
/// Only the settings *groups* merge. `shell.env` and `profiles` are values that
/// happen to be tables; merging into them would make a profile unable to clear
/// an inherited entry.
fn is_mergeable(path: &str) -> bool {
    matches!(
        path,
        "typography"
            | "appearance"
            | "window"
            | "tabs"
            | "shell"
            | "scrolling"
            | "cursor"
            | "motion"
    )
}

/// Keys present in the merged table that no setting corresponds to.
pub(crate) fn unknown_in(merged: &toml::Table) -> Vec<String> {
    let known: Vec<&str> = crate::invalidate::KEYS.iter().map(|(k, _)| *k).collect();
    let mut out = Vec::new();
    collect_leaves(merged, &mut String::new(), &mut out);
    out.retain(|k| !known.contains(&k.as_str()));
    out.sort();
    out
}

fn collect_leaves(table: &toml::Table, path: &mut String, out: &mut Vec<String>) {
    for (key, value) in table {
        let len = path.len();
        if !path.is_empty() {
            path.push('.');
        }
        path.push_str(key);
        match value {
            toml::Value::Table(inner) if is_mergeable(path.as_str()) => {
                collect_leaves(inner, path, out);
            }
            _ => out.push(path.clone()),
        }
        path.truncate(len);
    }
}

/// Pull a named profile out of a config table as its own layer.
///
/// Returns `None` when the profile does not exist, which the caller should treat
/// as worth warning about: silently running with no profile when one was asked
/// for is how someone ends up typing into production believing they are not.
#[must_use]
pub fn profile_layer(config: &toml::Table, name: &str) -> Option<Layer> {
    let mut table = config
        .get("profiles")?
        .as_table()?
        .get(name)?
        .as_table()?
        .clone();
    // Launcher and chrome inputs -- `command`, `icon`, `tab_color`, ... -- are
    // not settings. Left in, every profile-tab launch would spray them into
    // `unknown_keys` and the settings UI would warn about keys that are
    // perfectly legal where they were written. `profiles::ProfileMeta` is the
    // reader for these.
    for key in crate::profiles::PROFILE_ONLY_KEYS {
        table.remove(*key);
    }
    Some(Layer::new(Source::Profile(name.to_string()), table))
}

/// The `profiles.defaults` table as a layer, to sit beneath a named profile.
///
/// `defaults` is a reserved name: every profile falls through to it, so it is
/// hidden from ordinary listings ([`crate::profiles::list_profiles`]) and never
/// launched on its own.
#[must_use]
pub fn defaults_layer(config: &toml::Table) -> Option<Layer> {
    profile_layer(config, crate::profiles::RESERVED_PROFILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(source: Source, text: &str) -> Layer {
        Layer::parse(source, text).expect("valid toml")
    }

    #[test]
    fn a_later_layer_wins_and_is_recorded() {
        let r = resolve(&[
            layer(Source::User, "[typography]\nsize_pt = 14.0\n"),
            layer(Source::CommandLine, "[typography]\nsize_pt = 20.0\n"),
        ]);
        assert!((r.settings.typography.size_pt - 20.0).abs() < f32::EPSILON);
        assert_eq!(r.provenance.get("typography.size_pt"), Some(&Source::CommandLine));
    }

    #[test]
    fn groups_merge_rather_than_replace() {
        // A profile setting one field of a group must not blank the rest -- the
        // single most surprising thing a naive merge does.
        let r = resolve(&[
            layer(Source::User, "[typography]\nsize_pt = 14.0\nline_height = 1.5\n"),
            layer(Source::Profile("k8s".into()), "[typography]\nsize_pt = 18.0\n"),
        ]);
        assert!((r.settings.typography.size_pt - 18.0).abs() < f32::EPSILON);
        assert!(
            (r.settings.typography.line_height - 1.5).abs() < f32::EPSILON,
            "the profile blanked a sibling field"
        );
        assert_eq!(r.provenance.get("typography.line_height"), Some(&Source::User));
        assert_eq!(
            r.provenance.get("typography.size_pt"),
            Some(&Source::Profile("k8s".into()))
        );
    }

    #[test]
    fn a_list_replaces_rather_than_appends() {
        let r = resolve(&[
            layer(Source::User, "[typography]\nfamilies = [\"A\", \"B\"]\n"),
            layer(Source::Workspace, "[typography]\nfamilies = [\"C\"]\n"),
        ]);
        assert_eq!(r.settings.typography.families, vec!["C".to_string()]);
    }

    #[test]
    fn env_replaces_wholesale_so_a_profile_can_clear_it() {
        // `shell.env` is a value that happens to be a table. Merging into it
        // would make an inherited entry impossible to remove.
        let r = resolve(&[
            layer(Source::User, "[shell.env]\nA = \"1\"\nB = \"2\"\n"),
            layer(Source::Profile("clean".into()), "[shell.env]\nA = \"9\"\n"),
        ]);
        assert_eq!(r.settings.shell.env.get("A"), Some(&"9".to_string()));
        assert_eq!(r.settings.shell.env.get("B"), None);
    }

    #[test]
    fn an_unnamed_value_keeps_its_default_and_has_no_provenance() {
        let r = resolve(&[layer(Source::User, "[typography]\nsize_pt = 14.0\n")]);
        assert_eq!(
            r.settings.typography.line_height,
            crate::settings::Typography::default().line_height
        );
        assert!(!r.provenance.contains_key("typography.line_height"));
    }

    #[test]
    fn every_settings_group_merges_key_by_key() {
        // `is_mergeable` is the cascade's copy of the group list, beside
        // `invalidate::KEYS` and `invalidate::is_group`. A group present in
        // KEYS but missing here has its settings warned about as unknown and
        // ignored — found live the day `tabs.*` landed in the other two lists
        // but not this one. Deriving the expectation from KEYS makes the next
        // new group fail here instead of in a running window.
        let mut groups: Vec<&str> = crate::invalidate::KEYS
            .iter()
            .filter_map(|(k, _)| k.split_once('.').map(|(g, _)| g))
            .collect();
        groups.sort_unstable();
        groups.dedup();
        for g in groups {
            assert!(
                is_mergeable(g),
                "settings group `{g}` must merge key-by-key, or its keys are ignored"
            );
        }
    }

    #[test]
    fn a_tabs_setting_survives_the_cascade() {
        let r = resolve(&[layer(Source::User, "[tabs]\nposition = \"left\"\n")]);
        assert_eq!(r.settings.tabs.position, crate::settings::TabsPosition::Left);
        assert!(r.unknown_keys.is_empty(), "tabs.* are known settings");
    }

    #[test]
    fn the_user_file_is_the_weakest_thing_that_can_name_a_theme() {
        // There used to be an OS-appearance layer between `Default` and `User`
        // that forced `appearance.theme = "paper"`, and being a layer at all
        // was the bug: it sat *below* the user's file, so an explicit `theme =`
        // beat it -- which is everyone who cared enough to choose one -- and it
        // hardcoded one theme rather than reading `appearance.light_theme`.
        // Following the OS is a property of the live window, resolved by the
        // app per repaint, so nothing weaker than `User` may name a theme
        // again.
        let r = resolve(&[layer(Source::User, "[appearance]\ntheme = \"nord\"\n")]);
        assert_eq!(r.settings.appearance.theme, "nord");
        assert_eq!(
            r.provenance.get("appearance.theme"),
            Some(&Source::User),
            "the user's file owns the theme; no layer beneath it may claim the key"
        );
    }

    #[test]
    fn unknown_keys_are_collected_not_discarded() {
        let r = resolve(&[layer(Source::User, "[typography]\nsize_pt = 14.0\nsize_px = 20\n")]);
        assert_eq!(r.unknown_keys, vec!["typography.size_px".to_string()]);
        // ...and the rest of the file still applied.
        assert!((r.settings.typography.size_pt - 14.0).abs() < f32::EPSILON);
    }

    #[test]
    fn a_profile_is_extracted_by_name() {
        let table: toml::Table =
            "[profiles.k8s-prod]\n[profiles.k8s-prod.appearance]\ntheme = \"danger\"\n"
                .parse()
                .expect("valid toml");
        let l = profile_layer(&table, "k8s-prod").expect("profile exists");
        assert_eq!(l.source, Source::Profile("k8s-prod".into()));
        assert!(profile_layer(&table, "nope").is_none());
    }

    #[test]
    fn profile_only_keys_are_stripped_from_the_layer() {
        // `command = "wsl.exe"` is a launch input, not a setting. Left in the
        // layer it reaches the merged table as a root key the schema has never
        // heard of, and every profile-tab launch warns about its own profile.
        let table: toml::Table = "[profiles.wsl]\ncommand = \"wsl.exe\"\nicon = \"tux\"\n\
                                  tab_color = 3\n[profiles.wsl.typography]\nsize_pt = 18.0\n"
            .parse()
            .expect("valid toml");
        let l = profile_layer(&table, "wsl").expect("profile exists");
        assert!(!l.table.contains_key("command"), "launch input leaked into the cascade");
        assert!(!l.table.contains_key("icon"), "chrome input leaked into the cascade");

        let r = resolve(&[l]);
        assert!(
            r.unknown_keys.is_empty(),
            "profile-only keys sprayed unknown_keys: {:?}",
            r.unknown_keys
        );
        assert!(
            (r.settings.typography.size_pt - 18.0).abs() < f32::EPSILON,
            "the settings half of the profile must still apply"
        );
    }

    #[test]
    fn the_defaults_layer_sits_between_user_and_the_named_profile() {
        // The contract the profiles editor renders: user < profiles.defaults <
        // profiles.<name> < workspace. Each step must beat the one below it,
        // or an "overrides Defaults" chip would lie.
        let config: toml::Table = "\
            [profiles.defaults.typography]\nsize_pt = 12.0\nline_height = 1.9\n\
            [profiles.k8s.typography]\nsize_pt = 14.0\n"
            .parse()
            .expect("valid toml");
        let r = resolve(&[
            layer(Source::User, "[typography]\nsize_pt = 10.0\nline_height = 1.1\ncell_width = 0.7\n"),
            defaults_layer(&config).expect("defaults exists"),
            profile_layer(&config, "k8s").expect("profile exists"),
            layer(Source::Workspace, "[typography]\ncell_width = 0.9\n"),
        ]);
        assert!(
            (r.settings.typography.size_pt - 14.0).abs() < f32::EPSILON,
            "the named profile must beat profiles.defaults"
        );
        assert!(
            (r.settings.typography.line_height - 1.9).abs() < f32::EPSILON,
            "profiles.defaults must beat the user file"
        );
        assert!(
            (r.settings.typography.cell_width - 0.9).abs() < f32::EPSILON,
            "the workspace file must beat the named profile"
        );
        assert_eq!(
            r.provenance.get("typography.line_height"),
            Some(&Source::Profile("defaults".into())),
            "provenance must name the defaults layer, so the UI can say so"
        );
    }

    #[test]
    fn provenance_reads_as_a_sentence() {
        assert_eq!(Source::Profile("k8s-prod".into()).to_string(), "profile `k8s-prod`");
        assert_eq!(Source::CommandLine.to_string(), "command line");
    }
}

//! Profiles as launch targets: what to run, which machine runs it, how it looks.
//!
//! A profile's table holds two kinds of key. The *settings* keys are the
//! settings tree again, partially specified, and flow through the cascade
//! ([`crate::cascade::profile_layer`]). The *profile-only* keys — command,
//! host, icon, tab colour — are launcher and chrome inputs that no settings
//! struct wants; they are parsed here into [`ProfileMeta`], leniently, because
//! a typo in a profile must never take the terminal down (the never-crash
//! rule, [`crate`] docs).
//!
//! Outside the `fs` feature on purpose, like [`crate::ui`]: the web client's
//! profiles editor renders from the same resolution.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use crate::cascade::{self, Source};
use crate::ui::{self, UiField, UiVariant, Widget};

/// The reserved profile every other profile falls through to.
///
/// Hidden from ordinary listings and never launched on its own; the profiles
/// editor shows it as the parent, not as a sibling.
pub const RESERVED_PROFILE: &str = "defaults";

/// Keys that belong to the profile itself, not to the settings cascade.
///
/// [`crate::cascade::profile_layer`] strips these from the layer it returns:
/// they are launcher/chrome inputs, and left in they would land in
/// `unknown_keys` on every profile-tab launch.
pub const PROFILE_ONLY_KEYS: &[&str] = &[
    "command",
    "host",
    "ask_host",
    "starting_directory",
    "tab_title",
    "color_scheme",
    "tab_color",
    "icon",
    "color_from",
];

/// The settings keys the profiles *editor* offers as per-profile overrides.
///
/// This scopes the UI, not the cascade: `--profile windows` can still set any
/// settings key (the `k8s-prod` red-window use case sets `appearance.theme`),
/// and the editor's "Edit as TOML" path keeps that door open. The allowlist
/// only decides which rows the generated form renders.
pub const PROFILE_SETTINGS_KEYS: &[&str] = &[
    "typography.families",
    "typography.size_pt",
    "window.opacity",
    "window.backdrop",
    "cursor.shape",
    "cursor.blink",
];

/// Where a tab running this profile gets its title.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum TabTitle {
    /// Whatever the shell reports (OSC 0/2). The default.
    #[default]
    FromShell,
    /// The profile's own name, fixed.
    ProfileName,
    /// A literal title.
    Custom(String),
}

/// Whether a tab's accent colour comes from the profile or from the host it
/// runs on. Set on Defaults, the whole fleet reads by machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ColorFrom {
    Profile,
    Host,
}

/// The profile-only half of a profile's table.
///
/// Parsed leniently: a wrong-typed value warns and falls back to its default,
/// never fails — a profile with `tab_color = "red"` still launches.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct ProfileMeta {
    /// Command line to run. `None` falls through to `shell.command`.
    pub command: Option<String>,
    /// The machine this profile is pinned to. `None` means the local one.
    pub host: Option<String>,
    /// Ask which host at launch, for host-agnostic profiles.
    pub ask_host: bool,
    /// Working directory — possibly a path this machine has never heard of.
    pub starting_directory: Option<String>,
    pub tab_title: TabTitle,
    /// Colour scheme id — the ANSI half of a theme, applied to the grid only.
    pub color_scheme: Option<String>,
    /// Index into the theme's accents for the tab's rule and glyph tile.
    pub tab_color: Option<u8>,
    /// Glyph for the tab's icon tile.
    pub icon: Option<String>,
    pub color_from: Option<ColorFrom>,
}

/// Which layer a resolved profile value came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProfileProvenance {
    /// Set on the profile itself, shadowing whatever Defaults says.
    OverridesDefaults,
    /// Absent on the profile; the value fell through to `profiles.defaults`.
    InheritedFromDefaults,
    /// Set nowhere; the value is the compiled default.
    Unset,
}

/// A profile after falling through Defaults: settings overrides, launch/chrome
/// metadata, and where each value came from.
#[derive(Debug, Clone)]
pub struct ProfileResolved {
    /// The merged settings overrides — defaults then the named profile,
    /// with the cascade's own semantics (groups merge key-by-key,
    /// value-tables like `shell.env` replace wholesale).
    pub overrides: toml::Table,
    /// The profile-only keys, with per-key name-then-defaults fallback.
    pub meta: ProfileMeta,
    /// Dotted settings keys and bare profile-only keys, each mapped to where
    /// its value came from. Keys set nowhere are absent — see
    /// [`ProfileResolved::provenance_of`].
    pub provenance: BTreeMap<String, ProfileProvenance>,
    /// Keys in the merged overrides that are neither profile-only nor schema
    /// keys — the typo surface the editor warns about.
    pub unknown_keys: Vec<String>,
}

impl ProfileResolved {
    /// Provenance for one key, `Unset` when nothing wrote it.
    #[must_use]
    pub fn provenance_of(&self, key: &str) -> ProfileProvenance {
        self.provenance.get(key).copied().unwrap_or(ProfileProvenance::Unset)
    }
}

/// The ordinary profiles in a config, `defaults` excluded.
///
/// The launcher menu and the profile rail both read this; `defaults` appears
/// in neither as a sibling, because launching it is meaningless.
#[must_use]
pub fn list_profiles(config: &toml::Table) -> Vec<String> {
    config
        .get("profiles")
        .and_then(toml::Value::as_table)
        .map(|profiles| {
            profiles.keys().filter(|name| *name != RESERVED_PROFILE).cloned().collect()
        })
        .unwrap_or_default()
}

/// Keys in one profile's table that are neither profile-only nor settings.
///
/// The same typo surface as [`crate::Resolved::unknown_keys`], scoped to a
/// profile so the warning can name it.
#[must_use]
pub fn unknown_profile_keys(profile: &toml::Table) -> Vec<String> {
    let mut table = profile.clone();
    for key in PROFILE_ONLY_KEYS {
        table.remove(*key);
    }
    cascade::unknown_in(&table)
}

/// Resolve one profile through Defaults. Pure — no filesystem.
///
/// A name that does not exist resolves as an empty profile over Defaults
/// rather than failing: the caller warned at selection time
/// ([`crate::load`]), and the editor still has something to render.
#[must_use]
pub fn resolve_profile(config: &toml::Table, name: &str) -> ProfileResolved {
    let empty = toml::Table::new();
    let profiles = config.get("profiles").and_then(toml::Value::as_table);
    let named = profiles
        .and_then(|p| p.get(name))
        .and_then(toml::Value::as_table)
        .unwrap_or(&empty);
    // Resolving `defaults` itself must not fall through to itself, or every
    // one of its own keys would read as an override of nothing.
    let defaults = if name == RESERVED_PROFILE {
        &empty
    } else {
        profiles
            .and_then(|p| p.get(RESERVED_PROFILE))
            .and_then(toml::Value::as_table)
            .unwrap_or(&empty)
    };

    let strip = |table: &toml::Table| {
        let mut table = table.clone();
        for key in PROFILE_ONLY_KEYS {
            table.remove(*key);
        }
        table
    };

    // The cascade's own merge, so group-vs-value semantics cannot drift from
    // what `--profile` does at launch. Its provenance map, keyed by which
    // Source last wrote each dotted key, is exactly the override/inherit
    // distinction the editor's chips need.
    let mut overrides = toml::Table::new();
    let mut sources: BTreeMap<String, Source> = BTreeMap::new();
    let defaults_source = Source::Profile(RESERVED_PROFILE.to_string());
    cascade::merge_into(
        &mut overrides,
        &strip(defaults),
        &defaults_source,
        &mut String::new(),
        &mut sources,
    );
    cascade::merge_into(
        &mut overrides,
        &strip(named),
        &Source::Profile(name.to_string()),
        &mut String::new(),
        &mut sources,
    );

    let mut provenance: BTreeMap<String, ProfileProvenance> = sources
        .into_iter()
        .map(|(key, source)| {
            let p = if source == defaults_source && name != RESERVED_PROFILE {
                ProfileProvenance::InheritedFromDefaults
            } else {
                ProfileProvenance::OverridesDefaults
            };
            (key, p)
        })
        .collect();

    let (named_meta, named_set) = meta_with_presence(named);
    let (defaults_meta, defaults_set) = meta_with_presence(defaults);
    let meta = fold_meta(named_meta, &named_set, defaults_meta, &defaults_set);
    for key in PROFILE_ONLY_KEYS {
        if named_set.contains(key) {
            provenance.insert((*key).to_string(), ProfileProvenance::OverridesDefaults);
        } else if defaults_set.contains(key) {
            provenance.insert((*key).to_string(), ProfileProvenance::InheritedFromDefaults);
        }
    }

    let unknown_keys = cascade::unknown_in(&overrides);

    ProfileResolved { overrides, meta, provenance, unknown_keys }
}

/// Per-key fallback: the named profile's value where it set one, Defaults'
/// otherwise. Field-level, not struct-level — a profile that sets only `icon`
/// must still inherit Defaults' `color_from`.
fn fold_meta(
    named: ProfileMeta,
    named_set: &BTreeSet<&'static str>,
    defaults: ProfileMeta,
    defaults_set: &BTreeSet<&'static str>,
) -> ProfileMeta {
    let pick = |key: &'static str| named_set.contains(key) || !defaults_set.contains(key);
    ProfileMeta {
        command: if pick("command") { named.command } else { defaults.command },
        host: if pick("host") { named.host } else { defaults.host },
        ask_host: if pick("ask_host") { named.ask_host } else { defaults.ask_host },
        starting_directory: if pick("starting_directory") {
            named.starting_directory
        } else {
            defaults.starting_directory
        },
        tab_title: if pick("tab_title") { named.tab_title } else { defaults.tab_title },
        color_scheme: if pick("color_scheme") { named.color_scheme } else { defaults.color_scheme },
        tab_color: if pick("tab_color") { named.tab_color } else { defaults.tab_color },
        icon: if pick("icon") { named.icon } else { defaults.icon },
        color_from: if pick("color_from") { named.color_from } else { defaults.color_from },
    }
}

impl ProfileMeta {
    /// Parse the profile-only keys out of a profile's table, leniently.
    #[must_use]
    pub fn from_table(table: &toml::Table) -> Self {
        meta_with_presence(table).0
    }
}

/// Parse a table's profile-only keys, and record which of them were present
/// *and valid* — a wrong-typed key warns and counts as absent, so resolution
/// can still fall through to Defaults for it.
fn meta_with_presence(table: &toml::Table) -> (ProfileMeta, BTreeSet<&'static str>) {
    let mut set = BTreeSet::new();
    let mut meta = ProfileMeta::default();

    let mut track = |key: &'static str, present: bool| {
        if present {
            set.insert(key);
        }
    };

    meta.command = str_key(table, "command");
    track("command", meta.command.is_some());
    meta.host = str_key(table, "host");
    track("host", meta.host.is_some());
    if let Some(v) = bool_key(table, "ask_host") {
        meta.ask_host = v;
        track("ask_host", true);
    }
    meta.starting_directory = str_key(table, "starting_directory");
    track("starting_directory", meta.starting_directory.is_some());
    if let Some(v) = str_key(table, "tab_title") {
        // The two fixed spellings are the segmented control's values; any
        // other string is a custom title, which makes a title literally
        // spelled "from-shell" inexpressible — accepted, and cheaper than a
        // second key for the common case.
        meta.tab_title = match v.as_str() {
            "from-shell" => TabTitle::FromShell,
            "profile-name" => TabTitle::ProfileName,
            _ => TabTitle::Custom(v),
        };
        track("tab_title", true);
    }
    meta.color_scheme = str_key(table, "color_scheme");
    track("color_scheme", meta.color_scheme.is_some());
    if let Some(v) = int_key(table, "tab_color") {
        match u8::try_from(v) {
            Ok(v) => {
                meta.tab_color = Some(v);
                track("tab_color", true);
            }
            Err(_) => {
                tracing::warn!(key = "tab_color", value = v, "out of range for a profile accent; ignoring");
            }
        }
    }
    meta.icon = str_key(table, "icon");
    track("icon", meta.icon.is_some());
    if let Some(v) = str_key(table, "color_from") {
        match v.as_str() {
            "profile" => {
                meta.color_from = Some(ColorFrom::Profile);
                track("color_from", true);
            }
            "host" => {
                meta.color_from = Some(ColorFrom::Host);
                track("color_from", true);
            }
            _ => {
                tracing::warn!(key = "color_from", value = %v, "expected `profile` or `host`; ignoring");
            }
        }
    }

    (meta, set)
}

fn str_key(table: &toml::Table, key: &str) -> Option<String> {
    match table.get(key)? {
        // An empty string is the file's spelling of "unset": treating it as
        // present would mark provenance OverridesDefaults for a key carrying
        // nothing, and falling back to Defaults would then require a delete
        // rather than clearing the field.
        toml::Value::String(s) if s.is_empty() => None,
        toml::Value::String(s) => Some(s.clone()),
        other => {
            tracing::warn!(key, found = other.type_str(), "wrong type for profile key; ignoring");
            None
        }
    }
}

fn bool_key(table: &toml::Table, key: &str) -> Option<bool> {
    match table.get(key)? {
        toml::Value::Boolean(b) => Some(*b),
        other => {
            tracing::warn!(key, found = other.type_str(), "wrong type for profile key; ignoring");
            None
        }
    }
}

fn int_key(table: &toml::Table, key: &str) -> Option<i64> {
    match table.get(key)? {
        toml::Value::Integer(i) => Some(*i),
        other => {
            tracing::warn!(key, found = other.type_str(), "wrong type for profile key; ignoring");
            None
        }
    }
}

/// Every field the profiles editor renders, launch/identity first.
///
/// The launch and identity fields are hand-authored — they are not settings,
/// so no schema walk can produce them. The allowlisted settings rows are
/// cloned from [`ui::fields`] so their ranges, docs and widgets stay
/// single-sourced in the schema.
#[must_use]
pub fn fields() -> Vec<UiField> {
    let text = |key: &str, group: &str, description: &str| UiField {
        key: key.to_string(),
        group: group.to_string(),
        widget: Widget::Text,
        description: description.to_string(),
        range: None,
        integer: false,
        variants: Vec::new(),
        default: serde_json::Value::String(String::new()),
        restart_hint: false,
    };

    let mut out = vec![
        text(
            "command",
            "Launch",
            "Command line to run. Empty falls through to the shell setting; a WSL \
             invocation is long, so the editor renders this as a wrapping text area.",
        ),
        UiField {
            widget: Widget::HostPicker,
            description: "The machine this profile is pinned to. Empty means the local \
                          machine; the roster comes from the fleet, not the schema."
                .to_string(),
            ..text("host", "Launch", "")
        },
        UiField {
            widget: Widget::Toggle,
            description: "Ask which host at launch, for a profile that is not pinned to \
                          one machine."
                .to_string(),
            default: serde_json::Value::Bool(false),
            ..text("ask_host", "Launch", "")
        },
        UiField {
            widget: Widget::Path,
            description: "Working directory for the session. May be a path this machine \
                          has never heard of — it is resolved on the host that runs the \
                          profile."
                .to_string(),
            ..text("starting_directory", "Launch", "")
        },
        UiField {
            widget: Widget::Select,
            // Only the two fixed spellings are variants: a "custom" variant
            // would be written back verbatim by any client that round-trips
            // the selected value, setting the title to the literal string
            // "custom". The custom segment is the editor's affordance — it
            // writes the user's own text, which is any other string.
            description: "Where the tab's title comes from: the shell, the profile's \
                          name, or any other string used verbatim as a custom title."
                .to_string(),
            variants: vec![
                UiVariant {
                    value: "from-shell".to_string(),
                    description: "Whatever the shell reports.".to_string(),
                },
                UiVariant {
                    value: "profile-name".to_string(),
                    description: "The profile's own name, fixed.".to_string(),
                },
            ],
            default: serde_json::Value::String("from-shell".to_string()),
            ..text("tab_title", "Launch", "")
        },
        UiField {
            widget: Widget::SchemePicker,
            description: "Colour scheme for this profile's grid — the ANSI half of a \
                          theme. The window's chrome keeps its own theme; only the grid \
                          follows this."
                .to_string(),
            ..text("color_scheme", "Appearance", "")
        },
        UiField {
            widget: Widget::AccentPicker,
            description: "The tab's accent — the 2px top rule and the icon tile's \
                          colour — as an index into the theme's accents."
                .to_string(),
            integer: true,
            default: serde_json::Value::Null,
            ..text("tab_color", "Appearance", "")
        },
        UiField {
            widget: Widget::IconPicker,
            description: "Glyph for the tab's icon tile.".to_string(),
            ..text("icon", "Appearance", "")
        },
        UiField {
            widget: Widget::Select,
            description: "Whether the tab's accent comes from this profile or from the \
                          host it runs on. Set on Defaults, the whole fleet reads by \
                          machine."
                .to_string(),
            variants: vec![
                UiVariant {
                    value: "profile".to_string(),
                    description: "The profile's own accent, wherever it runs.".to_string(),
                },
                UiVariant {
                    value: "host".to_string(),
                    description: "The colour of the machine the session runs on.".to_string(),
                },
            ],
            default: serde_json::Value::String("profile".to_string()),
            ..text("color_from", "Appearance", "")
        },
    ];

    let root = ui::fields();
    for key in PROFILE_SETTINGS_KEYS {
        if let Some(field) = root.iter().find(|f| f.key == *key) {
            out.push(field.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> toml::Table {
        text.parse().expect("valid toml")
    }

    #[test]
    fn defaults_is_reserved_and_never_listed() {
        // The launcher launches profiles; launching `defaults` is meaningless,
        // and a rail that listed it as a sibling would invite deleting the
        // parent every other profile falls through to.
        let c = config(
            "[profiles.defaults]\nicon = \"star\"\n[profiles.ubuntu]\n[profiles.k8s-prod]\n",
        );
        assert_eq!(list_profiles(&c), vec!["k8s-prod".to_string(), "ubuntu".to_string()]);
    }

    #[test]
    fn meta_parses_leniently_and_never_fails() {
        // The never-crash rule: `tab_color = "red"` is a wrong type, and the
        // profile must still launch with that one key ignored.
        let t = config(
            "command = \"wsl.exe\"\ntab_color = \"red\"\nask_host = 3\nicon = \"tux\"\n",
        );
        let meta = ProfileMeta::from_table(&t);
        assert_eq!(meta.command.as_deref(), Some("wsl.exe"));
        assert_eq!(meta.icon.as_deref(), Some("tux"));
        assert_eq!(meta.tab_color, None, "a wrong-typed value falls back, never fails");
        assert!(!meta.ask_host, "a wrong-typed bool falls back to its default");
    }

    #[test]
    fn tab_title_reads_the_two_fixed_spellings_and_keeps_the_rest_verbatim() {
        let named = |s: &str| {
            ProfileMeta::from_table(&config(&format!("tab_title = \"{s}\"\n"))).tab_title
        };
        assert_eq!(named("from-shell"), TabTitle::FromShell);
        assert_eq!(named("profile-name"), TabTitle::ProfileName);
        assert_eq!(
            named("prod cluster"),
            TabTitle::Custom("prod cluster".to_string()),
            "any other string is a custom title"
        );
        assert_eq!(
            ProfileMeta::from_table(&toml::Table::new()).tab_title,
            TabTitle::FromShell,
            "the shell's title is the default"
        );
    }

    #[test]
    fn an_empty_string_is_unset_not_an_override() {
        // `command = ""` must fall through to Defaults exactly like an absent
        // key: clearing a field in the editor writes an empty string, and a
        // chip claiming OverridesDefaults over nothing would force the user
        // to hand-delete the key to get inheritance back.
        let mut root = toml::Table::new();
        let profiles: toml::Table = toml::from_str(
            "defaults = { command = \"zsh -l\" }\nx = { command = \"\" }\n",
        )
        .expect("literal profiles table parses");
        root.insert("profiles".into(), toml::Value::Table(profiles));
        let r = resolve_profile(&root, "x");
        assert_eq!(
            r.meta.command.as_deref(),
            Some("zsh -l"),
            "the empty override falls through to Defaults"
        );
        assert_eq!(
            r.provenance_of("command"),
            ProfileProvenance::InheritedFromDefaults,
            "and the chip says inherited, not overridden"
        );
    }

    #[test]
    fn resolve_profile_reports_override_inherit_and_unset() {
        // The three chips the editor renders. Getting these wrong makes the
        // inheritance UI lie, which is worse than not having one.
        let c = config(
            "[profiles.defaults]\nicon = \"circle\"\ncolor_from = \"host\"\n\
             [profiles.defaults.typography]\nsize_pt = 13.0\n\
             [profiles.ubuntu]\nicon = \"tux\"\ncommand = \"wsl.exe\"\n\
             [profiles.ubuntu.window]\nopacity = 0.9\n",
        );
        let r = resolve_profile(&c, "ubuntu");

        assert_eq!(r.provenance_of("icon"), ProfileProvenance::OverridesDefaults);
        assert_eq!(r.provenance_of("color_from"), ProfileProvenance::InheritedFromDefaults);
        assert_eq!(r.provenance_of("tab_color"), ProfileProvenance::Unset);
        assert_eq!(r.provenance_of("window.opacity"), ProfileProvenance::OverridesDefaults);
        assert_eq!(
            r.provenance_of("typography.size_pt"),
            ProfileProvenance::InheritedFromDefaults
        );
        assert_eq!(r.provenance_of("cursor.blink"), ProfileProvenance::Unset);

        // And the resolved values match the chips.
        assert_eq!(r.meta.icon.as_deref(), Some("tux"));
        assert_eq!(r.meta.color_from, Some(ColorFrom::Host), "fell through to Defaults");
        assert_eq!(r.meta.command.as_deref(), Some("wsl.exe"));
        assert_eq!(
            r.overrides["typography"]["size_pt"].as_float(),
            Some(13.0),
            "the merged overrides carry Defaults' settings too"
        );
    }

    #[test]
    fn shell_env_replaces_wholesale_through_defaults() {
        // Same rule as the cascade: `shell.env` is a value that happens to be
        // a table, so a profile can *clear* an inherited variable. Merging
        // key-by-key here while the launch cascade replaces wholesale would
        // make the editor preview a different environment than the session gets.
        let c = config(
            "[profiles.defaults.shell.env]\nA = \"1\"\nB = \"2\"\n\
             [profiles.clean.shell.env]\nC = \"3\"\n",
        );
        let r = resolve_profile(&c, "clean");
        let env = r.overrides["shell"]["env"].as_table().expect("table");
        assert_eq!(env.get("C").and_then(toml::Value::as_str), Some("3"));
        assert!(
            !env.contains_key("A") && !env.contains_key("B"),
            "Defaults' env leaked through a wholesale-replace value: {env:?}"
        );
        assert_eq!(r.provenance_of("shell.env"), ProfileProvenance::OverridesDefaults);
    }

    #[test]
    fn resolving_defaults_itself_does_not_inherit_from_itself() {
        let c = config("[profiles.defaults]\nicon = \"star\"\n");
        let r = resolve_profile(&c, "defaults");
        assert_eq!(
            r.provenance_of("icon"),
            ProfileProvenance::OverridesDefaults,
            "Defaults' own keys are its own, not inherited from a phantom parent"
        );
    }

    #[test]
    fn the_allowlist_names_real_schema_keys() {
        // Anti-rot: a renamed setting must fail here, not silently drop a row
        // from the profiles editor.
        let schema_keys = crate::schema::keys();
        for key in PROFILE_SETTINGS_KEYS {
            assert!(
                schema_keys.iter().any(|k| k == key),
                "PROFILE_SETTINGS_KEYS names '{key}', which is not a schema key — the list rotted"
            );
        }
    }

    #[test]
    fn the_allowlist_scopes_the_editor_not_the_cascade() {
        // Today's contract: `--profile windows` can set ANY settings key.
        // `appearance.theme` is not in the allowlist, and the k8s-prod
        // red-window use case depends on it applying anyway.
        assert!(
            !PROFILE_SETTINGS_KEYS.contains(&"appearance.theme"),
            "if the allowlist grows appearance.theme, pick a different off-list key here"
        );
        let c = config("[profiles.k8s-prod.appearance]\ntheme = \"danger\"\n");
        let layer = cascade::profile_layer(&c, "k8s-prod").expect("profile exists");
        let r = cascade::resolve(&[layer]);
        assert_eq!(
            r.settings.appearance.theme, "danger",
            "a profile must be able to set settings the editor does not offer"
        );
        assert!(r.unknown_keys.is_empty(), "and it is a known key, not a tolerated typo");
    }

    #[test]
    fn every_profile_field_arrives_renderable() {
        // Same property ui::fields() pins for the settings form: a field
        // without a description ships to three clients as a bare key.
        for field in fields() {
            assert!(!field.group.is_empty(), "'{}' has no group", field.key);
            assert!(!field.description.is_empty(), "'{}' has no description", field.key);
        }
    }

    #[test]
    fn allowlisted_rows_are_clones_of_the_schema_walk() {
        // Ranges, docs and widgets must stay single-sourced: a hand-copied
        // size_pt row would stop moving when the schema's range does.
        let all = fields();
        let root = ui::fields();
        for key in PROFILE_SETTINGS_KEYS {
            let ours = all.iter().find(|f| f.key == *key).expect("allowlisted field present");
            let theirs = root.iter().find(|f| f.key == *key).expect("schema field exists");
            assert_eq!(ours, theirs, "'{key}' drifted from the schema walk");
        }
    }

    #[test]
    fn unknown_profile_keys_are_the_typo_surface() {
        let t = config("command = \"zsh\"\nicon = \"star\"\n[typography]\nsize_px = 14\n");
        assert_eq!(
            unknown_profile_keys(&t),
            vec!["typography.size_px".to_string()],
            "profile-only keys are legal; the typo is what surfaces"
        );
    }
}

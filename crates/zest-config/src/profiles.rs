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
    "env",
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
    "window.background_image",
    "window.background_fit",
    "window.background_dim",
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
    /// Environment for the shell this profile launches, layered over the
    /// host's own `shell.env`; last-wins, and **an empty value unsets**.
    ///
    /// Profile-only rather than the `shell.env` settings key, which it looks
    /// like and is not. Three reasons, and each one is a bug avoided:
    ///
    /// - The cascade treats `shell.env` as a value that happens to be a table,
    ///   so it **replaces wholesale** ([`cascade::merge_into`]). A profile
    ///   setting one variable would silently drop the user's entire global env.
    /// - A profile's settings layer is process-wide (`--profile`), not per tab.
    ///   Applying it per launch means threading the whole cascade through every
    ///   one.
    /// - [`ProfileMeta::starting_directory`] is already exactly this: a
    ///   profile-only key beside a settings key (`shell.cwd`) that means the
    ///   same thing at a different scope.
    ///
    /// Values may carry [`expand`]'s placeholders, left unexpanded here: they
    /// resolve on the machine that *runs* the profile, which is what lets one
    /// profile mean the same thing on every machine in the fleet.
    ///
    /// To drop a variable `profiles.defaults.env` sets, give it an empty value
    /// rather than looking for a way to clear the table: empty means unset all
    /// the way down to the child, and it is per-variable rather than
    /// all-or-nothing.
    pub env: BTreeMap<String, String>,
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

/// [`Settings::profiles`](crate::Settings::profiles) re-rooted as the
/// `toml::Table` every function here walks.
///
/// The resolver takes a whole config rather than a profiles map, because a
/// profile's settings keys have to fall through the same cascade the root
/// does. Callers holding a parsed `Settings` therefore have to put the map
/// back under a `profiles` key first — one small encoding, which lived in
/// `zest-app`'s launcher until the daemon needed it too (#262). Two copies of
/// it would be two places for the key name to be spelled.
#[must_use]
pub fn root_of(settings: &crate::Settings) -> toml::Table {
    let mut table = toml::Table::new();
    for (key, profile) in &settings.profiles {
        table.insert(key.clone(), toml::Value::Table(profile.clone()));
    }
    let mut root = toml::Table::new();
    root.insert("profiles".into(), toml::Value::Table(table));
    root
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
        // The one field that merges instead of picking, and deliberately so.
        // Every other key is a single value where "the profile said something"
        // must shadow Defaults entirely. `env` is a *set* of variables, and
        // `profiles.defaults.env` carrying fleet-wide entries while one profile
        // adds its own is the obvious use -- a field-level pick would throw the
        // fleet-wide ones away the moment a profile named a single variable.
        //
        // Note this is the opposite of what the settings cascade does with
        // `shell.env`, which replaces wholesale. That difference is the whole
        // argument for `env` being a profile-only key rather than a reuse of
        // that one; `a_profiles_env_merges_over_defaults_key_by_key` pins it.
        env: {
            let mut env = defaults.env;
            env.extend(named.env);
            env
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
    if let Some(v) = env_key(table, "env") {
        // Presence here is only provenance -- `fold_meta` merges `env`
        // whichever way this lands, so an empty `[profiles.x.env]` is a no-op
        // rather than a way to clear Defaults.
        //
        // That is deliberate, and the alternative was considered and rejected:
        // "an empty table clears everything, one entry merges" is a rule that
        // surprises exactly when someone deletes their last variable. Clearing
        // one inherited entry is `NAME = ""`, which the empty-value-unsets
        // convention already spells and which reaches the child as a genuinely
        // absent variable. Per-variable beats all-or-nothing, and it is one
        // rule rather than two.
        meta.env = v;
        track("env", true);
    }
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

/// Where a profile's placeholders resolve to, on the machine running it.
///
/// Built by the *host*, never by the client: a launch carries the profile's
/// name and its unexpanded values, and the daemon that spawns the shell fills
/// these in. That is what makes one profile mean the same thing on every
/// machine — `${profile_dir}` on a Mac is a Mac path, and the same profile
/// launched on a Linux box gets that box's own. Resolving client-side would
/// hand a Linux daemon `/Users/...` and call it configuration.
///
/// The same rule ADR-014 already applies to `starting_directory`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExpandContext {
    /// The profile's name. Empty outside a profile launch, which makes
    /// `${profile}` and `${profile_dir}` unresolvable rather than wrong.
    pub profile: String,
    /// This machine's config directory — the parent of `${profile_dir}`.
    pub config_dir: Option<std::path::PathBuf>,
    /// This machine's home directory.
    pub home: Option<std::path::PathBuf>,
    /// The environment the child will actually have when it starts — what
    /// `${env:NAME}` reads.
    ///
    /// Supplied by the caller rather than read from `std::env` here, because
    /// those are different questions: `terminal_env` clears another terminal's
    /// stale identity and shell integration injects `ZDOTDIR`, so this
    /// process's environment both names variables the child will not have and
    /// misses ones it will. `CommandSpec::effective_env` is the producer, and
    /// it applies the same empty-means-unset rule both pty backends do.
    ///
    /// A consequence worth stating: the map is taken *before* this profile's
    /// own entries are layered, which is what makes "never a sibling entry"
    /// true by construction rather than by rule.
    pub env: std::collections::BTreeMap<String, String>,
}

impl ExpandContext {
    /// The directory `${profile_dir}` names: `<config>/profiles/<name>`.
    ///
    /// `None` when there is no profile or no config directory to hang it off.
    /// Not created here — see [`expand`]: a profiles editor rendering a row
    /// must not mint directories on disk.
    #[must_use]
    pub fn profile_dir(&self) -> Option<std::path::PathBuf> {
        if self.profile.is_empty() {
            return None;
        }
        // The profile name has to be a single path segment: this is the one
        // place it becomes a *filesystem* path, and a name carrying a
        // separator or climbing with `..` would point somewhere else entirely.
        //
        // `profiles_ui::rename_error` rejects such names at the door, so this
        // is the second lock rather than the only one — but it is a config
        // file, and a hand-written `[profiles."../x"]` never passes the
        // editor. Refusing here means `${profile_dir}` stays unexpanded and
        // says so, which is the never-crash rule's answer.
        //
        // `.` and `..` are checked as whole segments, not as a substring: a
        // profile legitimately called `node..old` contains `..` and escapes
        // nothing.
        let unsafe_segment = self
            .profile
            .split(['/', '\\'])
            .any(|part| part == "." || part == "..");
        if self.profile.contains(['/', '\\']) || unsafe_segment {
            tracing::warn!(
                profile = %self.profile,
                "a profile name that is not a single path segment has no directory of its own"
            );
            return None;
        }
        Some(self.config_dir.as_ref()?.join("profiles").join(&self.profile))
    }
}

/// Substitute a profile environment value's placeholders.
///
/// A **closed** set, and nothing else is touched:
///
/// | Token | Expands to |
/// |---|---|
/// | `${profile_dir}` | `<config dir>/profiles/<name>` |
/// | `${profile}` | the profile's name |
/// | `${home}` | the host's home directory |
/// | `${env:NAME}` | the child's inherited `NAME` |
///
/// `$FOO` is deliberately **not** expanded: these values go into the child's
/// environment block directly, without a shell, so implying shell expansion
/// would promise something nothing here delivers. `${env:…}` is the explicit
/// way to say it, spelled as VS Code spells it for the same job — and
/// namespaced on purpose, so no environment variable can ever collide with a
/// placeholder of ours. (kitty's bare `${VAR}` has no such problem only
/// because kitty has no placeholders of its own.)
///
/// **`${home}` is not a synonym for `${env:HOME}`.** `HOME` is `USERPROFILE`
/// on Windows, and a profile crosses the fleet: one written on a Mac may be
/// launched on a Windows box, where the platform-neutral spelling is the only
/// one that resolves.
///
/// **`${env:…}` sees the environment the child inherits, never a sibling entry
/// in the same table.** kitty resolves earlier `env` lines because its config
/// is line-ordered; a profile's env is a `BTreeMap`, so "earlier" would mean
/// *alphabetically* earlier — `A = "${env:B}"` resolving differently from
/// `Z = "${env:B}"` for a reason no one could see in the file. One pass, one
/// source.
///
/// An unresolvable or unknown `${token}` is left **verbatim** and warned about
/// rather than replaced with an empty string. Silently emptying it would turn
/// `CLAUDE_CONFIG_DIR = "${profile_dir}/claude"` into `/claude` — an absolute
/// path at the filesystem root, which a tool would then create or fail on, and
/// either way the cause would be nowhere near the symptom.
#[must_use]
pub fn expand(value: &str, ctx: &ExpandContext) -> String {
    // Only worth the work when there is a token at all, which is the common
    // case for an ordinary variable.
    if !value.contains("${") {
        return value.to_string();
    }
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find('}') else {
            // An unterminated `${` is text, not a token. Nothing after it can
            // be a placeholder either, so this ends the scan.
            out.push_str(&rest[start..]);
            return out;
        };
        let token = &after[..end];
        let resolved = match token {
            "profile" => (!ctx.profile.is_empty()).then(|| ctx.profile.clone()),
            "profile_dir" => ctx.profile_dir().map(|p| p.display().to_string()),
            "home" => ctx.home.as_ref().map(|p| p.display().to_string()),
            // `ctx.env`, never `std::env`: the child's environment is this
            // process's *plus* what the spec has layered — stale-identity
            // markers cleared, `ZDOTDIR` injected — so reading the process
            // directly would answer for variables the child will not have.
            // An empty name (`${env:}`) resolves to nothing and is left as
            // written, like any other token we cannot answer.
            _ => token
                .strip_prefix("env:")
                .filter(|name| !name.is_empty())
                .and_then(|name| ctx.env.get(name).cloned()),
        };
        match resolved {
            Some(text) => out.push_str(&text),
            None => {
                tracing::warn!(
                    token,
                    "no value for this placeholder here; leaving it as written"
                );
                out.push_str(&rest[start..start + 2 + end + 1]);
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
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

/// A table of string values, skipping any entry that is not one.
///
/// Per-entry leniency rather than per-table: a profile with one mistyped
/// variable still launches with the rest of its environment, which is the
/// never-crash rule applied at the granularity the user actually edits.
fn env_key(table: &toml::Table, key: &str) -> Option<BTreeMap<String, String>> {
    let found = table.get(key)?;
    let Some(entries) = found.as_table() else {
        tracing::warn!(key, found = found.type_str(), "wrong type for profile key; ignoring");
        return None;
    };
    let mut out = BTreeMap::new();
    for (name, value) in entries {
        match value {
            toml::Value::String(v) => {
                out.insert(name.clone(), v.clone());
            }
            other => {
                tracing::warn!(
                    key = %format!("env.{name}"),
                    found = other.type_str(),
                    "an environment value must be a string; ignoring this entry"
                );
            }
        }
    }
    Some(out)
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
            widget: Widget::KeyValue,
            // The first line is the whole row: `settings_ui::first_line` splits
            // on a newline and a Rust `\` continuation makes none, so writing
            // this as one flowing paragraph put 600 characters in a control
            // that shows one. The rest is for the schema and the web client's
            // editor, which render the full text.
            description: "Environment for this profile's shell, layered over the host's own.\n\
                          \n\
                          An empty value unsets a variable. Pointing a tool at its own \
                          config directory is what makes a profile a separate login: \
                          `CLAUDE_CONFIG_DIR = \"${profile_dir}/claude\"`, and the same \
                          one-line shape for GH_CONFIG_DIR, KUBECONFIG or \
                          GIT_CONFIG_GLOBAL.\n\
                          \n\
                          Placeholders: `${profile_dir}` (this profile's own directory, \
                          created when something points into it), `${profile}`, \
                          `${home}`, and `${env:NAME}` for a variable the session would \
                          inherit anyway — `PATH = \"${env:PATH}:/opt/bin\"`. They \
                          resolve on the machine that *runs* the profile, so one profile \
                          means the same thing on every machine in the fleet. Nothing \
                          else expands: a bare `$FOO` is text, because there is no shell \
                          here to expand it, and a placeholder with no answer is left as \
                          written rather than emptied. `${env:…}` reads the inherited \
                          environment, never another entry in this table.\n\
                          \n\
                          Do not set HOME: the zsh hook resolves your dotfiles from the \
                          daemon's HOME, so the session would hand itself back the wrong \
                          ones. Per-tool config-dir variables avoid this entirely."
                .to_string(),
            default: serde_json::Value::Object(serde_json::Map::new()),
            ..text("env", "Launch", "")
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
    fn every_fields_first_line_fits_a_row() {
        // The profiles editor renders `first_line(&description)` -- everything
        // up to the first newline -- in one row. A Rust `\` continuation makes
        // no newline, so a description written as one flowing paragraph
        // arrives as a single 600-character "line" in a control sized for a
        // sentence. `env` shipped that way until this test existed.
        //
        // 140 rather than a tight bound: this is a wall against the failure
        // mode, not a style rule, and the longest existing field sits well
        // under it.
        for field in fields() {
            let first = field.description.lines().next().unwrap_or_default();
            assert!(
                first.len() <= 140,
                "'{}': the row shows only the first line, and this one is {} chars. \
                 Put the summary first, then a blank line, then the detail.",
                field.key,
                first.len()
            );
        }
    }

    #[test]
    fn a_profiles_env_merges_over_defaults_key_by_key() {
        // The deliberate opposite of `shell_env_replaces_wholesale_through_defaults`
        // directly above, and the reason `env` is a profile-only key rather
        // than a reuse of that settings key. `profiles.defaults.env` is where
        // fleet-wide variables live; a profile naming one of its own must not
        // throw them away, which a `fold_meta` field-level pick would.
        let c = config(
            "[profiles.defaults.env]\nSHARED = \"1\"\nOVERRIDDEN = \"defaults\"\n\
             [profiles.work.env]\nOVERRIDDEN = \"work\"\nOWN = \"2\"\n",
        );
        let meta = resolve_profile(&c, "work").meta;
        assert_eq!(meta.env.get("SHARED").map(String::as_str), Some("1"), "Defaults' entry survives");
        assert_eq!(meta.env.get("OWN").map(String::as_str), Some("2"), "the profile's own is there");
        assert_eq!(
            meta.env.get("OVERRIDDEN").map(String::as_str),
            Some("work"),
            "the named profile wins the keys it names, and only those"
        );
    }

    #[test]
    fn one_inherited_variable_is_dropped_by_emptying_it_not_by_clearing_the_table() {
        // Review caught the comment here claiming an empty `[profiles.x.env]`
        // meant "inherit nothing" while `fold_meta` merged regardless. The
        // code was right and the comment was wrong: an empty table is a no-op,
        // and the way to drop one inherited variable is the empty-value-unsets
        // convention that already runs all the way to the child.
        let c = config(
            "[profiles.defaults.env]\nKEEP = \"1\"\nDROP = \"2\"\n\
             [profiles.work.env]\nDROP = \"\"\n",
        );
        let meta = resolve_profile(&c, "work").meta;
        assert_eq!(meta.env.get("KEEP").map(String::as_str), Some("1"), "the others are untouched");
        assert_eq!(
            meta.env.get("DROP").map(String::as_str),
            Some(""),
            "the empty value must survive resolution -- it is what unsets the variable at the \
             pty, so swallowing it here would silently restore Defaults' value"
        );

        // And the no-op, stated so the comment cannot drift back.
        let c = config("[profiles.defaults.env]\nKEEP = \"1\"\n[profiles.empty.env]\n");
        let meta = resolve_profile(&c, "empty").meta;
        assert_eq!(
            meta.env.get("KEEP").map(String::as_str),
            Some("1"),
            "an empty table inherits; it is not a way to clear Defaults"
        );
    }

    #[test]
    fn a_profile_name_containing_another_is_not_inside_it() {
        // `<config>/profiles/work` is a textual prefix of
        // `<config>/profiles/work2`, which is the shape of the bug review
        // found one layer up in `ensure_profile_dir`. Pinned here too, because
        // this is where the two paths are built and the sibling relationship
        // is created.
        let ctx = |name: &str| ExpandContext {
            profile: name.into(),
            config_dir: Some(std::path::PathBuf::from("/cfg")),
            home: None,
            env: Default::default(),
        };
        let work = ctx("work").profile_dir().expect("a plain name has a directory");
        let work2 = ctx("work2").profile_dir().expect("so does its neighbour");
        assert_ne!(work, work2);
        assert!(
            !work2.starts_with(&work),
            "one profile's directory must not be inside another's: {work2:?} under {work:?}"
        );
    }

    #[test]
    fn a_dotted_name_that_escapes_nothing_keeps_its_directory() {
        // The guard rejects `.` and `..` as whole segments, not as a
        // substring: review pointed out the first spelling refused any name
        // *containing* `..`, which a profile legitimately called `node..old`
        // does while escaping nothing.
        let ctx = ExpandContext {
            profile: "node..old".into(),
            config_dir: Some(std::path::PathBuf::from("/cfg")),
            home: None,
            env: Default::default(),
        };
        assert_eq!(
            ctx.profile_dir(),
            Some(std::path::PathBuf::from("/cfg").join("profiles").join("node..old")),
            "a name that merely contains dots is not a name that climbs"
        );
    }

    #[test]
    fn a_profiles_env_is_not_a_settings_override() {
        // `env` is stripped from the settings layer like every other
        // profile-only key. Left in, every profile-tab launch would report it
        // in `unknown_keys` and the settings UI would warn about a key that is
        // perfectly legal where it was written.
        let c = config("[profiles.work.env]\nA = \"1\"\n");
        let r = resolve_profile(&c, "work");
        assert!(r.overrides.is_empty(), "env must not reach the settings cascade: {:?}", r.overrides);
        assert!(r.unknown_keys.is_empty(), "and must not be reported as unknown: {:?}", r.unknown_keys);
    }

    #[test]
    fn a_wrong_typed_env_entry_is_dropped_without_taking_its_neighbours() {
        // Per-entry leniency, not per-table: the never-crash rule at the
        // granularity someone actually edits. A profile with one bad line
        // still launches with the rest of its environment.
        let c = config("[profiles.work.env]\nGOOD = \"1\"\nBAD = 7\n");
        let meta = resolve_profile(&c, "work").meta;
        assert_eq!(meta.env.get("GOOD").map(String::as_str), Some("1"));
        assert!(!meta.env.contains_key("BAD"), "a non-string entry is ignored, not coerced");
    }

    #[test]
    fn placeholders_resolve_against_the_machine_that_runs_the_profile() {
        let ctx = ExpandContext {
            profile: "work".into(),
            config_dir: Some(std::path::PathBuf::from("/cfg")),
            home: Some(std::path::PathBuf::from("/home/a")),
            env: Default::default(),
        };
        let dir = std::path::Path::new("/cfg").join("profiles").join("work");
        assert_eq!(expand("${profile_dir}/claude", &ctx), format!("{}/claude", dir.display()));
        assert_eq!(expand("${profile}", &ctx), "work");
        assert_eq!(expand("${home}/x", &ctx), "/home/a/x");
        // More than one in a value, and text on both sides of each.
        assert_eq!(
            expand("a${profile}b${profile}c", &ctx),
            "aworkbworkc",
            "every occurrence expands, not just the first"
        );
        assert_eq!(expand("plain", &ctx), "plain", "a value with no token is untouched");
    }

    /// An [`ExpandContext`] with a stated environment and nothing else.
    fn ctx_with_env(pairs: &[(&str, &str)]) -> ExpandContext {
        ExpandContext {
            env: pairs.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect(),
            ..ExpandContext::default()
        }
    }

    #[test]
    fn env_colon_reads_the_environment_the_child_will_inherit() {
        // The escape hatch, spelled as VS Code spells it for the same job.
        // Namespaced on purpose: a bare `${VAR}` would make `${profile}`
        // ambiguous between our placeholder and a variable of that name, and a
        // precedence rule is a thing users have to learn. kitty gets away with
        // the bare form only because it has no placeholders of its own.
        //
        // The environment is stated here rather than poked into the process
        // with `set_var`: review pointed out that reading `std::env` answers
        // the wrong question (the child's environment is this process's *plus*
        // what the spec layers), and taking the map as an argument makes these
        // tests hermetic as a side effect -- which matters on a threaded test
        // binary, per #403.
        let ctx = ctx_with_env(&[("ZESTERM_TEST_EXPAND_SRC", "/opt/base")]);
        assert_eq!(
            expand("${env:ZESTERM_TEST_EXPAND_SRC}/bin", &ctx),
            "/opt/base/bin",
            "the PATH-prepending case, which is why this exists at all"
        );
        assert_eq!(
            expand("${env:ZESTERM_TEST_NOT_SET_ANYWHERE}", &ctx),
            "${env:ZESTERM_TEST_NOT_SET_ANYWHERE}",
            "an unset name is left as written -- Tabby empties it instead, which turns one \
             typo in a PATH entry into a destroyed PATH with nothing to see"
        );
        assert_eq!(expand("${env:}", &ctx), "${env:}", "an empty name answers nothing");
    }

    #[test]
    fn env_colon_sees_what_the_spec_layered_not_the_raw_process() {
        // The distinction review caught. `terminal_env` clears another
        // terminal's stale identity and shell integration injects `ZDOTDIR`,
        // so a value resolved against `std::env` would name variables the
        // child does not get and miss ones it does. The context carries the
        // *effective* environment, so both directions are expressible.
        let ctx = ctx_with_env(&[("ZDOTDIR", "/from/integration")]);
        assert_eq!(
            expand("${env:ZDOTDIR}", &ctx),
            "/from/integration",
            "a variable the spec injected must be visible"
        );
        // And one `terminal_env` cleared is absent, so it reads as unset --
        // left verbatim rather than resolving to this process's stale copy.
        assert_eq!(
            expand("${env:WT_SESSION}", &ctx),
            "${env:WT_SESSION}",
            "a variable the child will not have must not answer from the daemon's own"
        );
    }

    #[test]
    fn a_placeholder_of_ours_is_never_shadowed_by_a_variable_of_the_same_name() {
        // The collision the `env:` prefix exists to make impossible. With a
        // bare `${profile}` this test could not be written -- one of the two
        // readings would have to lose, and which one would be a rule rather
        // than a fact.
        //
        // No `set_var`: the environment is an argument now, so this asserts
        // the rule without mutating a process-global on a threaded test
        // binary (#403's lesson, avoided rather than managed).
        let ctx = ExpandContext {
            profile: "ours".into(),
            env: [("profile".to_string(), "the-environments-idea-of-it".to_string())]
                .into_iter()
                .collect(),
            ..ExpandContext::default()
        };
        assert_eq!(expand("${profile}", &ctx), "ours", "our placeholder answers");
        assert_eq!(
            expand("${env:profile}", &ctx),
            "the-environments-idea-of-it",
            "and the environment is reachable under its own spelling, with no rule to learn"
        );
    }

    #[test]
    fn a_sibling_entry_is_not_a_source_for_expansion() {
        // kitty resolves earlier `env` lines because kitty.conf is
        // line-ordered. A profile's env is a `BTreeMap`, so "earlier" would
        // mean *alphabetically* earlier: `A = "${env:B}"` would resolve and
        // `Z = "${env:B}"` would not, for a reason invisible in the file.
        // Expansion reads the inherited environment and nothing else, and this
        // asserts the table itself is not a source.
        let c = config(
            "[profiles.work.env]\nBASE = \"/opt\"\nDERIVED = \"${env:BASE}/bin\"\n",
        );
        let meta = resolve_profile(&c, "work").meta;
        let ctx = ExpandContext::default();
        assert_eq!(
            expand(meta.env.get("DERIVED").expect("the entry"), &ctx),
            "${env:BASE}/bin",
            "a sibling key must not resolve -- and being left as written says so plainly"
        );
    }

    #[test]
    fn an_unresolvable_placeholder_is_left_as_written_rather_than_emptied() {
        // The failure mode this prevents is specific and nasty: emptying
        // `${profile_dir}` turns `"${profile_dir}/claude"` into `/claude` --
        // an absolute path at the filesystem root that a tool will happily
        // create or fail on, with the cause nowhere near the symptom.
        let ctx = ExpandContext::default();
        assert_eq!(expand("${profile_dir}/claude", &ctx), "${profile_dir}/claude");
        assert_eq!(expand("${nonsense}", &ctx), "${nonsense}", "an unknown token is text");
    }

    #[test]
    fn a_shell_style_variable_is_not_a_placeholder() {
        // These values reach the child's environment block directly, with no
        // shell anywhere to expand anything. Treating `$FOO` as a token would
        // promise a substitution nothing here performs.
        let ctx = ExpandContext {
            profile: "work".into(),
            config_dir: Some(std::path::PathBuf::from("/cfg")),
            home: Some(std::path::PathBuf::from("/home/a")),
            env: Default::default(),
        };
        assert_eq!(expand("$HOME/x", &ctx), "$HOME/x");
        assert_eq!(expand("${unterminated", &ctx), "${unterminated", "an unclosed token is text");
    }

    #[test]
    fn a_profile_name_that_could_escape_gets_no_directory() {
        // This is the one place a profile name becomes a *filesystem* path, so
        // a name carrying a separator must resolve to nothing rather than to
        // somewhere else entirely. The editor rejects such names; this is the
        // second lock, on the door that would matter.
        for name in ["../elsewhere", "a/b", "a\\b"] {
            let ctx = ExpandContext {
                profile: name.into(),
                config_dir: Some(std::path::PathBuf::from("/cfg")),
                home: None,
                env: Default::default(),
            };
            assert_eq!(ctx.profile_dir(), None, "{name} must not name a directory");
            assert_eq!(expand("${profile_dir}", &ctx), "${profile_dir}", "and must not expand");
        }
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

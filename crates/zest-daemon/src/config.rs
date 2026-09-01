//! Answering a client's questions about this machine's configuration (#498).
//!
//! The read is the cascade, not the file: an effective value with the layer
//! that wrote it. A client scraping `config.toml` through
//! [`ClientMessage::ReadFile`](zest_proto::ClientMessage::ReadFile) — which it
//! can, today, with no path restriction — sees the *user* layer alone and
//! cannot tell a value it is looking at from one a profile or a command-line
//! flag is currently overriding.
//!
//! The write is the same edit that file scraping could do, with one thing in
//! front of it, and that one thing is the whole reason this exists:
//!
//! `cascade::resolve` finishes with `try_into::<Settings>().unwrap_or_default()`,
//! so a **single wrongly-typed value resets the entire settings tree** to
//! defaults in every running client — `typography.size_pt = "big"` and the
//! person's themes, fonts, padding and keybindings all quietly revert, with no
//! error anywhere and nothing naming the cause. A writer that hands bytes to a
//! file cannot prevent that. [`check`] can, and turns it into a refusal a
//! client can act on.
//!
//! Everything here answers on the serve loop rather than a worker, on
//! `server.rs`'s own split: a read is one `read_to_string` plus a pure
//! cascade, and a write is one parse and one rename. Neither spawns anything.
//! The one genuinely unbounded thing, the theme directory scan, is bounded by
//! [`zest_theme::dir::MAX_THEMES`] rather than here — a caller that has to
//! remember a bound is a caller that forgets it.

use std::path::{Path, PathBuf};

// The crate's own re-export, so this does not pin a second copy of it.
use zest_config::toml_edit;

use zest_proto::{
    ConfigField, ConfigOp, ConfigProfile, ConfigTheme, ConfigValue, ConfigVariant, HostMessage,
};

/// What a daemon will say and change about its own configuration.
///
/// One struct rather than two fields on
/// [`DaemonConfig`](crate::DaemonConfig), so a harness cannot enable writes
/// without also naming the file they land in. `None` there means this daemon
/// answers no config questions at all — what every existing test wants, and
/// what a daemon built before this did.
#[derive(Debug, Clone)]
pub struct ConfigSeam {
    /// The file to read and write.
    ///
    /// A field rather than a call to
    /// [`zest_config::paths::config_write_target`] is the entire reason this
    /// struct exists: `directories::ProjectDirs` has no override, so a daemon
    /// test that exercised a write with no seam would write the developer's
    /// own config — and pass.
    pub path: PathBuf,
    /// Whether [`ClientMessage::SetConfig`](zest_proto::ClientMessage::SetConfig)
    /// is obeyed rather than refused.
    pub writes: bool,
}

/// The machine-readable spelling of a cascade layer.
///
/// Deliberately not `Source`'s `Display`, which is prose for a settings tab
/// ("set by profile `k8s-prod`"). A client branching on prose is what this
/// avoids; the two are allowed to diverge and this one is the contract.
fn source_name(source: &zest_config::Source) -> String {
    match source {
        zest_config::Source::Default => "default".into(),
        zest_config::Source::User => "user".into(),
        zest_config::Source::Profile(p) => format!("profile:{p}"),
        zest_config::Source::Workspace => "workspace".into(),
        zest_config::Source::CommandLine => "command-line".into(),
    }
}

/// Whether `key` is in scope for a request that asked for `keys`.
///
/// Prefix matching on **dot boundaries**, not `starts_with`: plain string
/// prefixing would make `window` select `windows_something` if such a key ever
/// existed, and a filter that quietly returns a neighbouring key is worse than
/// one that returns nothing.
fn in_scope(key: &str, keys: &[String]) -> bool {
    keys.is_empty()
        || keys.iter().any(|k| key == k || key.strip_prefix(k).is_some_and(|r| r.starts_with('.')))
}

/// A `toml::Value` in the spelling it would have in the file.
///
/// Goes through `toml_edit::Value` rather than `toml::to_string`, because the
/// latter cannot serialize a bare scalar at all — it wants a table — and the
/// obvious workaround of wrapping and then stripping `k = ` is a parser nobody
/// asked for.
fn toml_spelling(value: &toml::Value) -> String {
    let edit: toml_edit::Value = match value {
        toml::Value::String(s) => s.as_str().into(),
        toml::Value::Integer(i) => (*i).into(),
        toml::Value::Float(f) => (*f).into(),
        toml::Value::Boolean(b) => (*b).into(),
        toml::Value::Datetime(d) => d.to_string().as_str().into(),
        toml::Value::Array(a) => {
            toml_edit::Value::Array(a.iter().map(|v| parse_or_string(&toml_spelling(v))).collect())
        }
        toml::Value::Table(t) => {
            let mut inline = toml_edit::InlineTable::new();
            for (k, v) in t {
                inline.insert(k, parse_or_string(&toml_spelling(v)));
            }
            toml_edit::Value::InlineTable(inline)
        }
    };
    edit.to_string().trim().to_string()
}

fn parse_or_string(text: &str) -> toml_edit::Value {
    text.parse::<toml_edit::Value>().unwrap_or_else(|_| text.into())
}

/// The same, for a `serde_json::Value` — which is how `ui::fields` carries a
/// schema default.
fn json_spelling(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => String::new(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) => toml_edit::Value::from(s.as_str()).to_string().trim().into(),
        serde_json::Value::Array(a) => {
            let items: Vec<String> = a.iter().map(json_spelling).collect();
            format!("[{}]", items.join(", "))
        }
        serde_json::Value::Object(o) => {
            let items: Vec<String> =
                o.iter().map(|(k, v)| format!("{k} = {}", json_spelling(v))).collect();
            format!("{{ {} }}", items.join(", "))
        }
    }
}

/// Flatten a settings tree into dotted key and TOML spelling.
fn flatten(prefix: &str, value: &toml::Value, out: &mut Vec<(String, String)>) {
    match value {
        // A table that is a settings *group* is walked into; one that is a
        // value (`shell.env`, `profiles`) is not — the same distinction
        // `invalidate::is_group` draws, and for the same reason: walking a map
        // produces per-entry keys the schema has never heard of.
        toml::Value::Table(t) if is_group(prefix) => {
            for (k, v) in t {
                let path = if prefix.is_empty() { k.clone() } else { format!("{prefix}.{k}") };
                flatten(&path, v, out);
            }
        }
        other => {
            if !prefix.is_empty() {
                out.push((prefix.to_string(), toml_spelling(other)));
            }
        }
    }
}

/// Whether a dotted path names a settings group rather than a value.
///
/// Derived from the schema's own key list rather than restated, because a
/// hand-kept fourth copy of the group set is how the other three drifted
/// (`invalidate::is_group` was missing `prompt` until #499).
fn is_group(path: &str) -> bool {
    path.is_empty()
        || zest_config::schema::keys()
            .iter()
            .any(|k| k.strip_prefix(path).is_some_and(|r| r.starts_with('.')))
}

/// Validate one edit before it reaches the file.
///
/// Refuses in three ways, each of which a raw file write would have let
/// through:
///
/// 1. **An unknown key**, with the near misses named. A key that silently does
///    nothing is indistinguishable from one this version ignores, so a typo
///    looks exactly like a feature that is not implemented yet.
/// 2. **An illegal value** — unparseable, outside the schema's range, or not
///    one of a closed set of variants.
/// 3. **A value of the wrong type**, which is the one that matters: see the
///    module doc. The check is the real thing rather than a type comparison —
///    the edit is applied to the real settings tree in memory and the result
///    has to still deserialize.
pub fn check(
    settings: &toml::Table,
    key: &str,
    value: &str,
    profile: bool,
) -> Result<toml_edit::Value, String> {
    let schema_keys = zest_config::schema::keys();
    let profile_only = profile && zest_config::profiles::PROFILE_ONLY_KEYS.contains(&key);

    // `PROFILE_ONLY_KEYS` *widens* what a profile may set; it does not narrow
    // it. Deliberately not `PROFILE_SETTINGS_KEYS`, whose own doc says it
    // scopes the editor rather than the cascade -- a profile may legitimately
    // set any settings key, and checking against that list instead would
    // refuse the feature's headline use (a profile that recolours its window).
    if !profile_only && !schema_keys.iter().any(|k| k == key) {
        let near = nearest(key, &schema_keys, profile);
        return Err(if near.is_empty() {
            format!("no setting named `{key}`")
        } else {
            format!("no setting named `{key}`; did you mean {near}?")
        });
    }

    let parsed: toml_edit::Value = value
        .trim()
        .parse()
        .map_err(|e| format!("`{value}` is not a TOML value: {e}"))?;

    // A profile-only key is not in the schema, so there is no field to check
    // it against; its shape is enforced by `ProfileMeta`'s lenient parse.
    if !profile_only {
        if let Some(field) = zest_config::ui::fields().into_iter().find(|f| f.key == key) {
            check_against_field(&field, &parsed)?;
        }
        // The wipe guard. Applying the edit and re-deserializing is the only
        // check that catches every shape of type error, including ones no
        // per-field rule would model.
        let mut probe = settings.clone();
        if to_toml(&parsed).is_none() {
            return Err(format!("`{value}` is not a value this config file can hold"));
        }
        apply(&mut probe, key, &parsed);
        if probe.clone().try_into::<zest_config::Settings>().is_err() {
            return Err(format!(
                "`{value}` is the wrong type for `{key}`; writing it would make the \
                 whole config unreadable and silently reset every other setting"
            ));
        }
    }

    Ok(parsed)
}

/// A `toml_edit::Value` as a `toml::Value`.
///
/// Through a one-key document, because **`toml::Value` has no `FromStr` for a
/// bare scalar** — it parses a *document*, so `"big"` and `20.0` both fail.
/// Getting this wrong is silent in the worst way: the probe below simply never
/// changes, every value looks well-typed, and the wipe guard passes everything.
/// That is what the wrong-type test caught.
fn to_toml(value: &toml_edit::Value) -> Option<toml::Value> {
    let doc = format!("v = {}", value.to_string().trim());
    doc.parse::<toml::Table>().ok()?.remove("v")
}

/// Put a parsed value at a dotted key in a settings table, for the probe.
fn apply(table: &mut toml::Table, key: &str, value: &toml_edit::Value) {
    let Some(parsed) = to_toml(value) else { return };
    let mut node = table;
    let parts: Vec<&str> = key.split('.').collect();
    let Some((last, groups)) = parts.split_last() else { return };
    for g in groups {
        node = node
            .entry((*g).to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .expect("a settings group is a table");
    }
    node.insert((*last).to_string(), parsed);
}

fn check_against_field(field: &zest_config::ui::UiField, value: &toml_edit::Value) -> Result<(), String> {
    if let Some((lo, hi)) = field.range {
        let n = value.as_float().or_else(|| value.as_integer().map(|i| i as f64));
        if let Some(n) = n {
            if n < lo || n > hi {
                return Err(format!("`{n}` is outside `{}`'s range of {lo} to {hi}", field.key));
            }
        }
    }
    if field.integer && value.as_float().is_some() {
        return Err(format!("`{}` is a whole number; write `14`, not `14.0`", field.key));
    }
    if !field.variants.is_empty() {
        if let Some(s) = value.as_str() {
            if !field.variants.iter().any(|v| v.value == s) {
                let legal: Vec<&str> = field.variants.iter().map(|v| v.value.as_str()).collect();
                return Err(format!(
                    "`{s}` is not one of `{}`'s values: {}",
                    field.key,
                    legal.join(", ")
                ));
            }
        }
    }
    Ok(())
}

/// The key `typed` was probably meant to be, if there is an obvious one.
///
/// `zest_config::schema::suggest_key` is the settings tab's own did-you-mean,
/// used here rather than reimplemented so a refusal over the wire and a row in
/// the UI name the same key for the same typo. Its `<= 2` edits cut-off is the
/// judgement being reused: a stretch suggestion sends somebody to the wrong
/// setting, which is worse than telling them nothing.
///
/// Inside a profile the profile-only keys are candidates too, since they are
/// legal there and are in no schema.
fn nearest(typed: &str, keys: &[String], profile: bool) -> String {
    let mut candidates = keys.to_vec();
    if profile {
        candidates.extend(
            zest_config::profiles::PROFILE_ONLY_KEYS.iter().map(|k| (*k).to_string()),
        );
    }
    // Also against the bare last segment, so `sizept` finds `size_pt` rather
    // than being drowned by the dotted prefix's own edit distance.
    let tail = typed.rsplit('.').next().unwrap_or(typed);
    zest_config::schema::suggest_key(typed, &candidates)
        .or_else(|| {
            let tails: Vec<String> = candidates
                .iter()
                .filter_map(|k| k.rsplit('.').next().map(str::to_owned))
                .collect();
            let near = zest_config::schema::suggest_key(tail, &tails)?;
            candidates.iter().find(|k| k.ends_with(&format!(".{near}")) || **k == near).cloned()
        })
        .map_or_else(String::new, |k| format!("`{k}`"))
}

/// Load this machine's config from `path`, as the cascade sees it.
///
/// Deliberately not `zest_config::load(&Options::default())`, which
/// `offer.rs` uses: that resolves the *real* config directory, and this has to
/// resolve the seam's file so a test is not reading the developer's own
/// settings. The layer set is otherwise the same — user file only, no profile
/// selected and no workspace layer, which is what the daemon runs on.
fn load_at(path: &Path) -> (zest_config::Resolved, Vec<String>, bool) {
    let exists = path.is_file();
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let mut problems = Vec::new();
    let table = match text.parse::<toml::Table>() {
        Ok(t) => t,
        Err(e) => {
            // Reported, never swallowed, and worded for the case that actually
            // happens: an editor mid-save. A client told "the file did not
            // parse" retries; one told nothing concludes the daemon is broken.
            problems.push(format!(
                "{} did not parse; it may be mid-save: {e}",
                path.display()
            ));
            toml::Table::new()
        }
    };
    let layers =
        [zest_config::Layer { source: zest_config::Source::User, table }];
    (zest_config::cascade::resolve(&layers), problems, exists)
}

/// Answer [`ClientMessage::GetConfig`](zest_proto::ClientMessage::GetConfig).
#[must_use]
pub fn get(
    seam: Option<&ConfigSeam>,
    keys: &[String],
    profile: &str,
    want_fields: bool,
    want_themes: bool,
) -> HostMessage {
    let refuse = |why: &str| HostMessage::ConfigState {
        keys: keys.to_vec(),
        profile: profile.to_string(),
        path: String::new(),
        exists: false,
        values: Vec::new(),
        profiles: Vec::new(),
        profile_detail: None,
        fields: Vec::new(),
        themes: Vec::new(),
        unknown_keys: Vec::new(),
        problems: Vec::new(),
        error: why.to_string(),
    };
    let Some(seam) = seam else {
        return refuse("this daemon does not serve configuration");
    };

    let (resolved, problems, exists) = load_at(&seam.path);

    let mut flat = Vec::new();
    let table = toml::Value::try_from(&resolved.settings).unwrap_or(toml::Value::Table(
        toml::Table::new(),
    ));
    flatten("", &table, &mut flat);

    let values: Vec<ConfigValue> = flat
        .into_iter()
        .filter(|(k, _)| in_scope(k, keys))
        .map(|(key, value)| {
            let source = resolved
                .provenance
                .get(&key)
                .map_or_else(|| "default".to_string(), source_name);
            ConfigValue { key, value, source }
        })
        .collect();

    let root = zest_config::profiles::root_of(&resolved.settings);
    let names = zest_config::profiles::list_profiles(&root);
    let profile_detail = (!profile.is_empty()).then(|| {
        let r = zest_config::profiles::resolve_profile(&root, profile);
        Box::new(project_profile(profile, &r))
    });

    let fields = if want_fields {
        zest_config::ui::fields()
            .into_iter()
            .filter(|f| in_scope(&f.key, keys))
            .map(project_field)
            .collect()
    } else {
        Vec::new()
    };

    let themes = if want_themes { roster() } else { Vec::new() };

    HostMessage::ConfigState {
        keys: keys.to_vec(),
        profile: profile.to_string(),
        path: seam.path.to_string_lossy().into_owned(),
        exists,
        values,
        profiles: names,
        profile_detail,
        fields,
        themes,
        unknown_keys: resolved.unknown_keys,
        problems,
        error: String::new(),
    }
}

/// This machine's themes: the built-ins, then whatever it imported.
fn roster() -> Vec<ConfigTheme> {
    let mut out: Vec<ConfigTheme> = zest_theme::builtin::all()
        .into_iter()
        .map(|t| ConfigTheme {
            id: t.id,
            name: t.name,
            mode: mode_name(t.mode),
            builtin: true,
        })
        .collect();
    if let Some(dir) = zest_config::paths::themes_dir() {
        out.extend(zest_theme::dir::load_dir(&dir).into_iter().map(|t| ConfigTheme {
            id: t.id,
            name: t.name,
            mode: mode_name(t.mode),
            builtin: false,
        }));
    }
    out
}

fn mode_name(mode: zest_theme::ThemeMode) -> String {
    match mode {
        zest_theme::ThemeMode::Dark => "dark".into(),
        zest_theme::ThemeMode::Light => "light".into(),
    }
}

fn project_field(f: zest_config::ui::UiField) -> ConfigField {
    ConfigField {
        key: f.key,
        group: f.group,
        // serde already spells `Widget` kebab-case for the settings UIs; going
        // through it keeps one spelling rather than a match arm that drifts.
        widget: serde_json::to_value(f.widget)
            .ok()
            .and_then(|v| v.as_str().map(str::to_owned))
            .unwrap_or_default(),
        description: f.description,
        range: f.range,
        integer: f.integer,
        variants: f
            .variants
            .into_iter()
            .map(|v| ConfigVariant { value: v.value, description: v.description })
            .collect(),
        default: json_spelling(&f.default),
        restart_hint: f.restart_hint,
    }
}

fn project_profile(name: &str, r: &zest_config::profiles::ProfileResolved) -> ConfigProfile {
    let mut overrides = Vec::new();
    flatten("", &toml::Value::Table(r.overrides.clone()), &mut overrides);
    ConfigProfile {
        name: name.to_string(),
        command: r.meta.command.clone().unwrap_or_default(),
        host: r.meta.host.clone().unwrap_or_default(),
        ask_host: r.meta.ask_host,
        starting_directory: r.meta.starting_directory.clone().unwrap_or_default(),
        tab_title: match &r.meta.tab_title {
            zest_config::profiles::TabTitle::FromShell => "from-shell".into(),
            zest_config::profiles::TabTitle::ProfileName => "profile-name".into(),
            zest_config::profiles::TabTitle::Custom(s) => s.clone(),
        },
        color_scheme: r.meta.color_scheme.clone().unwrap_or_default(),
        tab_color: r.meta.tab_color,
        icon: r.meta.icon.clone().unwrap_or_default(),
        color_from: match r.meta.color_from {
            Some(zest_config::profiles::ColorFrom::Profile) => "profile".into(),
            Some(zest_config::profiles::ColorFrom::Host) => "host".into(),
            None => String::new(),
        },
        overrides: overrides
            .into_iter()
            .map(|(key, value)| {
                // Which of the two layers wrote it, rather than a bare "this
                // profile has it": a value inherited from `profiles.defaults`
                // and one the profile sets itself are edited in different
                // places, which is the question somebody editing is asking.
                let source = match r.provenance.get(&key) {
                    Some(zest_config::profiles::ProfileProvenance::InheritedFromDefaults) => {
                        "profile:defaults".to_string()
                    }
                    _ => format!("profile:{name}"),
                };
                ConfigValue { key, value, source }
            })
            .collect(),
        unknown_keys: r.unknown_keys.clone(),
    }
}

/// Answer [`ClientMessage::SetConfig`](zest_proto::ClientMessage::SetConfig).
#[must_use]
pub fn set(
    seam: Option<&ConfigSeam>,
    op: ConfigOp,
    key: &str,
    profile: &str,
    value: &str,
    to: &str,
) -> HostMessage {
    let reply = |path: String, invalidation: &str, needs_restart: bool, effective, conflict, error: String| {
        HostMessage::ConfigWritten {
            op,
            key: key.to_string(),
            profile: profile.to_string(),
            to: to.to_string(),
            path,
            invalidation: invalidation.to_string(),
            needs_restart,
            effective,
            conflict,
            error,
        }
    };
    let refuse = |why: String| reply(String::new(), "", false, None, false, why);
    let conflict = |why: String| reply(String::new(), "", false, None, true, why);

    let Some(seam) = seam else {
        return refuse("this daemon does not serve configuration".into());
    };
    if !seam.writes {
        return refuse("this daemon serves configuration read-only".into());
    }

    let (before, problems, _) = load_at(&seam.path);
    if let Some(p) = problems.first() {
        // A file that did not parse must not be written through: `toml_edit`
        // would refuse anyway, but the message it gives names a byte offset
        // rather than the thing to do about it.
        return refuse(p.clone());
    }
    let path = seam.path.to_string_lossy().into_owned();

    let outcome: Result<(), String> = match op {
        ConfigOp::Set => {
            let table = match toml::Value::try_from(&before.settings) {
                Ok(toml::Value::Table(t)) => t,
                // Settings is a struct, so this cannot happen; defaulting is
                // still better than an unwrap on the write path.
                _ => toml::Table::new(),
            };
            match check(&table, key, value, !profile.is_empty()) {
                Err(e) => return refuse(e),
                Ok(parsed) => {
                    if profile.is_empty() {
                        zest_config::write_value(&seam.path, key, parsed)
                    } else {
                        zest_config::write_profile_value(&seam.path, profile, key, parsed)
                    }
                    .map_err(|e| e.to_string())
                }
            }
        }
        ConfigOp::Reset => if profile.is_empty() {
            zest_config::remove_value(&seam.path, key)
        } else {
            zest_config::remove_profile_value(&seam.path, profile, key)
        }
        .map_err(|e| e.to_string()),
        ConfigOp::CreateProfile => match reserved(profile) {
            Some(e) => return refuse(e),
            None => zest_config::create_profile(&seam.path, profile).map_err(|e| e.to_string()),
        },
        ConfigOp::CopyProfile => match reserved(to) {
            Some(e) => return refuse(e),
            None => match zest_config::copy_profile(&seam.path, profile, to) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return conflict(format!("there is no profile named `{profile}` to copy"));
                }
                other => other.map_err(|e| e.to_string()),
            },
        },
        ConfigOp::RenameProfile => match reserved(to) {
            Some(e) => return refuse(e),
            None => match zest_config::rename_profile(&seam.path, profile, to) {
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                    return conflict(format!("a profile named `{to}` already exists"));
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    return conflict(format!("there is no profile named `{profile}` to rename"));
                }
                other => other.map_err(|e| e.to_string()),
            },
        },
        ConfigOp::RemoveProfile => {
            zest_config::remove_profile(&seam.path, profile).map_err(|e| e.to_string())
        }
    };

    if let Err(e) = outcome {
        return refuse(e);
    }

    let (after, _, _) = load_at(&seam.path);
    let class = cost(&before, &after, op, key, profile);
    let effective = (!key.is_empty()).then(|| {
        let mut flat = Vec::new();
        let table = toml::Value::try_from(&after.settings)
            .unwrap_or(toml::Value::Table(toml::Table::new()));
        flatten("", &table, &mut flat);
        let value = flat
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
            .unwrap_or_default();
        let source = after.provenance.get(key).map_or_else(|| "default".to_string(), source_name);
        ConfigValue { key: key.to_string(), value, source }
    });

    reply(
        path,
        &class_name(class),
        class == zest_config::Invalidation::Restart,
        effective,
        false,
        String::new(),
    )
}

/// `defaults` is the layer every profile falls through, not a launch target.
fn reserved(name: &str) -> Option<String> {
    if name == zest_config::profiles::RESERVED_PROFILE {
        return Some(format!(
            "`{}` is the layer every profile inherits from, not a profile of its own",
            zest_config::profiles::RESERVED_PROFILE
        ));
    }
    if name.trim().is_empty() {
        return Some("a profile needs a name".into());
    }
    None
}

/// What a completed write costs a running client.
///
/// **A profile write cannot use `diff`**, and this is the trap: the daemon
/// resolves with no profile selected, so a root diff after a profile edit is
/// `Invalidation::None` — a geometry change reported as free. The key's own
/// class is the honest answer there.
///
/// And a profile-*only* key (`command`, `icon`, `tab_title`) is in no `KEYS`
/// row, so `class_of` would fall back to `Restart`. It maps to the existing
/// `("profiles", Free)` row instead, whose comment already says a running
/// session keeps what it launched with.
fn cost(
    before: &zest_config::Resolved,
    after: &zest_config::Resolved,
    op: ConfigOp,
    key: &str,
    profile: &str,
) -> zest_config::Invalidation {
    if !profile.is_empty() || matches!(
        op,
        ConfigOp::CreateProfile
            | ConfigOp::CopyProfile
            | ConfigOp::RenameProfile
            | ConfigOp::RemoveProfile
    ) {
        if key.is_empty() || zest_config::profiles::PROFILE_ONLY_KEYS.contains(&key) {
            return zest_config::invalidate::class_of("profiles");
        }
        return zest_config::invalidate::class_of(key);
    }
    zest_config::diff(&before.settings, &after.settings)
}

fn class_name(c: zest_config::Invalidation) -> String {
    match c {
        zest_config::Invalidation::None => "none",
        zest_config::Invalidation::Free => "free",
        zest_config::Invalidation::AtlasBump => "atlas-bump",
        zest_config::Invalidation::Geometry => "geometry",
        zest_config::Invalidation::SurfaceRebuild => "surface-rebuild",
        zest_config::Invalidation::Restart => "restart",
    }
    .into()
}

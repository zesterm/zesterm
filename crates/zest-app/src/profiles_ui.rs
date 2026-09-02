//! The profiles editor's rail, rows and preview, built as data (design §12).
//!
//! Pure on purpose, mirroring `settings_ui`: a resolved profile, the window's
//! resolved settings and a filter in; display rows, their inheritance chips
//! and a parallel action list out — one pass, so a drawn row and its meaning
//! cannot drift. Nothing here touches the window or the filesystem, which is
//! what lets the chip/provenance tests run without opening one.

use zest_config::profiles::{self, ProfileMeta, ProfileProvenance, ProfileResolved};
use zest_config::ui::{UiField, Widget};
use zest_config::{ColorFrom, Settings, TabTitle};

use crate::chrome::model::{
    AccentChoice, InheritChip, ProfilePreviewModel, ProfileRailRow, SchemeSwatch,
    SettingsRowModel, SettingsValueCell,
};
use crate::settings_ui::{self, EditBuffer, RowAction};

/// The §12 sections, in editor order. `section_of` folds every field into
/// one of these, so a schema group the list does not name cannot vanish.
pub const SECTION_ORDER: &[&str] = &["Launch", "Appearance", "Cursor"];

/// The launch identity fields: never a chip, never a dot. They are what makes
/// the profile a profile, so "inheriting" them is meaningless (§12) — and a
/// reset dot on `command` would read as "remove what this profile is".
///
/// `env` is here for a second reason on top of that one. Its chip would have
/// to say *one* thing about a row that is genuinely both: `fold_meta` merges
/// it key by key, so a profile's env is routinely part inherited and part its
/// own, and either chip would be a lie about half the rows.
pub const NEVER_CHIP: &[&str] = &["command", "host", "starting_directory", "env"];

/// The icon roster: BMP glyphs reachable by ordinary font fallback (the same
/// argument as the Settings chip's ⚙ — PUA icons need a Nerd Font that may
/// not exist). One roster, one place: the rail, the tiles and the tab chips
/// all draw from it. Everything here is from blocks the shipped stack
/// demonstrably covers — ★ (U+2605) was in this list and rendered as
/// *nothing* in the screenshot pass, the U+FF0B failure mode again: an icon
/// picker offering an invisible icon reads as broken.
pub const ICONS: &[&str] = &[
    "\u{276f}", // ❯
    "\u{25cf}", // ●
    "\u{25cb}", // ○
    "\u{25c6}", // ◆
    "\u{25b2}", // ▲
    "\u{25a0}", // ■
];

/// Display order inside Appearance (§12's reading order: scheme, type,
/// window, then the identity pair with `color_from` *before* the swatches it
/// gates). Unlisted keys append in schema order rather than vanishing.
const APPEARANCE_ORDER: &[&str] = &[
    "color_scheme",
    "typography.families",
    "typography.size_pt",
    "window.opacity",
    "window.backdrop",
    "window.background_image",
    "window.background_fit",
    "window.background_dim",
    "color_from",
    "tab_color",
    "icon",
];

/// Which §12 section a field draws under: the hand-authored groups map
/// straight through; the allowlisted settings groups (Text, Window) fold
/// into Appearance — the §12 table's shape, not the schema's.
#[must_use]
pub fn section_of(field: &UiField) -> &'static str {
    match field.group.as_str() {
        "Launch" => "Launch",
        "Cursor" => "Cursor",
        _ => "Appearance",
    }
}

/// Labels the generic humanizer gets wrong for this screen.
fn label_for(key: &str) -> String {
    match key {
        "ask_host" => "Ask which host at launch".to_string(),
        "tab_color" => "Tab colour".to_string(),
        "color_from" => "Colour from".to_string(),
        "color_scheme" => "Colour scheme".to_string(),
        _ => settings_ui::humanize_key(key),
    }
}

/// Everything the row builder needs about the window this editor lives in.
pub struct ProfileRowContext<'a> {
    /// The window's *resolved* settings as JSON — what an Unset row shows.
    pub window_values: &'a serde_json::Value,
    /// The window's theme id — the scheme an unset `color_scheme` follows.
    pub window_theme: &'a str,
    /// The resolved `shell.command` an unset `command` falls through to.
    pub fallback_command: &'a str,
    /// The local machine's display label — an unset `host` means it.
    pub local_host: &'a str,
    /// `(label, online)` from the fleet snapshot, for the host pill's dot.
    pub hosts: &'a [(String, bool)],
    /// The scheme picker's roster, from [`scheme_swatches`].
    pub schemes: &'a [SchemeSwatch],
    /// Editing Defaults: no inheritance chips — Defaults inherits from
    /// nothing, and a chip claiming "overrides Defaults" on Defaults itself
    /// would be a lie. The dot still marks (and resets) its own keys.
    pub is_defaults: bool,
}

/// Build the selected profile's rows, chips and actions — one pass, three
/// index-parallel lists (the picker discipline, plus §12's chips).
#[must_use]
pub fn build_profile_rows(
    fields: &[UiField],
    resolved: &ProfileResolved,
    ctx: &ProfileRowContext,
    filter: &str,
    editing: Option<&EditBuffer>,
    error: Option<&str>,
) -> (Vec<SettingsRowModel>, Vec<Option<InheritChip>>, Vec<RowAction>) {
    let filter = filter.to_lowercase();
    let overrides = toml_to_json(&toml::Value::Table(resolved.overrides.clone()));

    let mut rows = Vec::new();
    let mut chips = Vec::new();
    let mut actions = Vec::new();

    if let Some(error) = error {
        rows.push(SettingsRowModel::Notice { text: error.to_string() });
        chips.push(None);
        actions.push(RowAction::None);
    }

    for section in SECTION_ORDER {
        let members: Vec<usize> = ordered_indices(fields)
            .into_iter()
            .filter(|&i| section_of(&fields[i]) == *section)
            .filter(|&i| matches_filter(&fields[i], &filter))
            .collect();
        if members.is_empty() {
            continue;
        }
        rows.push(SettingsRowModel::Group { title: (*section).to_string() });
        chips.push(None);
        actions.push(RowAction::None);

        for idx in members {
            let field = &fields[idx];
            let provenance = resolved.provenance_of(&field.key);
            // `env` gives its chip slot to a different fact: it is on
            // NEVER_CHIP because an inheritance chip could only ever be half
            // true for it (`fold_meta` merges it key by key), and the slot is
            // better spent saying when the row takes effect.
            let chip = if field.key == "env" {
                Some(InheritChip::NewSessions)
            } else if ctx.is_defaults || NEVER_CHIP.contains(&field.key.as_str()) {
                None
            } else {
                match provenance {
                    ProfileProvenance::OverridesDefaults => Some(InheritChip::Overrides),
                    ProfileProvenance::InheritedFromDefaults => Some(InheritChip::Inherited),
                    ProfileProvenance::Unset => None,
                }
            };
            // The dot only on an override (§12) — and never on the launch
            // trio, whose reset would delete the profile's identity. On
            // Defaults the dot marks its own keys (reset removes them).
            let modified = provenance == ProfileProvenance::OverridesDefaults
                && !NEVER_CHIP.contains(&field.key.as_str());

            let value = effective_value(field, resolved, &overrides, ctx);
            let cell = match editing.filter(|e| e.field_idx == idx) {
                Some(edit) => SettingsValueCell::Editing {
                    buffer: edit.buffer.text().to_string(),
                    caret: edit.buffer.caret(),
                    selection: edit.buffer.selection(),
                    error: edit.error,
                },
                None => cell_for(field, &value, resolved, ctx),
            };
            rows.push(SettingsRowModel::Setting {
                label: label_for(&field.key),
                key: field.key.clone(),
                description: settings_ui::first_line(&field.description),
                value: cell,
                // The §12 chips travel in the parallel `chips` list — this
                // tuple's bool means *warn*, which no profile chip is.
                provenance: None,
                restart: false,
                inert: false,
                modified,
            });
            chips.push(chip);
            actions.push(RowAction::Field(idx));
        }
    }

    (rows, chips, actions)
}

/// Field indices in §12 display order: Launch and Cursor in schema order,
/// Appearance per `APPEARANCE_ORDER` with unlisted keys appended.
fn ordered_indices(fields: &[UiField]) -> Vec<usize> {
    let mut out: Vec<usize> = (0..fields.len()).collect();
    let rank = |i: usize| -> (usize, usize) {
        let key = fields[i].key.as_str();
        match APPEARANCE_ORDER.iter().position(|k| *k == key) {
            Some(p) => (0, p),
            None => (1, i),
        }
    };
    out.sort_by_key(|&i| rank(i));
    out
}

fn matches_filter(field: &UiField, filter: &str) -> bool {
    filter.is_empty()
        || field.key.to_lowercase().contains(filter)
        || label_for(&field.key).to_lowercase().contains(filter)
        || field.description.to_lowercase().contains(filter)
        || section_of(field).to_lowercase().contains(filter)
}

/// The merged defaults→profile overrides as JSON — [`effective_value`]'s
/// first lookup, exposed so the input path can compute the same value the
/// rows displayed.
#[must_use]
pub fn overrides_json(resolved: &ProfileResolved) -> serde_json::Value {
    toml_to_json(&toml::Value::Table(resolved.overrides.clone()))
}

/// The value a row shows: the profile's resolved value where one is set,
/// the window's resolved value otherwise (§12: "Unset → the window's
/// resolved value shown"). Only `window_values`, `window_theme`,
/// `fallback_command` and `local_host` are consulted from `ctx` — an input
/// path that has no fleet snapshot may pass empty `hosts`/`schemes`.
///
/// Display only: the unset launch strings come back as captions, not values
/// — a typed edit must seed from [`edit_seed_value`] instead.
#[must_use]
pub fn effective_value(
    field: &UiField,
    resolved: &ProfileResolved,
    overrides: &serde_json::Value,
    ctx: &ProfileRowContext,
) -> serde_json::Value {
    if field.key.contains('.') {
        // A settings key: the merged defaults→profile overrides, else the
        // window's resolved value.
        return zest_config::ui::value_at(overrides, &field.key)
            .cloned()
            .or_else(|| zest_config::ui::value_at(ctx.window_values, &field.key).cloned())
            .unwrap_or(serde_json::Value::Null);
    }
    let meta = &resolved.meta;
    match field.key.as_str() {
        "command" => serde_json::Value::String(
            meta.command.clone().unwrap_or_else(|| ctx.fallback_command.to_string()),
        ),
        "host" => serde_json::Value::String(
            meta.host.clone().unwrap_or_else(|| ctx.local_host.to_string()),
        ),
        "ask_host" => serde_json::Value::Bool(meta.ask_host),
        "starting_directory" => {
            serde_json::Value::String(meta.starting_directory.clone().unwrap_or_default())
        }
        // The *merged* env, Defaults' entries included, because that is what
        // the launch will actually use — `fold_meta` merges this one field
        // key by key rather than picking. An editor showing only the profile's
        // own rows would describe a different environment than the session
        // gets, which is the whole failure `shell_env_replaces_wholesale...`
        // guards against on the settings side.
        "env" => serde_json::Value::Object(
            meta.env
                .iter()
                .map(|(k, v)| (k.clone(), serde_json::Value::String(v.clone())))
                .collect(),
        ),
        "tab_title" => serde_json::Value::String(match &meta.tab_title {
            TabTitle::FromShell => "from-shell".to_string(),
            TabTitle::ProfileName => "profile-name".to_string(),
            TabTitle::Custom(s) => s.clone(),
        }),
        "color_scheme" => serde_json::Value::String(
            meta.color_scheme.clone().unwrap_or_else(|| ctx.window_theme.to_string()),
        ),
        "tab_color" => meta
            .tab_color
            .map_or(serde_json::Value::Null, |c| serde_json::json!(i64::from(c))),
        "icon" => serde_json::Value::String(meta.icon.clone().unwrap_or_default()),
        "color_from" => serde_json::Value::String(
            match meta.color_from.unwrap_or(ColorFrom::Profile) {
                ColorFrom::Profile => "profile",
                ColorFrom::Host => "host",
            }
            .to_string(),
        ),
        _ => serde_json::Value::Null,
    }
}

/// The value a typed edit opens with: [`effective_value`] everywhere except
/// the launch strings, which seed the profile's *resolved* value — empty when
/// unset — never the display fallback. `effective_value`'s unset `command` is
/// a caption (on a remote route, literally "the host's default shell") and
/// its unset `host` a display label, so a buffer seeded from the display path
/// is two Enters from committing a caption verbatim as a real override that a
/// launch would then spawn. An emptied buffer commits `""`, which resolution
/// treats exactly like an absent key (profiles.rs pins that), so the
/// untouched round trip stays unset.
#[must_use]
pub fn edit_seed_value(
    field: &UiField,
    resolved: &ProfileResolved,
    overrides: &serde_json::Value,
    ctx: &ProfileRowContext,
) -> serde_json::Value {
    match field.key.as_str() {
        "command" => {
            serde_json::Value::String(resolved.meta.command.clone().unwrap_or_default())
        }
        "host" => serde_json::Value::String(resolved.meta.host.clone().unwrap_or_default()),
        _ => effective_value(field, resolved, overrides, ctx),
    }
}

/// The §12 widget for a field: the profile pickers get their direct-choice
/// cells, the font stack draws as a pill (a profile overrides one face, not
/// a stack), everything else is the settings vocabulary unchanged.
fn cell_for(
    field: &UiField,
    value: &serde_json::Value,
    resolved: &ProfileResolved,
    ctx: &ProfileRowContext,
) -> SettingsValueCell {
    match field.widget {
        Widget::HostPicker => {
            let name = value.as_str().unwrap_or(ctx.local_host).to_string();
            let online = name == ctx.local_host
                || ctx.hosts.iter().any(|(label, online)| *label == name && *online);
            SettingsValueCell::HostPill { name, online }
        }
        Widget::SchemePicker => SettingsValueCell::SchemeSwatches {
            options: ctx.schemes.to_vec(),
            selected: value
                .as_str()
                .and_then(|v| ctx.schemes.iter().position(|s| s.id == v)),
        },
        Widget::AccentPicker => SettingsValueCell::AccentSwatches {
            selected: value.as_i64().and_then(|v| u8::try_from(v).ok()).filter(|v| *v < 6),
            // Dimmed and inert when the host decides — including the
            // inherited case, which is the §12 fleet story.
            inert: resolved.meta.color_from == Some(ColorFrom::Host),
        },
        Widget::IconPicker => SettingsValueCell::Glyphs {
            options: ICONS.iter().map(|g| (*g).to_string()).collect(),
            selected: value.as_str().and_then(|v| ICONS.iter().position(|g| *g == v)),
        },
        // The §12 "font pill": a profile picks one face; the stacked-list
        // widget is the root stack's editor, not an override's.
        Widget::FontList => SettingsValueCell::Select {
            value: value
                .as_array()
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .unwrap_or("\u{2014}")
                .to_string(),
        },
        // An unset command's caption ("the host's default shell") is a
        // placeholder, not a value: the input draws it faint, and the edit
        // seed already refuses to commit it (edit_seed_value).
        _ if field.key == "command" => SettingsValueCell::Text {
            text: value.as_str().unwrap_or_default().to_string(),
            placeholder: resolved.meta.command.is_none(),
        },
        _ => settings_ui::value_cell(field, Some(value), &[]),
    }
}

/// Arrow-key editing for the profile widgets `settings_ui::adjust` does not
/// know: the pickers cycle their rosters, the accent steps its six swatches.
#[must_use]
pub fn adjust_profile(
    field: &UiField,
    current: &serde_json::Value,
    dir: i32,
    themes: &[String],
    fonts: &[String],
) -> Option<serde_json::Value> {
    match field.widget {
        Widget::SchemePicker => {
            let cur = current.as_str().unwrap_or_default();
            let at = themes.iter().position(|t| t == cur).unwrap_or(0);
            let next = (at as i32 + dir).rem_euclid(themes.len().max(1) as i32) as usize;
            themes.get(next).map(|t| serde_json::Value::String(t.clone()))
        }
        Widget::IconPicker => {
            let cur = current.as_str().unwrap_or_default();
            let at = ICONS.iter().position(|g| *g == cur).unwrap_or(0);
            let next = (at as i32 + dir).rem_euclid(ICONS.len() as i32) as usize;
            Some(serde_json::Value::String(ICONS[next].to_string()))
        }
        Widget::AccentPicker => {
            // From "unset" the first press lands on 0, either direction.
            let next = match current.as_i64() {
                Some(cur) => (cur + i64::from(dir)).rem_euclid(6),
                None => 0,
            };
            Some(serde_json::json!(next))
        }
        Widget::HostPicker => None,
        _ => settings_ui::adjust(field, current, dir, themes, fonts),
    }
}

/// The value a direct-choice click writes — `ProfilesChoice(row, opt)`
/// resolved against the row's widget. `None` refuses rather than mis-writes.
#[must_use]
pub fn choice_value(
    field: &UiField,
    opt: usize,
    schemes: &[SchemeSwatch],
) -> Option<serde_json::Value> {
    match field.widget {
        Widget::SchemePicker => {
            schemes.get(opt).map(|s| serde_json::Value::String(s.id.clone()))
        }
        Widget::AccentPicker => {
            (opt < 6).then(|| serde_json::json!(opt as i64))
        }
        Widget::IconPicker => {
            ICONS.get(opt).map(|g| serde_json::Value::String((*g).to_string()))
        }
        _ => None,
    }
}

/// The scheme picker's roster: every theme's normal ANSI row in index
/// order — built-ins and imports alike — read through
/// `zest_theme::resolve`, never re-typed.
#[must_use]
pub fn scheme_swatches() -> Vec<SchemeSwatch> {
    crate::themes::all()
        .into_iter()
        .map(|theme| {
            let resolved = zest_theme::resolve(&theme);
            let mut ansi = [[0u8; 3]; 8];
            for (i, slot) in ansi.iter_mut().enumerate() {
                let c = resolved.colors[i];
                *slot = [c.r, c.g, c.b];
            }
            SchemeSwatch { id: theme.id, ansi }
        })
        .collect()
}

/// Rail order: Defaults pinned first, then the launcher's ordering (a
/// profile literally named `default` leads, the rest alphabetical) — so the
/// rail's digits and the launcher's ⌘digits name the same profiles.
#[must_use]
pub fn rail_names(settings: &Settings) -> Vec<String> {
    let root = crate::launcher::profiles_root(settings);
    let mut names = profiles::list_profiles(&root);
    if let Some(i) = names.iter().position(|n| n == "default") {
        let name = names.remove(i);
        names.insert(0, name);
    }
    let mut out = vec![profiles::RESERVED_PROFILE.to_string()];
    out.extend(names);
    out
}

/// The rail's rows, index-parallel to [`rail_names`].
#[must_use]
pub fn build_rail(
    settings: &Settings,
    fallback_command: &str,
    local_host: &str,
) -> Vec<ProfileRailRow> {
    let root = crate::launcher::profiles_root(settings);
    rail_names(settings)
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let resolved = profiles::resolve_profile(&root, name);
            let command =
                resolved.meta.command.clone().unwrap_or_else(|| fallback_command.to_string());
            let host = resolved.meta.host.clone().unwrap_or_else(|| local_host.to_string());
            ProfileRailRow {
                name: if i == 0 { "Defaults".to_string() } else { name.clone() },
                sub: format!("{command} \u{b7} {host}"),
                icon: resolved.meta.icon.clone(),
                accent: accent_of(&resolved.meta),
                // Defaults carries no digit: it is the parent, not a target.
                digit: u8::try_from(i).ok().filter(|d| (1..=9).contains(d)),
            }
        })
        .collect()
}

/// The chip accent a profile's meta resolves to — `tab_accent`'s truth table
/// for the local window (host slot 0), reachable from a bare resolve.
#[must_use]
pub fn accent_of(meta: &ProfileMeta) -> AccentChoice {
    match (meta.color_from, meta.tab_color) {
        (Some(ColorFrom::Host), _) | (_, None) => AccentChoice::Host(0),
        (_, Some(color)) => AccentChoice::Profile(color),
    }
}

/// The §12 live preview as pure data: the chip's accent and glyph (drawn in
/// the window's `ChromeColors` by the screen), the profile's scheme for the
/// body, and the caption naming the split.
#[must_use]
pub fn build_preview(
    display_name: &str,
    resolved: &ProfileResolved,
    window_theme: &str,
    os_line: &str,
) -> ProfilePreviewModel {
    // An unset (or unknown) scheme follows the window — the same fallback
    // the grid itself performs, so the preview cannot promise colours the
    // session will not get.
    let scheme_id = resolved
        .meta
        .color_scheme
        .as_deref()
        .filter(|s| crate::themes::get(s).is_some())
        .unwrap_or(window_theme);
    let theme = crate::themes::get(scheme_id).unwrap_or_else(zest_theme::builtin::obsidian);
    let palette = zest_theme::resolve(&theme);
    ProfilePreviewModel {
        title: display_name.to_string(),
        icon: resolved.meta.icon.clone(),
        accent: accent_of(&resolved.meta),
        scheme_bg: [palette.background.r, palette.background.g, palette.background.b],
        scheme_fg: [palette.foreground.r, palette.foreground.g, palette.foreground.b],
        scheme_accent: [theme.ui.accent.r, theme.ui.accent.g, theme.ui.accent.b],
        caption: format!(
            "Chrome is the window's theme ({window_theme}). Only the grid follows this \
             profile's scheme."
        ),
        lines: vec!["\u{276f} uname -sr".to_string(), os_line.to_string()],
    }
}

/// How many of the editor's fields override Defaults — the footer's number.
/// The launch trio is excluded, exactly as it draws no chip and no dot.
#[must_use]
pub fn override_count(fields: &[UiField], resolved: &ProfileResolved) -> usize {
    fields
        .iter()
        .filter(|f| !NEVER_CHIP.contains(&f.key.as_str()))
        .filter(|f| resolved.provenance_of(&f.key) == ProfileProvenance::OverridesDefaults)
        .count()
}

/// The footer's sentence — singular handled, and the §12 fall-through
/// sentence on Defaults.
#[must_use]
pub fn footer_sentence(is_defaults: bool, count: usize) -> String {
    if is_defaults {
        return "Every profile falls through to this one".to_string();
    }
    match count {
        0 => "Every setting falls through to Defaults".to_string(),
        1 => "1 setting overrides Defaults".to_string(),
        n => format!("{n} settings override Defaults"),
    }
}

/// A fresh profile's table name: `new-profile-N`, smallest free N.
#[must_use]
pub fn new_profile_name(existing: &[String]) -> String {
    (1..)
        .map(|n| format!("new-profile-{n}"))
        .find(|candidate| !existing.iter().any(|e| e == candidate))
        .expect("the integers do not run out")
}

/// A duplicate's name: `<from>-2`, `<from>-3`, … — first free suffix.
#[must_use]
pub fn copy_name(existing: &[String], from: &str) -> String {
    (2..)
        .map(|n| format!("{from}-{n}"))
        .find(|candidate| !existing.iter().any(|e| e == candidate))
        .expect("the integers do not run out")
}

/// Why a profile cannot be renamed to `to`, or `None` when it can (#283).
///
/// The caller trims before offering the name here and writes exactly what it
/// validated — validating one string and writing another is how a check that
/// reads correctly passes something it rejected.
///
/// `to == from` is allowed and means "nothing changed": the name entry commits
/// whether or not it was edited, and refusing a name for already being its own
/// would make Enter an error on every unmodified field.
#[must_use]
pub fn rename_error(existing: &[String], from: &str, to: &str) -> Option<&'static str> {
    if to == from {
        return None;
    }
    if to.is_empty() {
        return Some("a profile needs a name");
    }
    // `defaults` is the parent every other profile inherits from, so a second
    // one is not a name collision but a broken cascade. `rail_names` carries
    // it at index 0, so the collision check below would catch it anyway — this
    // arm exists to say *why*, which the generic message cannot.
    if to == profiles::RESERVED_PROFILE {
        return Some("defaults is the parent every profile inherits from");
    }
    // A control character survives TOML quoting and reaches the rail, the tab
    // chip and `[profiles.<name>]` as an escape nobody typed. Paste is how one
    // arrives; nothing about the name entry stops it.
    if to.chars().any(char::is_control) {
        return Some("a profile name cannot contain control characters");
    }
    // A name is also a *path segment*: `${profile_dir}` is
    // `<config>/profiles/<name>` (#496). A separator would make it two
    // segments and `..` would climb out, so a profile could name a directory
    // somewhere else entirely — and until this check existed, `profile_dir`'s
    // own guard was the only thing standing there, silently refusing to
    // resolve for a name the editor had happily accepted.
    if to.contains(['/', '\\']) {
        return Some("a profile name cannot contain / or \\");
    }
    if to.split(['/', '\\']).any(|part| part == "." || part == "..") {
        return Some("a profile name cannot be . or ..");
    }
    if existing.iter().any(|e| e == to) {
        return Some("a profile with that name already exists");
    }
    None
}

/// TOML to JSON, structurally — the row builders speak `serde_json::Value`
/// (the settings vocabulary), the resolver speaks `toml`.
fn toml_to_json(value: &toml::Value) -> serde_json::Value {
    match value {
        toml::Value::String(s) => serde_json::Value::String(s.clone()),
        toml::Value::Integer(i) => serde_json::json!(i),
        toml::Value::Float(f) => serde_json::json!(f),
        toml::Value::Boolean(b) => serde_json::Value::Bool(*b),
        toml::Value::Datetime(d) => serde_json::Value::String(d.to_string()),
        toml::Value::Array(a) => {
            serde_json::Value::Array(a.iter().map(toml_to_json).collect())
        }
        toml::Value::Table(t) => serde_json::Value::Object(
            t.iter().map(|(k, v)| (k.clone(), toml_to_json(v))).collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings_ui::{take_pending_edit, Pending};
    use zest_config::profiles::resolve_profile;

    fn fields() -> Vec<UiField> {
        zest_config::profiles::fields()
    }

    fn config(text: &str) -> toml::Table {
        text.parse().expect("valid toml")
    }

    fn window_values() -> serde_json::Value {
        serde_json::to_value(Settings::default()).expect("serializes")
    }

    fn ctx<'a>(
        values: &'a serde_json::Value,
        schemes: &'a [SchemeSwatch],
        is_defaults: bool,
    ) -> ProfileRowContext<'a> {
        ProfileRowContext {
            window_values: values,
            window_theme: "obsidian",
            fallback_command: "pwsh -NoLogo",
            local_host: "studio",
            hosts: &[],
            schemes,
            is_defaults,
        }
    }

    fn build(
        resolved: &ProfileResolved,
        is_defaults: bool,
    ) -> (Vec<SettingsRowModel>, Vec<Option<InheritChip>>, Vec<RowAction>) {
        let values = window_values();
        let schemes = scheme_swatches();
        build_profile_rows(&fields(), resolved, &ctx(&values, &schemes, is_defaults), "", None, None)
    }

    fn field_index(key: &str) -> usize {
        fields().iter().position(|f| f.key == key).unwrap_or_else(|| panic!("{key} is a field"))
    }

    fn open_edit(field_idx: usize, text: &str) -> Option<EditBuffer> {
        Some(EditBuffer {
            field_idx,
            buffer: crate::text_field::TextField::new(text),
            error: false,
            append: false,
        })
    }

    #[test]
    fn a_profile_name_must_be_one_path_segment() {
        // A name is a TOML key *and* a directory segment: `${profile_dir}` is
        // `<config>/profiles/<name>` (#496). Review found the guard on the
        // config side claiming this entry already enforced it, when nothing
        // did -- so a name accepted here silently produced a profile whose
        // `${profile_dir}` would not resolve, with the refusal landing at
        // spawn time and nowhere near the entry that caused it.
        let names = vec!["defaults".to_string(), "wsl".to_string()];
        assert!(rename_error(&names, "wsl", "a/b").is_some(), "a separator makes it two segments");
        assert!(rename_error(&names, "wsl", "a\\b").is_some(), "and so does the other one");
        assert!(rename_error(&names, "wsl", "..").is_some(), "`..` climbs out of the directory");
        assert!(rename_error(&names, "wsl", ".").is_some(), "and `.` is not a name");
        assert_eq!(
            rename_error(&names, "wsl", "node..old"),
            None,
            "a name that merely contains dots escapes nothing and stays legal"
        );
        assert_eq!(rename_error(&names, "wsl", "work"), None, "an ordinary name is unaffected");
    }

    #[test]
    fn the_env_row_shows_the_merged_environment_the_launch_will_use() {
        // Caught a real gap: `effective_value` matches profile-only keys by
        // name and falls through to `Null`, so a new key renders as an empty
        // row — a control that looks like "nothing set" over a profile that
        // has three variables. Nothing else would have noticed, because
        // `every_profile_field_arrives_renderable` asserts the field exists,
        // not that it carries its value.
        //
        // Merged, Defaults included, because that is what the *launch* uses:
        // an editor showing only the profile's own entries would describe a
        // different environment than the session gets.
        let c = config(
            "[profiles.defaults.env]\nSHARED = \"1\"\n\
             [profiles.work.env]\nOWN = \"2\"\n",
        );
        let resolved = resolve_profile(&c, "work");
        let values = window_values();
        let schemes = scheme_swatches();
        let shown = effective_value(
            &fields()[field_index("env")],
            &resolved,
            &serde_json::Value::Null,
            &ctx(&values, &schemes, false),
        );
        let map = shown.as_object().expect("the env row is an object, which KeyValue renders");
        assert_eq!(map.get("OWN").and_then(|v| v.as_str()), Some("2"));
        assert_eq!(
            map.get("SHARED").and_then(|v| v.as_str()),
            Some("1"),
            "Defaults' entries are part of what this profile launches with, so the row shows them"
        );
    }

    #[test]
    fn the_env_row_says_when_it_takes_effect_rather_than_claiming_an_inheritance() {
        // Every other row's chip says one of two things about inheritance.
        // `env` is genuinely both at once -- `fold_meta` merges it key by key
        // -- so either would be a lie about half the rows, and the slot is
        // better spent on the fact a user actually needs: a running process
        // cannot be handed a new environment, so this reaches new sessions
        // only.
        let c = config("[profiles.work.env]\nOWN = \"2\"\n");
        let (rows, chips, _) = build(&resolve_profile(&c, "work"), false);
        let idx = rows
            .iter()
            .position(|r| matches!(r, SettingsRowModel::Setting { key, .. } if key == "env"))
            .expect("the env row is rendered");
        assert_eq!(
            chips[idx],
            Some(InheritChip::NewSessions),
            "env must say when it applies, and must never claim an inheritance it half has"
        );

        // And on Defaults, where every other chip is suppressed, it still
        // says it: the fact is about processes, not about the cascade.
        let (rows, chips, _) = build(&resolve_profile(&c, "defaults"), true);
        let idx = rows
            .iter()
            .position(|r| matches!(r, SettingsRowModel::Setting { key, .. } if key == "env"))
            .expect("the env row is rendered on Defaults too");
        assert_eq!(chips[idx], Some(InheritChip::NewSessions));
    }

    #[test]
    fn a_rename_is_refused_for_a_reason_the_user_can_read() {
        // #283. Each arm is a different failure, and the messages differ
        // because "already exists" is wrong for `defaults` — it exists, but
        // the reason it cannot be taken is that the whole cascade resolves
        // through it.
        let names = vec!["defaults".to_string(), "wsl".to_string(), "mac".to_string()];
        assert_eq!(rename_error(&names, "wsl", "forge"), None, "a free name is free");
        assert_eq!(
            rename_error(&names, "wsl", "wsl"),
            None,
            "committing an unchanged name is a no-op, not an error — the entry \
             commits whether or not it was edited"
        );
        assert!(rename_error(&names, "wsl", "").is_some(), "a profile needs a name");
        assert!(
            rename_error(&names, "wsl", "mac").is_some(),
            "renaming onto a live profile would destroy it silently"
        );
        assert_ne!(
            rename_error(&names, "wsl", "defaults"),
            rename_error(&names, "wsl", "mac"),
            "the reserved parent is refused for its own reason, not as a collision"
        );
        assert!(
            rename_error(&names, "wsl", "for\nge").is_some(),
            "a pasted control character reaches the rail and the tab chip as an \
             escape nobody typed"
        );
    }

    #[test]
    fn leaving_a_field_hands_the_edit_back_instead_of_dropping_it() {
        // #272: Enter was the only thing that committed, so a pasted command
        // path died on the way out of the field — silently, and identically
        // whether you clicked the rail, hit Duplicate or closed the tab.
        let fields = fields();
        let idx = field_index("command");
        let mut editing = open_edit(idx, "/opt/homebrew/bin/fish");
        assert_eq!(
            take_pending_edit(&mut editing, &fields),
            Pending::Commit(idx, serde_json::Value::String("/opt/homebrew/bin/fish".into())),
            "leaving a field must hand the value back to be written, not drop it"
        );
        assert!(editing.is_none(), "a committed buffer closes");
    }

    #[test]
    fn an_emptied_field_still_commits_because_empty_means_unset() {
        // The file's spelling of "unset" is the empty string, and resolution
        // falls back through Defaults for it — so clearing a field and
        // leaving must write, not be mistaken for "nothing pending".
        let fields = fields();
        let idx = field_index("command");
        let mut editing = open_edit(idx, "");
        assert_eq!(
            take_pending_edit(&mut editing, &fields),
            Pending::Commit(idx, serde_json::Value::String(String::new())),
            "clearing a field is an edit like any other"
        );
    }

    #[test]
    fn a_buffer_that_cannot_be_written_refuses_to_be_left() {
        // The counterpart to committing on blur: a value that does not parse
        // must not be silently destroyed by looking away from it either. It
        // stays open, flagged, and the caller is told to stay put.
        let fields = fields();
        let idx = field_index("typography.size_pt");
        let mut editing = open_edit(idx, "not a number");
        assert_eq!(
            take_pending_edit(&mut editing, &fields),
            Pending::Refused,
            "an unparseable buffer blocks the exit rather than being dropped"
        );
        let edit = editing.as_ref().expect("the buffer stays open");
        assert!(edit.error, "and it says why");
        assert_eq!(edit.buffer.text(), "not a number", "with the text intact");
    }

    #[test]
    fn nothing_open_never_blocks_the_caller() {
        let fields = fields();
        let mut editing = None;
        assert_eq!(take_pending_edit(&mut editing, &fields), Pending::None);
    }

    #[test]
    fn a_buffer_naming_a_field_that_is_gone_is_dropped_not_stuck() {
        // Keeping it would trap every exit for ever: it can be neither
        // written nor shown, so `Refused` would be a door that never opens.
        let mut editing = open_edit(9999, "orphan");
        assert_eq!(take_pending_edit(&mut editing, &fields()), Pending::None);
        assert!(editing.is_none(), "an unwritable orphan is dropped, not held");
    }

    #[test]
    fn every_profile_field_renders_exactly_once_and_the_lists_stay_parallel() {
        // The §12 form is generated from profiles::fields() — a field that
        // renders twice or not at all is a drifted display-order list.
        let r = resolve_profile(&toml::Table::new(), "ubuntu");
        let (rows, chips, actions) = build(&r, false);
        assert_eq!(rows.len(), chips.len(), "chips ride index-parallel to rows");
        assert_eq!(rows.len(), actions.len(), "actions too — one pass builds all three");
        for field in fields() {
            let count = rows
                .iter()
                .filter(|r| matches!(r, SettingsRowModel::Setting { key, .. } if *key == field.key))
                .count();
            assert_eq!(count, 1, "'{}' must render exactly once", field.key);
        }
        // And the sections arrive in §12 order, as Group rows.
        let groups: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                SettingsRowModel::Group { title } => Some(title.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(groups, SECTION_ORDER, "Launch, Appearance, Cursor — the §12 rules");
        // color_from precedes the swatches it gates (§12's reading order).
        let keys: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                SettingsRowModel::Setting { key, .. } => Some(key.as_str()),
                _ => None,
            })
            .collect();
        let from = keys.iter().position(|k| *k == "color_from").expect("exists");
        let color = keys.iter().position(|k| *k == "tab_color").expect("exists");
        assert!(from < color, "the segmented choice comes before the swatches it dims");
    }

    #[test]
    fn chips_track_profile_provenance_exactly() {
        // The three §12 states per key: overrides → chip + dot, inherited →
        // chip alone, unset → neither. Getting these wrong makes the
        // inheritance UI lie, which is worse than not having one.
        let c = config(
            "[profiles.defaults]\nicon = \"\u{2605}\"\n\
             [profiles.defaults.window]\nopacity = 0.9\n\
             [profiles.ubuntu]\ntab_color = 2\ncommand = \"wsl.exe\"\n",
        );
        let r = resolve_profile(&c, "ubuntu");
        let (rows, chips, _) = build(&r, false);
        let of = |wanted: &str| {
            rows.iter()
                .zip(&chips)
                .find_map(|(row, chip)| match row {
                    SettingsRowModel::Setting { key, modified, .. } if key == wanted => {
                        Some((*chip, *modified))
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("'{wanted}' row exists"))
        };
        assert_eq!(
            of("tab_color"),
            (Some(InheritChip::Overrides), true),
            "set on the profile: the override chip AND the dot"
        );
        assert_eq!(
            of("icon"),
            (Some(InheritChip::Inherited), false),
            "fell through to Defaults: the inherited chip, no dot"
        );
        assert_eq!(
            of("window.opacity"),
            (Some(InheritChip::Inherited), false),
            "settings keys inherit through the same cascade"
        );
        assert_eq!(of("cursor.blink"), (None, false), "set nowhere: no chip at all");
        assert_eq!(
            of("command"),
            (None, false),
            "the launch trio never chips — inheriting what makes the profile \
             a profile is meaningless (§12)"
        );
        // And on Defaults itself, no chip lies about inheritance, but the
        // dot still marks (and resets) its own keys.
        //
        // `NewSessions` is exempt by construction rather than by exception:
        // it is not an inheritance claim at all, it is a fact about processes
        // — a running shell cannot be handed a new environment, on Defaults
        // or anywhere else. Asserted as "no *inheritance* chip" so this test
        // keeps meaning what its name says.
        let r = resolve_profile(&c, "defaults");
        let (rows, chips, _) = build(&r, true);
        for (row, chip) in rows.iter().zip(&chips) {
            assert!(
                !matches!(chip, Some(InheritChip::Overrides | InheritChip::Inherited)),
                "Defaults inherits from nothing: {row:?}"
            );
        }
        let dot = rows.iter().find_map(|row| match row {
            SettingsRowModel::Setting { key, modified, .. } if key == "icon" => Some(*modified),
            _ => None,
        });
        assert_eq!(dot, Some(true), "Defaults' own keys keep their reset dot");
    }

    #[test]
    fn an_unset_row_shows_the_windows_resolved_value() {
        let r = resolve_profile(&toml::Table::new(), "bare");
        let (rows, _, _) = build(&r, false);
        let cell = |wanted: &str| {
            rows.iter()
                .find_map(|row| match row {
                    SettingsRowModel::Setting { key, value, .. } if key == wanted => {
                        Some(value.clone())
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("'{wanted}' row exists"))
        };
        assert_eq!(
            cell("command"),
            SettingsValueCell::Text { text: "pwsh -NoLogo".into(), placeholder: true },
            "an unset command shows the resolved shell AS A PLACEHOLDER — faint, \
             because it is what will run, not what is written (#183)"
        );
        assert!(
            matches!(cell("host"), SettingsValueCell::HostPill { name, online: true } if name == "studio"),
            "an unset host is the local machine"
        );
        let defaults = Settings::default();
        assert!(
            matches!(cell("typography.size_pt"), SettingsValueCell::Stepper { text }
                if text == format!("{} pt", defaults.typography.size_pt)),
            "an unset settings key shows the window's resolved value"
        );
        assert!(
            matches!(cell("color_scheme"), SettingsValueCell::SchemeSwatches { selected: Some(_), .. }),
            "an unset scheme highlights the window's theme — what the grid will use"
        );
        assert!(
            matches!(cell("tab_color"), SettingsValueCell::AccentSwatches { selected: None, inert: false }),
            "an unset tab colour rings nothing"
        );
    }

    #[test]
    fn the_edit_seed_never_fabricates_the_display_fallbacks() {
        // `effective_value`'s unset command/host are display captions — on a
        // remote-routed window `fallback_command` is literally the string
        // "the host's default shell" — and `app::profiles_begin_edit` seeds
        // the typed edit from this module, so a caption-seeded buffer is two
        // Enters from being committed verbatim as a real [profiles.<name>]
        // value that a launch would then try to spawn.
        let all = fields();
        let field = |key: &str| all.iter().find(|f| f.key == key).expect("field");
        let values = window_values();
        let schemes = scheme_swatches();
        let ctx = ctx(&values, &schemes, false);

        // Unset: the row still captions the fallback, but the seed is EMPTY —
        // "" is the file's spelling of unset (profiles.rs pins that), so an
        // untouched buffer round-trips to unset instead of inventing a value.
        let r = resolve_profile(&toml::Table::new(), "bare");
        let overrides = overrides_json(&r);
        assert_eq!(
            effective_value(field("command"), &r, &overrides, &ctx),
            serde_json::json!("pwsh -NoLogo"),
            "the display keeps its caption — the fix is the seed, not the row"
        );
        assert_eq!(
            edit_seed_value(field("command"), &r, &overrides, &ctx),
            serde_json::json!(""),
            "an unset command seeds empty, never the fallback caption"
        );
        assert_eq!(
            edit_seed_value(field("host"), &r, &overrides, &ctx),
            serde_json::json!(""),
            "an unset host seeds empty, never a display label like 'this machine'"
        );

        // Set (directly or through Defaults): the resolved value, verbatim —
        // the buffer must agree with the row it opened from.
        let c = config(
            "[profiles.defaults]\nhost = \"mini\"\n[profiles.mac]\ncommand = \"zsh -l\"\n",
        );
        let r = resolve_profile(&c, "mac");
        let overrides = overrides_json(&r);
        assert_eq!(
            edit_seed_value(field("command"), &r, &overrides, &ctx),
            serde_json::json!("zsh -l"),
            "a set command seeds its stored value"
        );
        assert_eq!(
            edit_seed_value(field("host"), &r, &overrides, &ctx),
            serde_json::json!("mini"),
            "a host inherited from Defaults is a real value, so it seeds"
        );

        // Every other key keeps the settings-tab discipline — its fallback
        // is the window's resolved value, a real value worth stepping from.
        assert_eq!(
            edit_seed_value(field("color_scheme"), &r, &overrides, &ctx),
            effective_value(field("color_scheme"), &r, &overrides, &ctx),
            "non-launch keys still seed what the row shows"
        );
    }

    #[test]
    fn the_reset_action_resolves_to_the_profiles_table_never_the_root() {
        // Row → parallel action → field → key, then the file round trip:
        // the dot's delete lands under [profiles.<name>], and a root key of
        // the same name survives untouched — the one-state-path rule.
        let c = config("[profiles.ubuntu.window]\nopacity = 0.5\n");
        let r = resolve_profile(&c, "ubuntu");
        let (rows, _, actions) = build(&r, false);
        let row = rows
            .iter()
            .position(|r| matches!(r, SettingsRowModel::Setting { key, modified: true, .. } if key == "window.opacity"))
            .expect("the override row exists");
        let RowAction::Field(idx) = actions[row] else { panic!("a setting row maps to a field") };
        let key = &fields()[idx].key;
        assert_eq!(key, "window.opacity", "the parallel lists agree about the row");

        let path = std::env::temp_dir().join("zesterm-profiles-tab-reset-test.toml");
        std::fs::write(
            &path,
            "# my terminal\n[window]\nopacity = 0.8\n\
             [profiles.ubuntu]\n# the penguin\ncommand = \"wsl.exe\"\n\
             [profiles.ubuntu.window]\nopacity = 0.5\n",
        )
        .expect("write");
        zest_config::remove_profile_value(&path, "ubuntu", key).expect("reset");
        let text = std::fs::read_to_string(&path).expect("read");
        let table: toml::Table = text.parse().expect("still valid toml");
        assert_eq!(
            table["window"]["opacity"].as_float(),
            Some(0.8),
            "the ROOT key must survive a profile reset: {text}"
        );
        assert!(
            table["profiles"]["ubuntu"].get("window").is_none(),
            "the profile's override is gone (and its emptied table pruned): {text}"
        );
        assert!(text.contains("# my terminal"), "comments elsewhere survive: {text}");
        assert!(text.contains("# the penguin"), "the profile's own comments too: {text}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn duplicate_and_delete_round_trip_with_unique_names() {
        let existing = vec!["ubuntu".to_string(), "ubuntu-2".to_string()];
        assert_eq!(copy_name(&existing, "ubuntu"), "ubuntu-3", "suffixes skip what exists");
        assert_eq!(new_profile_name(&existing), "new-profile-1");
        assert_eq!(
            new_profile_name(&["new-profile-1".to_string()]),
            "new-profile-2",
            "N grows past what exists"
        );

        let path = std::env::temp_dir().join("zesterm-profiles-tab-dup-test.toml");
        std::fs::write(&path, "[profiles.ubuntu]\ncommand = \"wsl.exe\"\n").expect("write");
        zest_config::copy_profile(&path, "ubuntu", "ubuntu-2").expect("duplicate");
        zest_config::create_profile(&path, "new-profile-1").expect("new");
        zest_config::remove_profile(&path, "ubuntu").expect("delete");
        let table: toml::Table =
            std::fs::read_to_string(&path).expect("read").parse().expect("valid");
        let profiles = table["profiles"].as_table().expect("table");
        assert!(
            !profiles.contains_key("ubuntu")
                && profiles.contains_key("ubuntu-2")
                && profiles.contains_key("new-profile-1"),
            "duplicate + new + delete round-trips: {profiles:?}"
        );
        assert_eq!(
            profiles["ubuntu-2"]["command"].as_str(),
            Some("wsl.exe"),
            "the duplicate carries its source's keys"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn defaults_has_no_digit_and_the_fall_through_sentence() {
        let mut settings = Settings::default();
        settings.profiles.insert("ubuntu".into(), toml::Table::new());
        settings.profiles.insert("defaults".into(), toml::Table::new());
        let rail = build_rail(&settings, "sh", "studio");
        assert_eq!(rail[0].name, "Defaults", "the parent leads the rail");
        assert_eq!(rail[0].digit, None, "Defaults is not a jump target");
        assert_eq!(rail[1].digit, Some(1), "profiles number from 1, like the launcher");
        assert_eq!(
            rail_names(&settings),
            vec!["defaults".to_string(), "ubuntu".to_string()],
            "rail_names is the rail's index-parallel truth"
        );
        assert_eq!(
            footer_sentence(true, 3),
            "Every profile falls through to this one",
            "Defaults' footer is the fall-through sentence, whatever it sets"
        );
        assert_eq!(footer_sentence(false, 1), "1 setting overrides Defaults", "singular");
        assert_eq!(footer_sentence(false, 6), "6 settings override Defaults");
    }

    #[test]
    fn the_preview_carries_window_chrome_and_profile_scheme_as_data() {
        // §12's split: the chip stays the window's theme (the caption says
        // so, naming it), only the body block is the profile's scheme.
        let c = config("[profiles.mac]\ncolor_scheme = \"nord\"\ntab_color = 3\n");
        let r = resolve_profile(&c, "mac");
        let p = build_preview("mac", &r, "obsidian", "Darwin 24.5.0");
        let nord = zest_theme::resolve(&zest_theme::builtin::get("nord").expect("builtin"));
        assert_eq!(
            p.scheme_bg,
            [nord.background.r, nord.background.g, nord.background.b],
            "the body is the profile's scheme, not the window's"
        );
        assert_eq!(
            p.caption,
            "Chrome is the window's theme (obsidian). Only the grid follows this \
             profile's scheme.",
            "the §12 caption, verbatim, naming the window's theme"
        );
        assert_eq!(p.accent, AccentChoice::Profile(3), "the chip's rule is the profile accent");
        assert!(!p.lines.is_empty(), "static content, so the block reads as a terminal");

        // No scheme (or a deleted one): the preview follows the window,
        // exactly as the grid will.
        let r = resolve_profile(&toml::Table::new(), "bare");
        let p = build_preview("bare", &r, "obsidian", "os");
        let obsidian = zest_theme::resolve(&zest_theme::builtin::obsidian());
        assert_eq!(
            p.scheme_bg,
            [obsidian.background.r, obsidian.background.g, obsidian.background.b]
        );
    }

    #[test]
    fn accent_of_matches_the_chips_truth_table() {
        // One truth table (§12): this helper must agree with
        // chrome::model::tab_accent for every combination, or the preview
        // and the real chip diverge.
        use crate::chrome::model::tab_accent;
        use crate::tabs::ProfileIdentity;
        let cases = [
            (None, None),
            (None, Some(3)),
            (Some(ColorFrom::Profile), Some(3)),
            (Some(ColorFrom::Profile), None),
            (Some(ColorFrom::Host), Some(3)),
        ];
        for (from, color) in cases {
            let meta = ProfileMeta { color_from: from, tab_color: color, ..Default::default() };
            let identity = ProfileIdentity {
                name: "t".into(),
                scheme: None,
                selection_bg: None,
                tab_color: color,
                icon: None,
                color_from: from,
                opacity: None,
                background_image: None,
                background_fit: None,
                background_dim: None,
                title: TabTitle::FromShell,
            };
            assert_eq!(
                accent_of(&meta),
                tab_accent(Some(&identity), 0),
                "color_from={from:?} tab_color={color:?}"
            );
        }
    }

    #[test]
    fn swatch_chips_are_the_builtin_ansi_rows_in_index_order() {
        // The design reads the strips from zest-theme; re-typing them is the
        // drift this forbids. Every builtin, every index, via resolve().
        let swatches = scheme_swatches();
        let all = zest_theme::builtin::all();
        assert_eq!(swatches.len(), all.len(), "one chip per builtin");
        for (swatch, theme) in swatches.iter().zip(&all) {
            assert_eq!(swatch.id, theme.id);
            let resolved = zest_theme::resolve(theme);
            for i in 0..8 {
                let c = resolved.colors[i];
                assert_eq!(
                    swatch.ansi[i],
                    [c.r, c.g, c.b],
                    "{}[{i}] must be the resolved ANSI colour",
                    theme.id
                );
            }
        }
    }

    #[test]
    fn adjust_and_choice_speak_the_same_values() {
        let all = fields();
        let field = |key: &str| all.iter().find(|f| f.key == key).expect("field").clone();
        let schemes = scheme_swatches();
        let themes: Vec<String> = schemes.iter().map(|s| s.id.clone()).collect();

        let accent = field("tab_color");
        assert_eq!(
            adjust_profile(&accent, &serde_json::Value::Null, 1, &themes, &[]),
            Some(serde_json::json!(0)),
            "from unset the first press lands on the first swatch"
        );
        assert_eq!(
            adjust_profile(&accent, &serde_json::json!(5), 1, &themes, &[]),
            Some(serde_json::json!(0)),
            "the six swatches wrap"
        );
        assert_eq!(
            choice_value(&accent, 3, &schemes),
            Some(serde_json::json!(3)),
            "a swatch click writes its index"
        );
        assert_eq!(choice_value(&accent, 6, &schemes), None, "past the roster refuses");

        let scheme = field("color_scheme");
        assert_eq!(
            choice_value(&scheme, 1, &schemes),
            Some(serde_json::Value::String(schemes[1].id.clone())),
            "a scheme chip writes its id"
        );
        let icon = field("icon");
        assert_eq!(
            choice_value(&icon, 0, &schemes),
            Some(serde_json::Value::String(ICONS[0].to_string()))
        );
        // And every written value survives to_toml — the write path the app
        // funnels these through.
        for (f, v) in [
            (&accent, serde_json::json!(3)),
            (&scheme, serde_json::Value::String("nord".into())),
            (&icon, serde_json::Value::String(ICONS[0].to_string())),
        ] {
            assert!(
                settings_ui::to_toml(f, &v).is_some(),
                "'{}' must be writable through the one write path",
                f.key
            );
        }
    }

    #[test]
    fn filtering_keeps_section_rules_only_for_surviving_rows() {
        let r = resolve_profile(&toml::Table::new(), "x");
        let values = window_values();
        let schemes = scheme_swatches();
        let (rows, chips, actions) = build_profile_rows(
            &fields(),
            &r,
            &ctx(&values, &schemes, false),
            "cursor",
            None,
            None,
        );
        assert_eq!(rows.len(), chips.len());
        assert_eq!(rows.len(), actions.len());
        let groups: Vec<&str> = rows
            .iter()
            .filter_map(|r| match r {
                SettingsRowModel::Group { title } => Some(title.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(groups, ["Cursor"], "empty sections drop their rule under a filter");
        assert!(
            rows.iter().any(|r| matches!(r, SettingsRowModel::Setting { key, .. } if key == "cursor.shape")),
            "the matching rows survive"
        );
    }
}

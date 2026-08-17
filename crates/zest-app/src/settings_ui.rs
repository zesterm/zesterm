//! The settings tab's rows, built from the schema walk.
//!
//! Pure on purpose, like `chrome::layout`: fields, values and provenance in,
//! display rows and a parallel action list out. Nothing here touches the
//! window, the fonts or the filesystem, which is what lets the coverage test
//! hold the tab to the schema without opening a window.
//!
//! The row list and the action list are built in one pass — the picker's
//! discipline — so a drawn row and its meaning cannot drift.

use std::collections::BTreeMap;

use zest_config::ui::{UiField, Widget};
use zest_config::Source;

use crate::chrome::model::{SettingsFace, SettingsRowModel, SettingsValueCell};

/// What a settings row means to the input path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RowAction {
    /// A group header; selection skips it, Enter does nothing.
    None,
    /// Index into the `UiField` list the rows were built from.
    Field(usize),
}

/// One in-flight typed edit (a number being written out in full).
///
/// While this exists, characters belong to the buffer and not the filter —
/// the input-mode collision is resolved by construction, not by guessing.
pub struct EditBuffer {
    pub field_idx: usize,
    pub buffer: crate::text_field::TextField,
    /// The last Enter did not parse; drawn as an error until the text changes.
    pub error: bool,
    /// Enter *appends* to a list field (a new tag, a new env entry) instead
    /// of replacing the value — the add-chip's mode.
    pub append: bool,
}

/// What leaving an open edit should do (#272, #275).
#[derive(Debug, Clone, PartialEq)]
pub enum Pending {
    /// Nothing was open; the caller may leave.
    None,
    /// Write this parsed value into the field, then leave.
    Commit(usize, serde_json::Value),
    /// Append this raw text to the field's list — the add-chip's mode. Not
    /// parsed here: appending needs the list's *current* value, which lives
    /// with the writer. The buffer is deliberately left open, because only
    /// the caller can tell whether the append took.
    Append(usize, String),
    /// The buffer does not parse. It stays open, flagged, and the caller must
    /// NOT leave — a value that cannot be written must not be lost by looking
    /// away from it.
    Refused,
}

/// Take an open edit so the caller can write it.
///
/// Enter used to be the only thing that committed, so every other way out of a
/// field — clicking another row, changing category, closing the tab — dropped
/// the buffer on the floor and a typed value simply vanished (#272 on the
/// profiles editor, #275 on this one). Leaving a field is a commit; only Esc
/// discards, and only Esc should.
///
/// Returns the value rather than writing it because writing needs the config
/// path and a reload, which live on `App`. Keeping the decision here is what
/// lets it be tested without a window.
pub fn take_pending_edit(editing: &mut Option<EditBuffer>, fields: &[UiField]) -> Pending {
    let Some(edit) = editing.as_mut() else { return Pending::None };
    let idx = edit.field_idx;
    // A buffer whose field index no longer names a field cannot be written and
    // cannot be shown; keeping it open would trap the caller for ever.
    let Some(field) = fields.get(idx) else {
        *editing = None;
        return Pending::None;
    };
    if edit.append {
        return Pending::Append(idx, edit.buffer.text().to_string());
    }
    match parse_input(field, edit.buffer.text()) {
        Some(value) => {
            *editing = None;
            Pending::Commit(idx, value)
        }
        None => {
            edit.error = true;
            Pending::Refused
        }
    }
}

/// Settings the schema declares but the app does not consume yet.
///
/// Honesty over polish: a control that visibly does nothing reads as broken,
/// so these rows carry a faint "not applied yet" tag instead of pretending.
/// Wiring one up includes deleting its entry here — the test below only
/// keeps this list from naming keys that do not exist at all.
pub const NOT_YET_WIRED: &[&str] = &[
    "cursor.shape",
    "cursor.trail",
    "motion.enabled",
    "motion.respect_system_reduce_motion",
    "motion.smooth_scroll",
    "motion.spring_damping",
    "motion.spring_response",
    "typography.features",
    "typography.ligatures",
    "window.columns",
    "window.rows",
];

/// Display order of the groups. The schema's alphabetical property order is
/// an artifact; this is the order a person tunes a terminal in. Groups the
/// list does not name append at the end rather than vanishing.
pub const GROUP_ORDER: &[&str] =
    &["Text", "Appearance", "Window", "Tabs", "Shell", "Scrolling", "Cursor", "Motion"];

/// The fixed ninth category (§11): the one the schema cannot express.
pub const UNKNOWN_CATEGORY: &str = "Unknown keys";

/// Every category, in rail order: `GROUP_ORDER`, any group the schema grew
/// that the list does not name (appended rather than vanishing), then
/// "Unknown keys".
#[must_use]
pub fn categories(fields: &[UiField]) -> Vec<String> {
    let mut out: Vec<String> = GROUP_ORDER.iter().map(|g| (*g).to_string()).collect();
    for field in fields {
        if !out.iter().any(|g| g == &field.group) {
            out.push(field.group.clone());
        }
    }
    out.push(UNKNOWN_CATEGORY.to_string());
    out
}

/// The category's dotted prefix — the settings-tree branch its fields live
/// under ("typography" for Text), drawn in mono beside the heading. Empty
/// when the group's keys do not share one (or the group has no fields).
#[must_use]
pub fn category_prefix(fields: &[UiField], group: &str) -> String {
    let mut prefix: Option<&str> = None;
    for field in fields.iter().filter(|f| f.group == group) {
        let head = field.key.split('.').next().unwrap_or(&field.key);
        match prefix {
            None => prefix = Some(head),
            Some(p) if p == head => {}
            Some(_) => return String::new(),
        }
    }
    prefix.unwrap_or_default().to_string()
}

/// The category ledes — designed copy (docs/design/client-ui, §11's mock),
/// keyed by group name. A group the design never met gets an honest generic.
#[must_use]
pub fn category_lede(group: &str) -> &'static str {
    match group {
        "Text" => "Font stack, size, and the cell geometry that follows from them.",
        "Appearance" => {
            "Which theme, and how glyph coverage is sampled before it reaches the frame."
        }
        "Window" => "Shape, transparency, and who draws the titlebar.",
        "Tabs" => "Where the strip lives and what comes back on the next launch.",
        "Shell" => "What runs, where it runs, and the environment it inherits.",
        "Scrolling" => "How much history is kept and what moves the view.",
        "Cursor" => "Shape, blink, and motion between cells.",
        "Motion" => "Animation, and how much of it.",
        UNKNOWN_CATEGORY => "Keys your files set that this schema does not know.",
        _ => "Settings the schema added after this build's rail was written.",
    }
}

/// Fields of `group` that differ from their schema defaults — the rail's
/// per-category count. `values` must already be f32-narrowed (the callers
/// below do it once for every group).
fn modified_in(fields: &[UiField], values: &serde_json::Value, group: &str) -> usize {
    fields
        .iter()
        .filter(|f| f.group == group)
        .filter(|f| {
            zest_config::ui::value_at(values, &f.key).is_some_and(|v| *v != f.default)
        })
        .count()
}

/// Per-category modified counts plus the total, one pass — the rail's
/// badges and the footer's sentence must agree by construction.
#[must_use]
pub fn modified_counts(
    fields: &[UiField],
    values: &serde_json::Value,
    groups: &[String],
) -> (Vec<usize>, usize) {
    let mut normalized = values.clone();
    zest_config::schema::narrow_f32_literals(&mut normalized);
    let counts: Vec<usize> =
        groups.iter().map(|g| modified_in(fields, &normalized, g)).collect();
    let total = counts.iter().sum();
    (counts, total)
}

/// Whether a category has anything to show under `filter` — what lets the
/// rail hide empty categories while a filter is live (§11).
#[must_use]
pub fn category_matches(
    fields: &[UiField],
    group: &str,
    filter: &str,
    unknown_keys: &[String],
) -> bool {
    let filter = filter.to_lowercase();
    if group == UNKNOWN_CATEGORY {
        return unknown_keys.iter().any(|k| k.to_lowercase().contains(&filter));
    }
    fields.iter().any(|f| f.group == group && matches_filter(f, &filter))
}

/// Build one category's rows and their actions, one pass — no group headers;
/// the rail is the grouping now (§11). Banners pin to the top, above any
/// filter's result: a write that failed or a restart owed is true whatever
/// the user is searching for.
#[must_use]
#[allow(clippy::too_many_arguments, reason = "one build, one pass; a params struct would be ceremony")]
pub fn build_category_rows(
    fields: &[UiField],
    values: &serde_json::Value,
    provenance: &BTreeMap<String, Source>,
    group: &str,
    filter: &str,
    editing: Option<&EditBuffer>,
    restart_pending: &std::collections::BTreeSet<String>,
    error: Option<&str>,
    installed: &[String],
) -> (Vec<SettingsRowModel>, Vec<RowAction>) {
    // Both sides of the "modified" comparison have to be spelled the same way.
    // Every float in `Settings` is an `f32` and JSON has only `f64`, so a live
    // `1.3f32` serializes as `1.2999999523162842` while the schema default is
    // the shortest exact spelling. Comparing them raw marks every untouched
    // float as edited, which is a dot next to a setting nobody changed.
    let mut normalized = values.clone();
    zest_config::schema::narrow_f32_literals(&mut normalized);
    let values = &normalized;

    let filter = filter.to_lowercase();
    let mut rows = Vec::new();
    let mut actions = Vec::new();

    if let Some(error) = error {
        rows.push(SettingsRowModel::Notice { text: error.to_string() });
        actions.push(RowAction::None);
    }
    if !restart_pending.is_empty() {
        let text = if restart_pending.len() == 1 {
            format!("{} applies on the next launch", restart_pending.iter().next().expect("len 1"))
        } else {
            format!("{} changed settings apply on the next launch", restart_pending.len())
        };
        rows.push(SettingsRowModel::Notice { text });
        actions.push(RowAction::None);
    }

    for (index, field) in fields
        .iter()
        .enumerate()
        .filter(|(_, f)| f.group == group && matches_filter(f, &filter))
    {
        let editing = editing.filter(|e| e.field_idx == index);
        rows.push(setting_row(field, values, provenance, editing, installed));
        actions.push(RowAction::Field(index));
    }
    (rows, actions)
}

/// The unknown-keys category's rows: the key in mono, which layer set it,
/// and a suggestion when a schema key is within a plausible-typo distance.
#[must_use]
pub fn build_unknown_rows(
    unknown_keys: &[String],
    provenance: &BTreeMap<String, Source>,
    filter: &str,
    schema_keys: &[String],
) -> (Vec<SettingsRowModel>, Vec<RowAction>) {
    let filter = filter.to_lowercase();
    let mut rows = Vec::new();
    let mut actions = Vec::new();
    for key in unknown_keys {
        if !key.to_lowercase().contains(&filter) {
            continue;
        }
        rows.push(SettingsRowModel::Unknown {
            key: key.clone(),
            // The cascade records provenance for every leaf it merged,
            // unknown ones included — that is where "which file did this"
            // comes from.
            source: provenance
                .get(key)
                .map_or_else(|| "config file".to_string(), std::string::ToString::to_string),
            suggestion: suggest_key(key, schema_keys),
        });
        actions.push(RowAction::None);
    }
    (rows, actions)
}

/// Build every category's rows with headers — the whole-schema view the
/// coverage tests hold to the schema. The tab itself renders one category at
/// a time via [`build_category_rows`]; this is that, concatenated, so the
/// two cannot disagree about which rows exist.
#[allow(dead_code, reason = "the coverage tests' whole-schema view; production renders per category")]
#[must_use]
pub fn build_rows(
    fields: &[UiField],
    values: &serde_json::Value,
    provenance: &BTreeMap<String, Source>,
    filter: &str,
    editing: Option<&EditBuffer>,
    restart_pending: &std::collections::BTreeSet<String>,
    error: Option<&str>,
) -> (Vec<SettingsRowModel>, Vec<RowAction>) {
    let mut rows = Vec::new();
    let mut actions = Vec::new();
    let mut first = true;
    for group in categories(fields) {
        if group == UNKNOWN_CATEGORY {
            continue;
        }
        // Banners belong to the panel, not to a group: only the first
        // category carries them, or they would repeat eight times.
        let (banner_pending, banner_error) = if first {
            (restart_pending.clone(), error)
        } else {
            (std::collections::BTreeSet::new(), None)
        };
        let (mut group_rows, group_actions) = build_category_rows(
            fields,
            values,
            provenance,
            &group,
            filter,
            editing,
            &banner_pending,
            banner_error,
            &[],
        );
        first = false;
        let banners =
            group_rows.iter().take_while(|r| matches!(r, SettingsRowModel::Notice { .. })).count();
        if group_rows.len() > banners {
            rows.extend(group_rows.drain(..banners));
            actions.extend(group_actions[..banners].iter().copied());
            rows.push(SettingsRowModel::Group { title: group.clone() });
            actions.push(RowAction::None);
            rows.extend(group_rows);
            actions.extend(group_actions[banners..].iter().copied());
        } else {
            // Only banners survived the filter for this group: keep them
            // (they are pinned truth), drop the empty header.
            rows.extend(group_rows);
            actions.extend(group_actions);
        }
    }
    (rows, actions)
}

fn matches_filter(field: &UiField, filter: &str) -> bool {
    filter.is_empty()
        || field.key.to_lowercase().contains(filter)
        || field.group.to_lowercase().contains(filter)
        || humanize_key(&field.key).to_lowercase().contains(filter)
        || field.description.to_lowercase().contains(filter)
}

fn setting_row(
    field: &UiField,
    values: &serde_json::Value,
    provenance: &BTreeMap<String, Source>,
    editing: Option<&EditBuffer>,
    installed: &[String],
) -> SettingsRowModel {
    let value = zest_config::ui::value_at(values, &field.key);
    // Warn when the winning layer outranks the user's file: an edit written
    // there is correct — it applies when the stronger layer goes away — but
    // the visible value will not move, and without the chip that reads as
    // "settings are broken" instead of "the profile is winning".
    let provenance = provenance.get(&field.key).map(|source| {
        (format!("set by {source}"), *source > Source::User)
    });
    let cell = match editing {
        Some(edit) => SettingsValueCell::Editing {
            buffer: edit.buffer.text().to_string(),
            caret: edit.buffer.caret(),
            selection: edit.buffer.selection(),
            error: edit.error,
        },
        None => value_cell(field, value, installed),
    };
    SettingsRowModel::Setting {
        label: humanize_key(&field.key),
        key: field.key.clone(),
        description: first_line(&field.description),
        value: cell,
        provenance,
        restart: zest_config::invalidate::class_of(&field.key) == zest_config::Invalidation::Restart,
        inert: NOT_YET_WIRED.contains(&field.key.as_str()),
        modified: value.is_some_and(|v| *v != field.default),
    }
}

/// A select is the segmented control when its variants are ≤3 (§11);
/// otherwise the 288px dropdown, whose rows have room for the doc comments —
/// `window.backdrop` is the case that earns it. Deliberately *not* "or
/// documented ones": the mock draws two-variant documented selects
/// (Antialiasing) segmented, and the reference screenshot is the tiebreaker.
#[must_use]
pub fn select_is_segmented(field: &UiField) -> bool {
    field.widget == Widget::Select && field.variants.len() <= 3
}

/// `from-shell` → `From shell`: a variant's wire value as a label.
#[must_use]
pub fn humanize_value(value: &str) -> String {
    let spaced = value.replace('-', " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => spaced,
    }
}

/// The unit a stepper's value carries (§11: "14 pt", "530 ms", "1.25 ×").
/// From the key's spelling — the schema does not carry units, and the design
/// writes them next to exactly these shapes of key.
#[must_use]
pub fn unit_of(key: &str) -> &'static str {
    let last = key.rsplit('.').next().unwrap_or(key);
    if last.ends_with("_pt") {
        "pt"
    } else if last.ends_with("_ms") {
        "ms"
    } else if last == "line_height" {
        "×"
    } else if last == "padding" {
        "px"
    } else {
        ""
    }
}

pub(crate) fn value_cell(
    field: &UiField,
    value: Option<&serde_json::Value>,
    installed: &[String],
) -> SettingsValueCell {
    let text_of = |v: Option<&serde_json::Value>| match v {
        Some(serde_json::Value::String(s)) if !s.is_empty() => s.clone(),
        Some(serde_json::Value::String(_)) | None => "—".to_string(),
        Some(other) => format_scalar(other),
    };
    match field.widget {
        Widget::Toggle => SettingsValueCell::Toggle {
            on: value.and_then(serde_json::Value::as_bool).unwrap_or(false),
        },
        Widget::Select if select_is_segmented(field) => {
            let current = value.and_then(serde_json::Value::as_str).unwrap_or_default();
            SettingsValueCell::Segmented {
                options: field.variants.iter().map(|v| humanize_value(&v.value)).collect(),
                selected: field.variants.iter().position(|v| v.value == current),
            }
        }
        // The profile pickers (#130) are string-valued choices from a roster
        // the client brings, exactly like ThemePicker; AccentPicker is the
        // integer-valued exception and lands with the plain-text widgets.
        Widget::Select
        | Widget::ThemePicker
        | Widget::HostPicker
        | Widget::SchemePicker
        | Widget::IconPicker => SettingsValueCell::Select { value: text_of(value) },
        Widget::Slider => {
            let v = value.and_then(serde_json::Value::as_f64).unwrap_or(0.0);
            let frac = field
                .range
                .map(|(min, max)| {
                    if max > min { ((v - min) / (max - min)).clamp(0.0, 1.0) } else { 0.0 }
                })
                .unwrap_or(0.0);
            #[allow(clippy::cast_possible_truncation)] // display fraction, not data
            SettingsValueCell::Slider { frac: frac as f32, text: format_number(v) }
        }
        Widget::Number => {
            let unit = unit_of(&field.key);
            let text = match (value, unit) {
                (None, _) => "—".to_string(),
                (Some(v), "") => format_scalar(v),
                (Some(v), unit) => format!("{} {unit}", format_scalar(v)),
            };
            SettingsValueCell::Stepper { text }
        }
        Widget::Text | Widget::Path | Widget::AccentPicker => {
            // The settings tab shows resolved values, never captions, so
            // nothing here is a placeholder; the profiles editor sets the
            // flag where a fallback stands in for unset.
            SettingsValueCell::Text { text: text_of(value), placeholder: false }
        }
        Widget::FontList => {
            let faces: Vec<String> = value
                .and_then(serde_json::Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            // Rows past the first *resolvable* face are fallback (§11): the
            // first that resolves is the one the atlas actually shapes with.
            let first_resolvable = faces.iter().position(|f| {
                installed.iter().any(|i| i.eq_ignore_ascii_case(f))
            });
            SettingsValueCell::FontList {
                faces: faces
                    .into_iter()
                    .enumerate()
                    .map(|(i, family)| SettingsFace {
                        family,
                        fallback: first_resolvable.is_some_and(|first| i > first),
                    })
                    .collect(),
            }
        }
        Widget::TagList => SettingsValueCell::TagList {
            tags: value
                .and_then(serde_json::Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(serde_json::Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default(),
        },
        Widget::KeyValue => SettingsValueCell::KeyValue {
            entries: value
                .and_then(serde_json::Value::as_object)
                .map(|map| {
                    map.iter()
                        .map(|(k, v)| (k.clone(), v.as_str().unwrap_or_default().to_string()))
                        .collect()
                })
                .unwrap_or_default(),
        },
    }
}

fn format_scalar(value: &serde_json::Value) -> String {
    match value {
        // Integers first: a 10_000_000 scrollback must not round through f32.
        serde_json::Value::Number(n) if n.is_i64() || n.is_u64() => n.to_string(),
        serde_json::Value::Number(n) => n.as_f64().map_or_else(|| n.to_string(), format_number),
        other => other.to_string(),
    }
}

/// `13`, `0.16`, `1.25` — never `0.1599999964237213`.
///
/// Settings floats are `f32` and serde_json widens them noisily; the
/// committed schema's own `spring_response` default shows the damage.
/// Narrowing back to `f32` before Display recovers the shortest form the
/// user actually wrote, losslessly, because `f32` is what the value was.
fn format_number(v: f64) -> String {
    #[allow(clippy::cast_possible_truncation)]
    let narrowed = v as f32;
    format!("{narrowed}")
}

/// The value an arrow key or Enter produces on a field: flip a toggle,
/// cycle a select (wrapping), step a number (clamped to the schema range).
///
/// `themes` and `fonts` are the rosters for [`Widget::ThemePicker`] and
/// [`Widget::FontList`] — the schema cannot know either, so the caller brings
/// them.
#[must_use]
pub fn adjust(
    field: &UiField,
    current: &serde_json::Value,
    dir: i32,
    themes: &[String],
    fonts: &[String],
) -> Option<serde_json::Value> {
    let cycle = |options: &[String]| -> Option<serde_json::Value> {
        if options.is_empty() {
            return None;
        }
        let current = current.as_str().unwrap_or_default();
        let at = options.iter().position(|o| o == current).unwrap_or(0);
        let len = options.len() as i32;
        let next = (at as i32 + dir).rem_euclid(len) as usize;
        Some(serde_json::Value::String(options[next].clone()))
    };
    match field.widget {
        Widget::Toggle => {
            Some(serde_json::Value::Bool(!current.as_bool().unwrap_or(false)))
        }
        Widget::Select => {
            let options: Vec<String> =
                field.variants.iter().map(|v| v.value.clone()).collect();
            cycle(&options)
        }
        Widget::ThemePicker => cycle(themes),
        Widget::FontList => {
            // One entry, replacing whatever was there.
            //
            // The list is a *preference order*, not a coverage mechanism: a
            // character the chosen face lacks is resolved per-character by
            // fontique against the system (DirectWrite, CoreText, fontconfig),
            // Nerd Font icons come from `symbol_families`, and emoji have their
            // own path. None of that consults this list. The shipped default is
            // several entries only because it has to name a face that exists on
            // Windows *and* macOS *and* Linux before it knows which it is on.
            //
            // Once someone has chosen, that reason is gone, and keeping a tail
            // is how the picker used to grow one: it appended on every press,
            // so a single pass across an installed set left ninety families
            // behind, Curlz MT among them.
            if fonts.is_empty() {
                return None;
            }
            let current = current
                .as_array()
                .and_then(|a| a.first())
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let at = fonts.iter().position(|f| f == current).unwrap_or(0);
            let next = (at as i32 + dir).rem_euclid(fonts.len() as i32) as usize;
            Some(serde_json::Value::Array(vec![serde_json::Value::String(
                fonts[next].clone(),
            )]))
        }
        Widget::Number | Widget::Slider if field.integer => {
            let (min, max) = field
                .range
                .map_or((i64::MIN, i64::MAX), |(a, b)| (a as i64, b as i64));
            let next = current
                .as_i64()
                .unwrap_or(0)
                .saturating_add(i64::from(dir))
                .clamp(min, max);
            Some(serde_json::json!(next))
        }
        Widget::Number | Widget::Slider => {
            let (min, max) = field.range?;
            let span = max - min;
            // A slider is proportional (a twentieth of its travel); a number
            // steps in units a person would type: 1pt of font size, 0.1 of
            // line height, 0.05 of a 0..1 knob.
            let step = if field.widget == Widget::Slider {
                span / 20.0
            } else if span >= 50.0 {
                1.0
            } else if span >= 2.0 {
                0.1
            } else {
                0.05
            };
            let current = current.as_f64().unwrap_or(min);
            // To the *next grid point* in the direction of travel, not
            // current±step: repeated presses cannot accumulate binary noise
            // (every result is on the grid), and an off-grid start moves by
            // less than a step instead of jumping past its neighbour.
            let steps = (current - min) / step;
            let n = if dir > 0 { (steps + 1e-6).floor() + 1.0 } else { (steps - 1e-6).ceil() - 1.0 };
            let next = (min + n * step).clamp(min, max);
            Some(serde_json::json!(clean_float(next)))
        }
        _ => None,
    }
}

/// Typed input committed from an [`EditBuffer`], clamped to the schema range.
/// `None` is a parse failure the caller shows as an error, not a silent drop.
#[must_use]
pub fn parse_input(field: &UiField, text: &str) -> Option<serde_json::Value> {
    let text = text.trim();
    if field.integer {
        let (min, max) =
            field.range.map_or((i64::MIN, i64::MAX), |(a, b)| (a as i64, b as i64));
        return Some(serde_json::json!(text.parse::<i64>().ok()?.clamp(min, max)));
    }
    match field.widget {
        Widget::Number | Widget::Slider => {
            let v = text.parse::<f64>().ok()?;
            let clamped = field.range.map_or(v, |(min, max)| v.clamp(min, max));
            Some(serde_json::json!(clean_float(clamped)))
        }
        _ => Some(serde_json::Value::String(text.to_string())),
    }
}

/// What the edit buffer opens with: the current value, spelled the way the
/// user would type it back.
#[must_use]
pub fn edit_seed(field: &UiField, value: Option<&serde_json::Value>) -> String {
    match (field.widget, value) {
        (_, None) => String::new(),
        (Widget::Text | Widget::Path, Some(v)) => v.as_str().unwrap_or_default().to_string(),
        (_, Some(v)) if field.integer => {
            v.as_i64().map_or_else(String::new, |i| i.to_string())
        }
        (_, Some(v)) => v.as_f64().map_or_else(String::new, format_number),
    }
}

/// A slider position (0..1 along the drawn track) as the value to write,
/// quantized to the same twentieth-of-travel grid the arrow keys use — a
/// drag and a keypress must never disagree about which values exist.
#[must_use]
pub fn slider_value(field: &UiField, frac: f64) -> Option<serde_json::Value> {
    let (min, max) = field.range?;
    let frac = frac.clamp(0.0, 1.0);
    if field.integer {
        let v = min + frac * (max - min);
        return Some(serde_json::json!(v.round() as i64));
    }
    let steps = (frac * 20.0).round();
    Some(serde_json::json!(clean_float(min + steps * (max - min) / 20.0)))
}

/// A committed value as the TOML to write, or `None` when the value's shape
/// does not fit the field (a caller bug, refused rather than mis-written).
#[must_use]
pub fn to_toml(
    field: &UiField,
    value: &serde_json::Value,
) -> Option<zest_config::toml_edit::Value> {
    match field.widget {
        Widget::Toggle => Some(zest_config::toml_edit::Value::from(value.as_bool()?)),
        // The profile pickers (#130) write strings, except the accent, which
        // is an index into the theme's accents.
        Widget::Select
        | Widget::ThemePicker
        | Widget::Text
        | Widget::Path
        | Widget::HostPicker
        | Widget::SchemePicker
        | Widget::IconPicker => Some(zest_config::toml_edit::Value::from(value.as_str()?)),
        Widget::AccentPicker => Some(zest_config::toml_edit::Value::from(value.as_i64()?)),
        Widget::Number | Widget::Slider if field.integer => {
            Some(zest_config::toml_edit::Value::from(value.as_i64()?))
        }
        Widget::Number | Widget::Slider => {
            Some(zest_config::toml_edit::Value::from(clean_float(value.as_f64()?)))
        }
        Widget::FontList | Widget::TagList => {
            // The whole list, in order — for fonts, writing only the primary
            // would drop the fallbacks and turn a missing glyph into tofu;
            // for tags, order is what the user wrote.
            let mut arr = zest_config::toml_edit::Array::new();
            for f in value.as_array()?.iter().filter_map(serde_json::Value::as_str) {
                arr.push(f);
            }
            Some(zest_config::toml_edit::Value::Array(arr))
        }
        Widget::KeyValue => {
            // An inline table, entries in the map's order. Empty values are
            // written, not skipped: empty *unsets* the variable under the
            // wholesale-replace merge (`shell.env` replaces, cascade.rs).
            let mut table = zest_config::toml_edit::InlineTable::new();
            for (k, v) in value.as_object()? {
                table.insert(k, v.as_str().unwrap_or_default().into());
            }
            Some(zest_config::toml_edit::Value::InlineTable(table))
        }
    }
}

/// Plain Levenshtein edit distance, for the unknown-keys suggestions. Small
/// and in-crate on purpose: two dotted keys are ~25 characters, a config has
/// a handful of unknowns, and a dependency for this would be all cost.
#[must_use]
pub fn levenshtein(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0; b.len() + 1];
    for (i, ca) in a.iter().enumerate() {
        cur[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            cur[j + 1] = (prev[j + 1] + 1).min(cur[j] + 1).min(prev[j] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// "did you mean `size_pt`?" — the nearest schema key when the distance is
/// small enough to read as a typo (≤ 2 edits), else nothing: a stretch
/// suggestion is worse than none.
#[must_use]
pub fn suggest_key(unknown: &str, schema_keys: &[String]) -> Option<String> {
    schema_keys
        .iter()
        .map(|k| (levenshtein(unknown, k), k))
        .filter(|(d, _)| *d <= 2)
        .min_by_key(|(d, _)| *d)
        .map(|(_, k)| k.clone())
}

/// The f64 that *displays* as the f32 the user meant.
///
/// The settings floats are f32; anything that went through serde_json is
/// carrying widening noise (0.1599999964237213). Narrow, print shortest,
/// parse back — the result formats as `0.16` everywhere downstream,
/// including `toml_edit`, which writes exactly what Display says.
fn clean_float(v: f64) -> f64 {
    #[allow(clippy::cast_possible_truncation)]
    let narrowed = v as f32;
    format!("{narrowed}").parse().unwrap_or(v)
}

/// `typography.size_pt` → `Size pt`: the last segment, spaces for
/// underscores, one capital. The description underneath carries the real
/// explanation; this is just a name that does not look like an identifier.
#[must_use]
pub fn humanize_key(key: &str) -> String {
    let last = key.rsplit('.').next().unwrap_or(key).replace('_', " ");
    let mut chars = last.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
        None => last,
    }
}

/// The doc comment's first line — the summary sentence, per the schema's own
/// convention; the full text would not fit a row.
pub(crate) fn first_line(description: &str) -> String {
    description.lines().next().unwrap_or_default().to_string()
}

/// The nearest selectable row at or after `from`, wrapping backwards —
/// group headers are labels, and the keyboard must never rest on one.
#[must_use]
pub fn nearest_field(actions: &[RowAction], from: usize) -> usize {
    let is_field = |i: &usize| matches!(actions.get(*i), Some(RowAction::Field(_)));
    (from..actions.len())
        .find(is_field)
        .or_else(|| (0..from).rev().find(is_field))
        .unwrap_or(0)
}

/// The next selectable row in `dir` from `from`, or `from` at the edge.
#[must_use]
pub fn step_selection(actions: &[RowAction], from: usize, down: bool) -> usize {
    let is_field = |i: &usize| matches!(actions.get(*i), Some(RowAction::Field(_)));
    if down {
        (from + 1..actions.len()).find(is_field).unwrap_or(from)
    } else {
        (0..from).rev().find(is_field).unwrap_or(from)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fields() -> Vec<UiField> {
        zest_config::ui::fields()
    }

    fn values() -> serde_json::Value {
        serde_json::to_value(zest_config::Settings::default()).expect("serializes")
    }

    fn field_index(key: &str) -> usize {
        fields().iter().position(|f| f.key == key).unwrap_or_else(|| panic!("{key} is a field"))
    }

    fn open(field_idx: usize, text: &str, append: bool) -> Option<EditBuffer> {
        Some(EditBuffer {
            field_idx,
            buffer: crate::text_field::TextField::new(text),
            error: false,
            append,
        })
    }

    #[test]
    fn leaving_a_settings_field_hands_the_edit_back() {
        // #275, the Settings tab's half of #272: Enter was the only exit that
        // wrote, so changing category or closing the tab dropped the buffer.
        let fields = fields();
        let idx = field_index("typography.size_pt");
        let mut editing = open(idx, "18", false);
        assert_eq!(
            take_pending_edit(&mut editing, &fields),
            Pending::Commit(idx, serde_json::json!(18.0)),
            "leaving the field must hand the value back to be written"
        );
        assert!(editing.is_none(), "a committed buffer closes");
    }

    #[test]
    fn a_settings_buffer_that_cannot_be_written_refuses_to_be_left() {
        let fields = fields();
        let idx = field_index("typography.size_pt");
        let mut editing = open(idx, "not a number", false);
        assert_eq!(take_pending_edit(&mut editing, &fields), Pending::Refused);
        let edit = editing.as_ref().expect("the buffer stays open");
        assert!(edit.error, "and says so");
        assert_eq!(edit.buffer.text(), "not a number", "with the text intact");
    }

    #[test]
    fn an_add_chip_buffer_comes_back_unparsed_and_still_open() {
        // The add-chip appends to a list, and appending needs the list's
        // CURRENT value — which lives with the writer, not here. So this arm
        // hands back the raw text and deliberately leaves the buffer open:
        // only the caller can tell whether the append took, and closing it
        // first would destroy the text on the failing half.
        let fields = fields();
        let idx = field_index("typography.families");
        let mut editing = open(idx, "Cascadia Mono", true);
        assert_eq!(
            take_pending_edit(&mut editing, &fields),
            Pending::Append(idx, "Cascadia Mono".to_string()),
            "an append is handed back as typed"
        );
        assert!(
            editing.is_some(),
            "and the buffer stays open until the caller says the append landed"
        );
    }

    #[test]
    fn a_buffer_naming_a_field_that_is_gone_is_dropped_not_stuck() {
        let mut editing = open(9999, "orphan", false);
        assert_eq!(take_pending_edit(&mut editing, &fields()), Pending::None);
        assert!(editing.is_none(), "an unwritable orphan is dropped, not held");
    }

    fn setting_keys(rows: &[SettingsRowModel]) -> Vec<String> {
        rows.iter()
            .filter_map(|r| match r {
                SettingsRowModel::Setting { key, .. } => Some(key.clone()),
                _ => None,
            })
            .collect()
    }

    fn no_pending() -> std::collections::BTreeSet<String> {
        std::collections::BTreeSet::new()
    }

    fn build(
        values: &serde_json::Value,
        provenance: &BTreeMap<String, Source>,
        filter: &str,
    ) -> (Vec<SettingsRowModel>, Vec<RowAction>) {
        build_rows(&fields(), values, provenance, filter, None, &no_pending(), None)
    }

    #[test]
    fn every_schema_key_renders_exactly_once() {
        // A new setting must appear in the overlay without a UI change, or
        // be excluded on purpose — that is the whole point of generating
        // rows from the schema.
        let (rows, actions) = build(&values(), &BTreeMap::new(), "");
        assert_eq!(rows.len(), actions.len(), "rows and actions are parallel by construction");
        let keys = setting_keys(&rows);
        for schema_key in zest_config::schema::keys() {
            let expected = usize::from(!zest_config::ui::UI_EXCLUDED.contains(&schema_key.as_str()));
            assert_eq!(
                keys.iter().filter(|k| **k == schema_key).count(),
                expected,
                "'{schema_key}' must render exactly {expected} time(s)"
            );
        }
    }

    #[test]
    fn not_yet_wired_names_only_real_settings() {
        // An honesty list that lies is worse than none. This cannot catch a
        // key that *became* wired — deleting the entry is part of wiring it —
        // but it catches renames and typos rotting the list.
        let schema_keys = zest_config::schema::keys();
        for key in NOT_YET_WIRED {
            assert!(
                schema_keys.iter().any(|k| k == key),
                "NOT_YET_WIRED names '{key}', which is not a schema key"
            );
        }
    }

    #[test]
    fn a_wired_setting_carries_no_not_applied_tag() {
        // The other direction of the list, which nothing else checks: #301
        // wired both scrolling keys, so their rows must have lost the faint
        // "not applied yet" chip. Asserted on the row rather than on the
        // constant, because `inert` is what the user actually sees — and a
        // row that keeps claiming a live setting does nothing is exactly the
        // dishonesty NOT_YET_WIRED exists to prevent.
        let (rows, _) = build(&values(), &BTreeMap::new(), "");
        for key in [
            "scrolling.lines_per_notch",
            "scrolling.scroll_on_keypress",
            "shell.cwd",
            "shell.env",
        ] {
            let inert = rows
                .iter()
                .find_map(|r| match r {
                    SettingsRowModel::Setting { key: k, inert, .. } if k == key => Some(*inert),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("'{key}' renders a row"));
            assert!(!inert, "'{key}' is wired, so its row must not say otherwise");
        }
    }

    #[test]
    fn filtering_keeps_group_headers_only_for_surviving_rows() {
        let (rows, _) = build(&values(), &BTreeMap::new(), "opacity");
        assert!(!rows.is_empty(), "two opacity settings exist");
        let mut expects_setting = false;
        for row in &rows {
            match row {
                SettingsRowModel::Group { .. } => {
                    assert!(!expects_setting, "an empty group header survived the filter");
                    expects_setting = true;
                }
                SettingsRowModel::Setting { key, .. } => {
                    assert!(key.contains("opacity"), "'{key}' does not match the filter");
                    expects_setting = false;
                }
                _ => {}
            }
        }
        assert!(!expects_setting, "a trailing group header has no rows");
    }

    #[test]
    fn a_changed_value_is_marked_modified_and_a_default_is_not() {
        let mut settings = zest_config::Settings::default();
        settings.typography.size_pt = 15.0;
        let values = serde_json::to_value(settings).expect("serializes");
        let (rows, _) = build(&values, &BTreeMap::new(), "");
        for row in &rows {
            if let SettingsRowModel::Setting { key, modified, .. } = row {
                assert_eq!(
                    *modified,
                    key == "typography.size_pt",
                    "'{key}': the modified dot must track the schema default"
                );
            }
        }
    }

    #[test]
    fn a_stronger_layer_warns_and_the_user_file_does_not() {
        // The chip is the difference between "settings are broken" and "the
        // profile is winning" when an edit visibly does not move the value.
        let mut provenance = BTreeMap::new();
        provenance.insert("typography.size_pt".to_string(), Source::Profile("k8s".into()));
        provenance.insert("window.opacity".to_string(), Source::User);
        let (rows, _) = build(&values(), &provenance, "");
        let of = |wanted: &str| {
            rows.iter().find_map(|r| match r {
                SettingsRowModel::Setting { key, provenance, .. } if key == wanted => {
                    Some(provenance.clone())
                }
                _ => None,
            })
        };
        let (text, warn) = of("typography.size_pt").expect("row exists").expect("has a chip");
        assert!(text.contains("profile `k8s`") && warn, "a profile outranks the user file");
        let (text, warn) = of("window.opacity").expect("row exists").expect("has a chip");
        assert!(text.contains("config file") && !warn, "the user's own file is not a warning");
        assert!(of("typography.line_height").expect("row exists").is_none(), "defaults get no chip");
    }

    #[test]
    fn restart_class_keys_say_so() {
        let (rows, _) = build(&values(), &BTreeMap::new(), "");
        for row in &rows {
            if let SettingsRowModel::Setting { key, restart, .. } = row {
                assert_eq!(
                    *restart,
                    zest_config::invalidate::class_of(key) == zest_config::Invalidation::Restart,
                    "'{key}': the restart tag must come from class_of, the authoritative table"
                );
            }
        }
        assert!(
            rows.iter().any(|r| matches!(
                r,
                SettingsRowModel::Setting { restart: true, .. }
            )),
            "shell.command et al. are Restart class; the tag must appear somewhere"
        );
    }

    #[test]
    fn selection_helpers_skip_group_headers() {
        let actions =
            [RowAction::None, RowAction::Field(0), RowAction::Field(1), RowAction::None];
        assert_eq!(nearest_field(&actions, 0), 1, "a header normalizes to the row below it");
        assert_eq!(step_selection(&actions, 1, true), 2);
        assert_eq!(step_selection(&actions, 2, true), 2, "the last field is a wall, not a wrap");
        assert_eq!(step_selection(&actions, 1, false), 1, "so is the first");
    }

    fn field(key: &str) -> UiField {
        fields().into_iter().find(|f| f.key == key).expect("schema key exists")
    }

    #[test]
    fn adjust_clamps_cycles_and_flips() {
        let themes = vec!["obsidian".to_string(), "nord".to_string()];

        let size = field("typography.size_pt");
        assert_eq!(adjust(&size, &serde_json::json!(13.0), 1, &themes, &[]), Some(serde_json::json!(14.0)), "font size steps in whole points");
        assert_eq!(
            adjust(&size, &serde_json::json!(144.0), 1, &themes, &[]),
            Some(serde_json::json!(144.0)),
            "the schema maximum is a wall"
        );
        assert_eq!(
            adjust(&size, &serde_json::json!(4.0), -1, &themes, &[]),
            Some(serde_json::json!(4.0)),
            "and so is the minimum"
        );

        let toggle = field("tabs.restore");
        assert_eq!(adjust(&toggle, &serde_json::json!(true), 1, &themes, &[]), Some(serde_json::json!(false)));

        let position = field("tabs.position");
        assert_eq!(
            adjust(&position, &serde_json::json!("left"), 1, &themes, &[]),
            Some(serde_json::json!("top")),
            "a select cycles with wraparound"
        );

        let theme = field("appearance.theme");
        assert_eq!(
            adjust(&theme, &serde_json::json!("nord"), 1, &themes, &[]),
            Some(serde_json::json!("obsidian")),
            "the theme picker cycles the roster the caller brings"
        );

        let padding = field("window.padding");
        assert_eq!(adjust(&padding, &serde_json::json!(8), 1, &themes, &[]), Some(serde_json::json!(9)), "integers step by one");
    }

    #[test]
    fn repeated_arrow_steps_never_accumulate_float_noise() {
        // Ten steps of 0.1 in naive f32/f64 arithmetic drift; quantizing to
        // the step grid keeps every intermediate value one the user could
        // have typed — and one the config file can hold without noise.
        let lh = field("typography.line_height");
        let mut v = serde_json::json!(1.25);
        for _ in 0..10 {
            v = adjust(&lh, &v, 1, &[], &[]).expect("steps");
        }
        // The off-grid 1.25 snaps to 1.3 first, then nine clean 0.1 steps.
        assert_eq!(v, serde_json::json!(2.2));
    }

    #[test]
    fn typed_input_parses_clamps_or_refuses() {
        let size = field("typography.size_pt");
        assert_eq!(parse_input(&size, " 18 "), Some(serde_json::json!(18.0)));
        assert_eq!(
            parse_input(&size, "900"),
            Some(serde_json::json!(144.0)),
            "typed values clamp to the schema range"
        );
        assert_eq!(parse_input(&size, "big"), None, "garbage is an error, not a silent drop");
        let padding = field("window.padding");
        assert_eq!(parse_input(&padding, "12"), Some(serde_json::json!(12)));
    }

    #[test]
    fn written_toml_is_the_shortest_spelling() {
        // toml_edit writes exactly what Display says, so the widened-f32
        // noise must be cleaned before it reaches the file.
        let size = field("typography.size_pt");
        let v = to_toml(&size, &serde_json::json!(13.5)).expect("writable");
        assert_eq!(v.to_string().trim(), "13.5");
        let spring = field("motion.spring_response");
        let noisy = serde_json::to_value(0.16f32).expect("number");
        let v = to_toml(&spring, &noisy).expect("writable");
        assert_eq!(v.to_string().trim(), "0.16", "widening noise never reaches config.toml");
        let padding = field("window.padding");
        let v = to_toml(&padding, &serde_json::json!(12)).expect("writable");
        assert_eq!(v.to_string().trim(), "12", "integers are written as integers");
        let families = field("typography.families");
        let v = to_toml(&families, &serde_json::json!(["Meslo LG M", "Consolas"]))
            .expect("a font stack is writable");
        assert_eq!(v.to_string().trim(), r#"["Meslo LG M", "Consolas"]"#, "the whole stack, in order");
        let features = field("typography.features");
        let v = to_toml(&features, &serde_json::json!(["calt", "-liga"]))
            .expect("tag lists write as arrays now that the chips edit them");
        assert_eq!(v.to_string().trim(), r#"["calt", "-liga"]"#, "a leading '-' is kept verbatim");
        let env = field("shell.env");
        let v = to_toml(&env, &serde_json::json!({"FOO": "bar", "GONE": ""}))
            .expect("key-value maps write as inline tables");
        assert_eq!(
            v.to_string().trim(),
            r#"{ FOO = "bar", GONE = "" }"#,
            "an empty value is written, not skipped — empty unsets under wholesale replace"
        );
    }

    #[test]
    fn banners_pin_above_everything() {
        let pending: std::collections::BTreeSet<String> =
            ["shell.command".to_string()].into_iter().collect();
        let (rows, actions) = build_rows(
            &fields(),
            &values(),
            &BTreeMap::new(),
            "",
            None,
            &pending,
            Some("could not save"),
        );
        assert!(
            matches!(&rows[0], SettingsRowModel::Notice { text } if text.contains("could not save")),
            "a failed write outranks everything"
        );
        assert!(
            matches!(&rows[1], SettingsRowModel::Notice { text } if text.contains("next launch")),
            "the restart banner is the surface the log line never had"
        );
        assert_eq!(actions[0], RowAction::None);
        assert_eq!(actions[1], RowAction::None);
        let (rows, _) = build(&values(), &BTreeMap::new(), "");
        assert!(
            !matches!(&rows[0], SettingsRowModel::Notice { .. }),
            "no banner when nothing is owed"
        );
    }

    #[test]
    fn the_edited_field_shows_its_buffer_not_its_value() {
        let all = fields();
        let idx = all.iter().position(|f| f.key == "typography.size_pt").expect("exists");
        let edit =
            EditBuffer {
            field_idx: idx,
            buffer: crate::text_field::TextField::new("18"),
            error: false,
            append: false,
        };
        let (rows, _) = build_rows(
            &all,
            &values(),
            &BTreeMap::new(),
            "",
            Some(&edit),
            &no_pending(),
            None,
        );
        let cell = rows
            .iter()
            .find_map(|r| match r {
                SettingsRowModel::Setting { key, value, .. } if key == "typography.size_pt" => {
                    Some(value.clone())
                }
                _ => None,
            })
            .expect("row exists");
        assert_eq!(
            cell,
            SettingsValueCell::Editing {
                buffer: "18".to_string(),
                caret: 2,
                selection: None,
                error: false
            },
            "while a buffer is open the row draws the buffer, or typing is invisible"
        );
    }

    #[test]
    fn strings_seed_their_buffer_and_round_trip() {
        let cmd = field("shell.command");
        assert_eq!(
            edit_seed(&cmd, Some(&serde_json::json!("/bin/zsh"))),
            "/bin/zsh",
            "a string edit starts from the current value, not a blank line"
        );
        assert_eq!(parse_input(&cmd, "/bin/fish"), Some(serde_json::json!("/bin/fish")));
        let v = to_toml(&cmd, &serde_json::json!("/bin/fish")).expect("writable");
        assert_eq!(v.to_string().trim(), "\"/bin/fish\"");
    }

    #[test]
    fn a_slider_quantizes_to_the_arrow_keys_grid() {
        // A drag and a keypress must never disagree about which values
        // exist, or dragging leaves noise the arrows then walk through.
        let opacity = field("window.opacity");
        assert_eq!(slider_value(&opacity, 0.5), Some(serde_json::json!(0.5)));
        assert_eq!(
            slider_value(&opacity, 0.52),
            Some(serde_json::json!(0.5)),
            "positions snap to twentieths of the travel"
        );
        assert_eq!(slider_value(&opacity, 2.0), Some(serde_json::json!(1.0)), "clamped past the end");
        assert_eq!(slider_value(&opacity, -1.0), Some(serde_json::json!(0.0)));
    }

    #[test]
    fn the_font_picker_writes_one_face() {
        // The point of the roster: a font face is choosable from the settings
        // screen rather than only from config.toml.
        let fonts = vec!["Cascadia Mono".to_string(), "Meslo LG M".to_string()];
        let f = field("typography.families");
        let current = serde_json::json!(["Cascadia Mono", "Consolas"]);

        let next = adjust(&f, &current, 1, &[], &fonts).expect("cycles");
        let got: Vec<&str> = next.as_array().unwrap().iter().filter_map(|v| v.as_str()).collect();

        assert_eq!(got, ["Meslo LG M"], "one chosen face, not a chain");
    }

    #[test]
    fn cycling_the_font_picker_never_grows_the_stack() {
        // It did. Keeping the face just stepped off looks considerate and
        // appends on every press, so one pass across an installed set left
        // ninety families in a real config -- Curlz MT among the fallbacks.
        let fonts: Vec<String> =
            (0..30).map(|i| format!("Face {i}")).collect();
        let f = field("typography.families");
        let mut v = serde_json::json!(["Face 0", "Consolas", "monospace"]);
        for _ in 0..60 {
            v = adjust(&f, &v, 1, &[], &fonts).expect("cycles");
        }
        let got: Vec<&str> = v.as_array().unwrap().iter().filter_map(|s| s.as_str()).collect();
        assert_eq!(got.len(), 1, "one entry however many times it is cycled: {got:?}");
    }

    #[test]
    fn a_font_row_without_a_roster_does_nothing_rather_than_emptying_the_stack() {
        let f = field("typography.families");
        let current = serde_json::json!(["Cascadia Mono"]);
        assert!(adjust(&f, &current, 1, &[], &[]).is_none());
    }

    #[test]
    fn every_widget_gets_the_cell_its_shape_asks_for() {
        // §11's vocabulary, decided here so the layout can stay dumb: ≤3
        // undocumented variants segment, documented ones drop down, numbers
        // step, lists draw as lists.
        let (rows, _) = build(&values(), &BTreeMap::new(), "");
        let cell_of = |wanted: &str| {
            rows.iter()
                .find_map(|r| match r {
                    SettingsRowModel::Setting { key, value, .. } if key == wanted => {
                        Some(value.clone())
                    }
                    _ => None,
                })
                .unwrap_or_else(|| panic!("'{wanted}' row exists"))
        };
        assert!(
            matches!(cell_of("tabs.position"), SettingsValueCell::Segmented { ref options, selected: Some(_) } if options.len() == 2),
            "two plain variants are a segmented control"
        );
        assert!(
            matches!(cell_of("window.backdrop"), SettingsValueCell::Select { .. }),
            "documented variants earn the dropdown — window.backdrop is the §11 case"
        );
        assert!(
            matches!(cell_of("window.padding"), SettingsValueCell::Stepper { ref text } if text == "8 px"),
            "a number is the stepper, value with its unit"
        );
        assert!(
            matches!(cell_of("typography.families"), SettingsValueCell::FontList { .. }),
            "the font stack draws as stacked rows — order is the setting"
        );
        assert!(
            matches!(cell_of("typography.features"), SettingsValueCell::TagList { .. }),
            "features are chips"
        );
        assert!(
            matches!(cell_of("shell.env"), SettingsValueCell::KeyValue { .. }),
            "env entries are paired cells"
        );
    }

    #[test]
    fn the_theme_pill_is_a_roster_select_with_no_variants() {
        // Why the mouse must route a Select-pill click through the Enter
        // dispatch instead of arming `ui.menu` directly: these fields draw
        // the same pill as a documented Select, but their options are a
        // roster (`zest_theme::builtin`, gathered at open) rather than
        // `field.variants` — a menu armed for them resolves against an
        // empty variant list and is discarded same-pass, so the pill would
        // open nothing and the theme would be unreachable by mouse.
        let fields = fields();
        for key in ["appearance.theme", "appearance.light_theme"] {
            let f = fields.iter().find(|f| f.key == key).unwrap_or_else(|| panic!("{key} in schema"));
            assert_eq!(f.widget, Widget::ThemePicker, "{key} is the roster widget");
            assert!(
                matches!(value_cell(f, None, &[]), SettingsValueCell::Select { .. }),
                "{key} draws the dropdown pill a click will land on"
            );
            assert!(
                f.variants.is_empty(),
                "{key} carries no variants — `ui.menu` would resolve to nothing for it"
            );
        }
    }

    #[test]
    fn font_faces_past_the_first_resolvable_are_fallback() {
        // The first face that resolves is the one the atlas shapes with;
        // everything after it is along for coverage, and the row should say
        // so instead of implying six co-equal choices.
        let f = field("typography.families");
        let value = serde_json::json!(["Ghost Grotesk", "Cascadia Mono", "Consolas"]);
        let installed = vec!["cascadia mono".to_string(), "Consolas".to_string()];
        let cell = value_cell(&f, Some(&value), &installed);
        let SettingsValueCell::FontList { faces } = cell else {
            panic!("font stack renders as FontList")
        };
        assert_eq!(
            faces.iter().map(|f| f.fallback).collect::<Vec<_>>(),
            [false, false, true],
            "an unresolvable face before the winner is not 'fallback'; the one after it is \
             (and resolvability compares case-insensitively)"
        );
    }

    #[test]
    fn unknown_keys_suggest_only_plausible_typos() {
        let keys: Vec<String> = zest_config::schema::keys();
        assert_eq!(
            suggest_key("typography.size_px", &keys).as_deref(),
            Some("typography.size_pt"),
            "one edit away is the canonical did-you-mean"
        );
        assert_eq!(
            suggest_key("window.blur_radius", &keys),
            None,
            "far from everything: no match beats a stretch"
        );
        // The ≤2 boundary itself, pinned with arithmetic rather than vibes.
        assert_eq!(levenshtein("size_pt", "size_px"), 1);
        assert_eq!(levenshtein("opacity", "opac"), 3);
        assert_eq!(levenshtein("", "ab"), 2);
        let (rows, actions) = build_unknown_rows(
            &["typography.size_px".to_string(), "window.blur_radius".to_string()],
            &BTreeMap::new(),
            "",
            &keys,
        );
        assert_eq!(rows.len(), 2);
        assert!(actions.iter().all(|a| *a == RowAction::None), "unknown rows run nothing");
        assert!(
            matches!(&rows[0], SettingsRowModel::Unknown { suggestion: Some(s), .. } if s == "typography.size_pt")
        );
        assert!(matches!(&rows[1], SettingsRowModel::Unknown { suggestion: None, .. }));
        let (rows, _) = build_unknown_rows(
            &["typography.size_px".to_string()],
            &BTreeMap::new(),
            "blur",
            &keys,
        );
        assert!(rows.is_empty(), "the filter reaches the unknown keys too");
    }

    #[test]
    fn rows_scope_to_their_category_and_counts_add_up() {
        // The rail's contract: one category's rows are exactly its group's
        // fields, and the per-group badges sum to the footer's sentence.
        let all = fields();
        let mut settings = zest_config::Settings::default();
        settings.typography.size_pt = 15.0;
        settings.window.opacity = 0.5;
        settings.window.padding = 12;
        let values = serde_json::to_value(settings).expect("serializes");

        let (rows, actions) = build_category_rows(
            &all,
            &values,
            &BTreeMap::new(),
            "Text",
            "",
            None,
            &no_pending(),
            None,
            &[],
        );
        assert_eq!(rows.len(), actions.len(), "parallel by construction");
        for row in &rows {
            match row {
                SettingsRowModel::Setting { key, .. } => assert!(
                    field(key).group == "Text",
                    "'{key}' leaked into the Text category"
                ),
                SettingsRowModel::Group { .. } => {
                    panic!("category views have no group headers — the rail is the grouping")
                }
                _ => {}
            }
        }

        let groups = categories(&all);
        assert_eq!(groups.first().map(String::as_str), Some("Text"));
        assert_eq!(
            groups.last().map(String::as_str),
            Some(UNKNOWN_CATEGORY),
            "the ninth category is fixed at the end"
        );
        let (counts, total) = modified_counts(&all, &values, &groups);
        let text_idx = groups.iter().position(|g| g == "Text").expect("exists");
        let window_idx = groups.iter().position(|g| g == "Window").expect("exists");
        assert_eq!(counts[text_idx], 1, "size_pt differs");
        assert_eq!(counts[window_idx], 2, "opacity and padding differ");
        assert_eq!(total, 3, "the footer's sentence is the badges' sum");
        assert_eq!(
            counts.iter().sum::<usize>(),
            total,
            "no category counts what another already did"
        );
    }

    #[test]
    fn the_reset_dot_resolves_its_row_to_the_right_key_and_the_file_loses_it() {
        // The dot IS the reset button (§11). remove_value's own tests cover
        // the file mechanics; this covers the wiring the click handler uses:
        // row index → parallel action → field → key, then the temp-file
        // round trip — write, reset, key gone, comments elsewhere intact.
        let all = fields();
        let mut settings = zest_config::Settings::default();
        settings.typography.size_pt = 15.0;
        let values = serde_json::to_value(settings).expect("serializes");
        let (rows, actions) = build_category_rows(
            &all,
            &values,
            &BTreeMap::new(),
            "Text",
            "",
            None,
            &no_pending(),
            None,
            &[],
        );
        let row = rows
            .iter()
            .position(|r| matches!(r, SettingsRowModel::Setting { key, modified: true, .. } if key == "typography.size_pt"))
            .expect("the modified row exists");
        let RowAction::Field(idx) = actions[row] else { panic!("a setting row maps to a field") };
        let key = &all[idx].key;
        assert_eq!(key, "typography.size_pt", "the parallel lists agree about the row's meaning");

        let path = std::env::temp_dir().join("zesterm-settings-tab-reset-test.toml");
        std::fs::write(
            &path,
            "# my terminal\n[typography]\n# I like it big\nsize_pt = 15.0\nline_height = 1.5\n",
        )
        .expect("write");
        zest_config::remove_value(&path, key).expect("reset");
        let text = std::fs::read_to_string(&path).expect("read");
        assert!(!text.contains("size_pt"), "the key is gone: {text}");
        assert!(text.contains("# my terminal"), "comments elsewhere survive: {text}");
        assert!(text.contains("line_height = 1.5"), "siblings survive: {text}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn category_prefixes_and_ledes_exist_for_every_rail_row() {
        let all = fields();
        assert_eq!(category_prefix(&all, "Text"), "typography", "the dotted prefix beside the heading");
        assert_eq!(category_prefix(&all, "Window"), "window");
        for group in categories(&all) {
            assert!(!category_lede(&group).is_empty(), "'{group}' has no lede");
        }
    }

    #[test]
    fn floats_render_shortest_never_noisy() {
        // serde_json widens the f32 settings to f64 — the committed schema
        // stores spring_response's default as 0.1599999964237213 — and that
        // noise must never reach a row, or, later, the config file.
        assert_eq!(format_number(f64::from(13.5f32)), "13.5");
        assert_eq!(format_number(13.0), "13");
        assert_eq!(format_number(f64::from(0.16f32)), "0.16");
        let (rows, _) = build(&values(), &BTreeMap::new(), "spring response");
        let cell = rows
            .iter()
            .find_map(|r| match r {
                SettingsRowModel::Setting { key, value, .. }
                    if key == "motion.spring_response" =>
                {
                    Some(value.clone())
                }
                _ => None,
            })
            .expect("the spring_response row exists");
        assert_eq!(
            cell,
            SettingsValueCell::Stepper { text: "0.16".to_string() },
            "the noisiest f32 in the schema must render as what the user wrote"
        );
    }
}

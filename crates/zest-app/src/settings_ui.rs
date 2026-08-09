//! The settings overlay's rows, built from the schema walk.
//!
//! Pure on purpose, like `chrome::layout`: fields, values and provenance in,
//! display rows and a parallel action list out. Nothing here touches the
//! window, the fonts or the filesystem, which is what lets the coverage test
//! hold the overlay to the schema without opening a window.
//!
//! The row list and the action list are built in one pass — the picker's
//! discipline — so a drawn row and its meaning cannot drift.

use std::collections::BTreeMap;

use zest_config::ui::{UiField, Widget};
use zest_config::Source;

use crate::chrome::model::{SettingsRowModel, SettingsValueCell};

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
    pub buffer: String,
    /// The last Enter did not parse; drawn as an error until the text changes.
    pub error: bool,
}

/// Settings the schema declares but the app does not consume yet.
///
/// Honesty over polish: a control that visibly does nothing reads as broken,
/// so these rows carry a faint "not applied yet" tag instead of pretending.
/// Wiring one up includes deleting its entry here — the test below only
/// keeps this list from naming keys that do not exist at all.
pub const NOT_YET_WIRED: &[&str] = &[
    "appearance.follow_system_theme",
    "appearance.light_theme",
    "appearance.text_contrast",
    "appearance.text_gamma",
    "cursor.blink",
    "cursor.blink_interval_ms",
    "cursor.shape",
    "cursor.trail",
    "motion.enabled",
    "motion.respect_system_reduce_motion",
    "motion.smooth_scroll",
    "motion.spring_damping",
    "motion.spring_response",
    "scrolling.lines_per_notch",
    "scrolling.scroll_on_keypress",
    "shell.cwd",
    "shell.env",
    "typography.features",
    "typography.ligatures",
    "window.backdrop",
    "window.columns",
    "window.custom_chrome",
    "window.rows",
];

/// Display order of the groups. The schema's alphabetical property order is
/// an artifact; this is the order a person tunes a terminal in. Groups the
/// list does not name append at the end rather than vanishing.
const GROUP_ORDER: &[&str] =
    &["Text", "Appearance", "Window", "Tabs", "Shell", "Scrolling", "Cursor", "Motion"];

/// Build the overlay's rows and their actions, one pass.
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
    let filter = filter.to_lowercase();
    let mut rows = Vec::new();
    let mut actions = Vec::new();

    // Banners pin to the top, above any filter's result: a write that
    // failed or a restart owed is true whatever the user is searching for.
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

    let mut groups: Vec<&str> = GROUP_ORDER.to_vec();
    for field in fields {
        if !groups.contains(&field.group.as_str()) {
            groups.push(&field.group);
        }
    }

    for group in groups {
        let members: Vec<(usize, &UiField)> = fields
            .iter()
            .enumerate()
            .filter(|(_, f)| f.group == group && matches_filter(f, &filter))
            .collect();
        if members.is_empty() {
            continue;
        }
        rows.push(SettingsRowModel::Group { title: group.to_string() });
        actions.push(RowAction::None);
        for (index, field) in members {
            let editing = editing.filter(|e| e.field_idx == index);
            rows.push(setting_row(field, values, provenance, editing));
            actions.push(RowAction::Field(index));
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
            buffer: edit.buffer.clone(),
            error: edit.error,
        },
        None => value_cell(field, value),
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

fn value_cell(field: &UiField, value: Option<&serde_json::Value>) -> SettingsValueCell {
    let text_of = |v: Option<&serde_json::Value>| match v {
        Some(serde_json::Value::String(s)) if !s.is_empty() => s.clone(),
        Some(serde_json::Value::String(_)) | None => "—".to_string(),
        Some(other) => format_scalar(other),
    };
    match field.widget {
        Widget::Toggle => SettingsValueCell::Toggle {
            on: value.and_then(serde_json::Value::as_bool).unwrap_or(false),
        },
        Widget::Select | Widget::ThemePicker => SettingsValueCell::Select { value: text_of(value) },
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
        Widget::Number | Widget::Text | Widget::Path => {
            SettingsValueCell::Text { text: text_of(value) }
        }
        Widget::FontList | Widget::TagList => SettingsValueCell::ReadOnly {
            text: match value.and_then(serde_json::Value::as_array) {
                Some(items) if !items.is_empty() => items
                    .iter()
                    .filter_map(serde_json::Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", "),
                _ => "—".to_string(),
            },
        },
        Widget::KeyValue => SettingsValueCell::ReadOnly {
            text: match value.and_then(serde_json::Value::as_object) {
                Some(map) if !map.is_empty() => map
                    .iter()
                    .map(|(k, v)| format!("{k}={}", v.as_str().unwrap_or_default()))
                    .collect::<Vec<_>>()
                    .join(", "),
                _ => "—".to_string(),
            },
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
/// `themes` is the theme roster for [`Widget::ThemePicker`] — the schema
/// cannot know it, so the caller brings it.
#[must_use]
pub fn adjust(
    field: &UiField,
    current: &serde_json::Value,
    dir: i32,
    themes: &[String],
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
    match value {
        Some(v) if field.integer => v.as_i64().map_or_else(String::new, |i| i.to_string()),
        Some(v) => v.as_f64().map_or_else(String::new, format_number),
        None => String::new(),
    }
}

/// A committed value as the TOML to write, or `None` for the widgets that
/// cannot be edited inline yet (the list-shaped ones).
#[must_use]
pub fn to_toml(
    field: &UiField,
    value: &serde_json::Value,
) -> Option<zest_config::toml_edit::Value> {
    match field.widget {
        Widget::Toggle => Some(zest_config::toml_edit::Value::from(value.as_bool()?)),
        Widget::Select | Widget::ThemePicker | Widget::Text | Widget::Path => {
            Some(zest_config::toml_edit::Value::from(value.as_str()?))
        }
        Widget::Number | Widget::Slider if field.integer => {
            Some(zest_config::toml_edit::Value::from(value.as_i64()?))
        }
        Widget::Number | Widget::Slider => {
            Some(zest_config::toml_edit::Value::from(clean_float(value.as_f64()?)))
        }
        Widget::FontList | Widget::TagList | Widget::KeyValue => None,
    }
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
fn first_line(description: &str) -> String {
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

    fn setting_keys(rows: &[SettingsRowModel]) -> Vec<String> {
        rows.iter()
            .filter_map(|r| match r {
                SettingsRowModel::Setting { key, .. } => Some(key.clone()),
                SettingsRowModel::Group { .. } | SettingsRowModel::Notice { .. } => None,
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
                SettingsRowModel::Notice { .. } => {}
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
        assert_eq!(adjust(&size, &serde_json::json!(13.0), 1, &themes), Some(serde_json::json!(14.0)), "font size steps in whole points");
        assert_eq!(
            adjust(&size, &serde_json::json!(144.0), 1, &themes),
            Some(serde_json::json!(144.0)),
            "the schema maximum is a wall"
        );
        assert_eq!(
            adjust(&size, &serde_json::json!(4.0), -1, &themes),
            Some(serde_json::json!(4.0)),
            "and so is the minimum"
        );

        let toggle = field("tabs.restore");
        assert_eq!(adjust(&toggle, &serde_json::json!(true), 1, &themes), Some(serde_json::json!(false)));

        let position = field("tabs.position");
        assert_eq!(
            adjust(&position, &serde_json::json!("left"), 1, &themes),
            Some(serde_json::json!("top")),
            "a select cycles with wraparound"
        );

        let theme = field("appearance.theme");
        assert_eq!(
            adjust(&theme, &serde_json::json!("nord"), 1, &themes),
            Some(serde_json::json!("obsidian")),
            "the theme picker cycles the roster the caller brings"
        );

        let padding = field("window.padding");
        assert_eq!(adjust(&padding, &serde_json::json!(8), 1, &themes), Some(serde_json::json!(9)), "integers step by one");
    }

    #[test]
    fn repeated_arrow_steps_never_accumulate_float_noise() {
        // Ten steps of 0.1 in naive f32/f64 arithmetic drift; quantizing to
        // the step grid keeps every intermediate value one the user could
        // have typed — and one the config file can hold without noise.
        let lh = field("typography.line_height");
        let mut v = serde_json::json!(1.25);
        for _ in 0..10 {
            v = adjust(&lh, &v, 1, &[]).expect("steps");
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
        assert!(to_toml(&families, &serde_json::json!([])).is_none(), "lists are not inline-editable yet");
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
        let edit = EditBuffer { field_idx: idx, buffer: "18".to_string(), error: false };
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
            SettingsValueCell::Editing { buffer: "18".to_string(), error: false },
            "while a buffer is open the row draws the buffer, or typing is invisible"
        );
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
            SettingsValueCell::Text { text: "0.16".to_string() },
            "the noisiest f32 in the schema must render as what the user wrote"
        );
    }
}

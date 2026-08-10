//! The schema, walked into fields a settings UI can render.
//!
//! [`crate::schema`] states the contract: settings UIs are *generated* from
//! the JSON Schema, so a new setting reaches every client without a matching
//! UI change. This module is the reading half of that contract — one walk
//! that turns the schema's `x_zest_*` extensions, ranges and doc comments
//! into [`UiField`]s. It lives outside the `fs` feature on purpose: the web
//! client renders from the same schema and may want the same walk.
//!
//! Nothing here knows how to *draw* a widget; [`Widget`] is a vocabulary,
//! and each client maps it to whatever it has.

use serde::Serialize;

use crate::schema::{self, resolve_ref};

/// How a field asks to be edited (`x_zest_widget`).
///
/// Serialized kebab-case to match `x_zest_widget`'s own spelling in the schema,
/// so a client reading either source sees the same string.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Widget {
    Toggle,
    Number,
    Slider,
    Select,
    /// A select whose options are the theme roster — which this crate cannot
    /// know, so [`UiField::variants`] arrives empty and the client fills it.
    ThemePicker,
    Text,
    Path,
    FontList,
    TagList,
    KeyValue,
}

/// One option of a [`Widget::Select`] field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UiVariant {
    /// The kebab-case wire value, exactly as serde writes it.
    pub value: String,
    /// The variant's doc comment; empty when the schema has none.
    pub description: String,
}

/// One settings field, as a UI should render it.
///
/// `camelCase` on the wire because the only consumer that reads it as data is
/// the browser, whose other generated record — `UiTokens` — is camelCase too;
/// one convention across the generated surface beats matching Rust's spelling
/// in a file no Rust ever parses.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UiField {
    /// Dotted key, e.g. `typography.size_pt`.
    pub key: String,
    /// Display group (`x_zest_group`).
    pub group: String,
    pub widget: Widget,
    /// The field's doc comment.
    pub description: String,
    /// Schema `minimum`/`maximum`, when both are present.
    pub range: Option<(f64, f64)>,
    /// Whether the schema types this as an integer — which decides how an
    /// edited value is written back (`14`, never `14.0`).
    pub integer: bool,
    pub variants: Vec<UiVariant>,
    /// The schema default, for a "modified" marker and reset-to-default.
    pub default: serde_json::Value,
    /// `x_zest_restart`. Advisory: `invalidate::class_of` is the
    /// authoritative "needs a restart" answer and covers more keys; this
    /// extension exists for clients that only have the schema.
    pub restart_hint: bool,
}

/// Keys the walk skips on purpose.
///
/// `schema_version` is machinery, not a preference. `profiles` is a free-form
/// tree edited through its own future form, not a single field.
pub const UI_EXCLUDED: &[&str] = &["schema_version", "profiles"];

/// Every renderable settings field, in schema walk order.
///
/// Grouping and group *ordering* are the client's call — the schema's
/// alphabetical property order is an artifact, not a design.
#[must_use]
pub fn fields() -> Vec<UiField> {
    let root = schema::json_schema();
    let mut out = Vec::new();
    walk(&root, &root, &mut String::new(), &mut out, 0);
    out
}

fn walk(
    root: &serde_json::Value,
    node: &serde_json::Value,
    path: &mut String,
    out: &mut Vec<UiField>,
    depth: usize,
) {
    if depth > 8 {
        return;
    }
    let node = resolve_ref(root, node);
    let Some(props) = node.get("properties").and_then(serde_json::Value::as_object) else {
        return;
    };
    for (name, child) in props {
        let len = path.len();
        if !path.is_empty() {
            path.push('.');
        }
        path.push_str(name);

        if !UI_EXCLUDED.contains(&path.as_str()) {
            let resolved = resolve_ref(root, child);
            if resolved.get("properties").is_some() {
                walk(root, &resolved, path, out, depth + 1);
            } else {
                out.push(field_of(root, path, child, &resolved));
            }
        }

        path.truncate(len);
    }
}

/// Build one field from its schema node.
///
/// Metadata sits on the *field* node (schemars keeps `x_zest_*`, `default`
/// and the doc comment beside the `$ref`); the shape — type, ranges, enum
/// variants — sits on the resolved node. Both are consulted for each so a
/// future schemars re-shuffle degrades to "still found" rather than "empty".
fn field_of(
    root: &serde_json::Value,
    key: &str,
    field: &serde_json::Value,
    resolved: &serde_json::Value,
) -> UiField {
    let str_of = |name: &str| {
        field
            .get(name)
            .or_else(|| resolved.get(name))
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    let num_of = |name: &str| {
        field.get(name).or_else(|| resolved.get(name)).and_then(serde_json::Value::as_f64)
    };
    let ty = field
        .get("type")
        .or_else(|| resolved.get("type"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string();

    let widget = match str_of("x_zest_widget").as_str() {
        "toggle" => Widget::Toggle,
        "number" => Widget::Number,
        "slider" => Widget::Slider,
        "select" => Widget::Select,
        "theme-picker" => Widget::ThemePicker,
        "text" => Widget::Text,
        "path" => Widget::Path,
        "font-list" => Widget::FontList,
        "tag-list" => Widget::TagList,
        "key-value" => Widget::KeyValue,
        // A field without a hint still renders: fall back from its type
        // rather than vanishing, because an invisible setting is the worst
        // outcome a metadata gap can have.
        _ => match ty.as_str() {
            "boolean" => Widget::Toggle,
            "number" | "integer" => Widget::Number,
            "array" => Widget::TagList,
            "object" => Widget::KeyValue,
            _ => Widget::Text,
        },
    };

    UiField {
        key: key.to_string(),
        group: str_of("x_zest_group"),
        widget,
        description: str_of("description"),
        range: num_of("minimum").zip(num_of("maximum")),
        integer: ty == "integer",
        variants: variants_of(root, resolved),
        default: field.get("default").cloned().unwrap_or(serde_json::Value::Null),
        restart_hint: field
            .get("x_zest_restart")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    }
}

/// Enum options, from either shape schemars emits.
///
/// A doc-commented enum comes out as `oneOf: [{const, description}, ..]`; a
/// plain one as `enum: [..]` — today's schema contains both (`Backdrop` vs
/// `CursorShape`), so this is not future-proofing, it is the present tense.
fn variants_of(root: &serde_json::Value, resolved: &serde_json::Value) -> Vec<UiVariant> {
    if let Some(one_of) = resolved.get("oneOf").and_then(serde_json::Value::as_array) {
        return one_of
            .iter()
            .map(|entry| resolve_ref(root, entry))
            .filter_map(|entry| {
                let value = entry.get("const").and_then(serde_json::Value::as_str)?.to_string();
                let description = entry
                    .get("description")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                Some(UiVariant { value, description })
            })
            .collect();
    }
    resolved
        .get("enum")
        .and_then(serde_json::Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(serde_json::Value::as_str)
                .map(|value| UiVariant { value: value.to_string(), description: String::new() })
                .collect()
        })
        .unwrap_or_default()
}

/// The value at a dotted key inside a serialized [`crate::Settings`] tree.
#[must_use]
pub fn value_at<'a>(root: &'a serde_json::Value, key: &str) -> Option<&'a serde_json::Value> {
    key.split('.').try_fold(root, |node, part| node.get(part))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_schema_key_yields_a_field_or_is_excluded_on_purpose() {
        // The schema is the UI contract: a key the walk drops is a setting
        // three clients silently cannot show.
        let field_keys: Vec<String> = fields().into_iter().map(|f| f.key).collect();
        for key in schema::keys() {
            let excluded = UI_EXCLUDED.contains(&key.as_str());
            assert_eq!(
                field_keys.iter().filter(|k| **k == key).count(),
                usize::from(!excluded),
                "'{key}' must appear exactly once, or be in UI_EXCLUDED knowingly"
            );
        }
        for excluded in UI_EXCLUDED {
            assert!(
                schema::keys().iter().any(|k| k == excluded),
                "UI_EXCLUDED names '{excluded}', which is not a schema key — the list rotted"
            );
        }
    }

    #[test]
    fn every_field_arrives_renderable() {
        // A field with no group lands nowhere; one with no description is a
        // bare key with no explanation, in three clients at once.
        for field in fields() {
            assert!(!field.group.is_empty(), "'{}' has no x_zest_group", field.key);
            assert!(!field.description.is_empty(), "'{}' has no description", field.key);
            assert!(
                !field.default.is_null(),
                "'{}' has no schema default, so 'modified' cannot be computed",
                field.key
            );
        }
    }

    #[test]
    fn every_select_has_variants_from_either_schema_shape() {
        // Backdrop is oneOf+const, CursorShape is a plain enum array. If a
        // schemars bump changes either shape, this fails loudly instead of
        // three clients rendering an empty dropdown.
        for field in fields() {
            if field.widget == Widget::Select {
                assert!(!field.variants.is_empty(), "select '{}' has no variants", field.key);
            }
        }
        let all = fields();
        let backdrop = all.iter().find(|f| f.key == "window.backdrop").expect("exists");
        assert!(
            backdrop.variants.iter().any(|v| v.value == "mica" && !v.description.is_empty()),
            "oneOf variants carry their doc comments"
        );
        let shape = all.iter().find(|f| f.key == "cursor.shape").expect("exists");
        assert!(
            shape.variants.iter().any(|v| v.value == "block"),
            "plain enum arrays still yield variants"
        );
    }

    #[test]
    fn numeric_widgets_carry_their_ranges() {
        // A number without bounds cannot clamp, step, or draw a slider.
        for field in fields() {
            if matches!(field.widget, Widget::Number | Widget::Slider) {
                assert!(field.range.is_some(), "'{}' has no minimum/maximum", field.key);
            }
        }
    }

    #[test]
    fn the_theme_picker_expects_the_client_to_bring_the_roster() {
        let all = fields();
        let theme = all.iter().find(|f| f.key == "appearance.theme").expect("exists");
        assert_eq!(theme.widget, Widget::ThemePicker);
        assert!(
            theme.variants.is_empty(),
            "the schema cannot know the theme roster; hardcoding one here would drift"
        );
    }

    #[test]
    fn value_at_reads_a_serialized_settings_tree() {
        let values = serde_json::to_value(crate::Settings::default()).expect("serializes");
        assert_eq!(
            value_at(&values, "typography.size_pt").and_then(serde_json::Value::as_f64),
            Some(13.0)
        );
        assert!(value_at(&values, "no.such.key").is_none());
    }
}

//! JSON Schema export, and the test that keeps it honest.
//!
//! The schema is not documentation. It is what the web and phone settings UIs are
//! *generated* from, so a new setting appears remotely without a matching UI
//! change in three clients. Shipping it also gives editors autocomplete over
//! `config.toml` through taplo, for free.
//!
//! [`keys`] walking the schema is what turns "added a setting but forgot to wire
//! its invalidation" from a mysterious *why doesn't this apply until restart* into
//! a failing test.

use crate::settings::Settings;

/// The JSON Schema for [`Settings`].
#[must_use]
pub fn json_schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(Settings)).unwrap_or(serde_json::Value::Null)
}

/// Pretty-printed schema, for writing to `schemas/zesterm.schema.json`.
#[must_use]
pub fn json_schema_string() -> String {
    serde_json::to_string_pretty(&json_schema()).unwrap_or_default()
}

/// Every settings key in the schema, dotted.
///
/// Walks `properties` through `$ref`s into `$defs`, which is how schemars emits
/// a nested struct. Descends only into groups — a map-valued setting like
/// `shell.env` is one key, not a namespace.
#[must_use]
pub fn keys() -> Vec<String> {
    let schema = json_schema();
    let mut out = Vec::new();
    walk(&schema, &schema, &mut String::new(), &mut out, 0);
    out.sort();
    out.dedup();
    out
}

fn walk(
    root: &serde_json::Value,
    node: &serde_json::Value,
    path: &mut String,
    out: &mut Vec<String>,
    depth: usize,
) {
    // The tree is a handful of levels deep. A bound costs nothing and means a
    // recursive schema cannot hang the test suite.
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

        let child = resolve_ref(root, child);
        // A struct has named properties; a map has `additionalProperties`. Only
        // the former is a group of settings.
        if child.get("properties").is_some() {
            walk(root, &child, path, out, depth + 1);
        } else {
            out.push(path.clone());
        }

        path.truncate(len);
    }
}

/// Follow a `$ref` into `$defs`, or return the node unchanged.
pub(crate) fn resolve_ref(root: &serde_json::Value, node: &serde_json::Value) -> serde_json::Value {
    let Some(reference) = node.get("$ref").and_then(serde_json::Value::as_str) else {
        return node.clone();
    };
    let Some(name) = reference.strip_prefix("#/$defs/") else {
        return node.clone();
    };
    root.get("$defs")
        .and_then(|d| d.get(name))
        .cloned()
        .unwrap_or_else(|| node.clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::invalidate::KEYS;

    /// The test this module exists for.
    ///
    /// Adding a setting and forgetting to give it an invalidation class produces
    /// a setting that silently only applies on restart. That is nearly
    /// impossible to notice by hand and trivial to catch here.
    #[test]
    fn every_setting_has_an_invalidation_class() {
        let known: Vec<&str> = KEYS.iter().map(|(k, _)| *k).collect();
        let missing: Vec<String> =
            keys().into_iter().filter(|k| !known.contains(&k.as_str())).collect();

        assert!(
            missing.is_empty(),
            "these settings have no entry in invalidate::KEYS, so a change to them \
             would silently need a restart: {missing:#?}"
        );
    }

    /// And the reverse, which catches a rename leaving a dead entry behind.
    #[test]
    fn every_invalidation_class_names_a_real_setting() {
        let schema_keys = keys();
        let stale: Vec<&str> = KEYS
            .iter()
            .map(|(k, _)| *k)
            .filter(|k| !schema_keys.iter().any(|s| s == k))
            .collect();

        assert!(stale.is_empty(), "invalidate::KEYS names settings that no longer exist: {stale:#?}");
    }

    #[test]
    fn the_schema_walk_finds_nested_settings() {
        let k = keys();
        assert!(k.contains(&"typography.size_pt".to_string()), "{k:#?}");
        assert!(k.contains(&"window.backdrop".to_string()));
        assert!(k.contains(&"motion.spring_damping".to_string()));
    }

    #[test]
    fn a_map_valued_setting_is_a_leaf() {
        let k = keys();
        assert!(k.contains(&"shell.env".to_string()));
        assert!(
            !k.iter().any(|s| s.starts_with("shell.env.")),
            "descended into a map-valued setting"
        );
    }

    #[test]
    fn every_setting_carries_a_description() {
        // schemars turns doc comments into `description`. A field without one
        // ships to the web and phone UIs as a bare key with no explanation.
        let schema = json_schema();
        let mut undocumented = Vec::new();
        for key in keys() {
            if description_of(&schema, &key).is_none() {
                undocumented.push(key);
            }
        }
        assert!(undocumented.is_empty(), "settings with no doc comment: {undocumented:#?}");
    }

    fn description_of(schema: &serde_json::Value, key: &str) -> Option<String> {
        let mut node = schema.clone();
        for part in key.split('.') {
            let resolved = resolve_ref(schema, &node);
            node = resolved.get("properties")?.get(part)?.clone();
        }
        // The description may sit on the field or on the type it refers to.
        node.get("description")
            .or_else(|| resolve_ref(schema, &node).get("description").map(|_| &node["$ref"]))
            .and_then(|_| {
                node.get("description")
                    .and_then(serde_json::Value::as_str)
                    .map(str::to_string)
                    .or_else(|| {
                        resolve_ref(schema, &node)
                            .get("description")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string)
                    })
            })
    }
}

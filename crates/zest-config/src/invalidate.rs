//! What a settings change actually costs.
//!
//! Hot reload is only pleasant if most edits are nearly free. Tweaking a theme
//! colour must not clear the glyph atlas, and changing the line height *must*
//! resize the pty — a terminal whose rows no longer match what the shell thinks
//! is worse than one that needs a restart.
//!
//! The failure this module is built to prevent is not a wrong class. It is a
//! *missing* one: someone adds a setting, forgets to wire its invalidation, and
//! it silently only applies on restart. [`crate::schema`] turns that into a
//! failing test by walking the generated schema and checking every key is
//! reachable from [`diff`].

use crate::settings::Settings;

/// What has to be rebuilt, cheapest first.
///
/// Ordered so `max` of two classes is the more expensive one — a diff touching
/// several areas costs the worst of them, not the sum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Invalidation {
    /// Nothing to do.
    None,
    /// Re-resolve and repaint. Theme colours, motion parameters, gamma.
    Free,
    /// Clear the glyph atlas and re-rasterize. Font family, size, features.
    AtlasBump,
    /// Atlas bump, plus a grid resize, plus `ResizePseudoConsole`.
    ///
    /// Anything that changes the size of a cell lands here, because cell size
    /// decides how many rows and columns fit, and the shell has to be told.
    Geometry,
    /// Reconfigure the surface. Opacity, backdrop, alpha mode.
    SurfaceRebuild,
    /// Only a new process picks this up. Reported so the UI can say so rather
    /// than pretending the change took effect.
    Restart,
}

impl Invalidation {
    /// The more expensive of two classes.
    #[must_use]
    pub fn max(self, other: Self) -> Self {
        if other > self {
            other
        } else {
            self
        }
    }
}

/// Every settings key, paired with the class its change belongs to.
///
/// A flat table rather than a nested walk, because it is meant to be read: the
/// question "what does changing this cost?" should be answerable by looking, and
/// the schema-coverage test consumes exactly this list.
pub const KEYS: &[(&str, Invalidation)] = &[
    ("schema_version", Invalidation::None),
    // Re-read at the next resolve: the launcher menu and the profiles editor
    // pick the change up live, and an already-running session keeps the
    // settings it launched with by design.
    ("profiles", Invalidation::Free),
    // Cell size comes from the font, so most of typography is geometry.
    ("typography.families", Invalidation::Geometry),
    ("typography.size_pt", Invalidation::Geometry),
    ("typography.line_height", Invalidation::Geometry),
    ("typography.cell_width", Invalidation::Geometry),
    ("typography.letter_spacing", Invalidation::Geometry),
    // Features change which glyphs are chosen, not how large a cell is.
    ("typography.features", Invalidation::AtlasBump),
    ("typography.ligatures", Invalidation::AtlasBump),
    // Changes what U+2500..=U+259F rasterizes to, not how large a cell is --
    // the generated mask is already cell-sized either way.
    ("typography.builtin_box_drawing", Invalidation::AtlasBump),
    ("appearance.theme", Invalidation::Free),
    ("appearance.light_theme", Invalidation::Free),
    ("appearance.follow_system_theme", Invalidation::Free),
    // Applied in the resolve pass, which is why these are free rather than an
    // atlas rebuild -- the whole reason gamma is not baked per glyph.
    ("appearance.text_gamma", Invalidation::Free),
    ("appearance.text_contrast", Invalidation::Free),
    // Unlike the two above, this one *is* baked into the atlas: it changes how
    // many bytes a mask texel holds and whether the outline was grid-fitted at
    // all, so every cached bitmap is wrong rather than merely stale.
    ("appearance.text_antialias", Invalidation::AtlasBump),
    // Same reason: grid-fitting changes the bitmap, not how it is composited.
    ("appearance.text_hinting", Invalidation::AtlasBump),
    ("window.opacity", Invalidation::SurfaceRebuild),
    ("window.chrome_opacity", Invalidation::Free),
    ("window.backdrop", Invalidation::SurfaceRebuild),
    // Padding does not change cell size, but it does change how many cells fit.
    ("window.padding", Invalidation::Geometry),
    ("window.custom_chrome", Invalidation::Restart),
    ("window.columns", Invalidation::Restart),
    ("window.rows", Invalidation::Restart),
    // The strip takes space from the grid, so moving or resizing it changes
    // how many cells fit -- geometry, exactly like padding.
    ("tabs.position", Invalidation::Geometry),
    ("tabs.strip_height", Invalidation::Geometry),
    ("tabs.sidebar_width", Invalidation::Geometry),
    ("tabs.show_single_tab", Invalidation::Geometry),
    ("tabs.restore", Invalidation::None),
    // Read when a signal arrives, so a change is in force for the next one.
    ("tabs.attention_bell", Invalidation::None),
    ("tabs.attention_notify", Invalidation::None),
    ("shell.command", Invalidation::Restart),
    ("shell.cwd", Invalidation::Restart),
    ("shell.env", Invalidation::Restart),
    ("scrolling.scrollback", Invalidation::Free),
    ("scrolling.lines_per_notch", Invalidation::Free),
    ("scrolling.scroll_on_output", Invalidation::Free),
    ("scrolling.scroll_on_keypress", Invalidation::Free),
    ("cursor.shape", Invalidation::Free),
    ("cursor.blink", Invalidation::Free),
    ("cursor.blink_interval_ms", Invalidation::Free),
    ("cursor.trail", Invalidation::Free),
    ("motion.enabled", Invalidation::Free),
    ("motion.respect_system_reduce_motion", Invalidation::Free),
    ("motion.smooth_scroll", Invalidation::Free),
    ("motion.spring_response", Invalidation::Free),
    ("motion.spring_damping", Invalidation::Free),
];

/// Which keys changed between two settings trees.
///
/// Compared through the serialized form rather than field by field. Hand-written
/// comparisons are where the missing-key bug is born: adding a field to the
/// struct does not add it to a match arm, and nothing complains.
#[must_use]
pub fn changed_keys(old: &Settings, new: &Settings) -> Vec<String> {
    let a = serde_json::to_value(old).unwrap_or(serde_json::Value::Null);
    let b = serde_json::to_value(new).unwrap_or(serde_json::Value::Null);
    let mut out = Vec::new();
    walk(&a, &b, &mut String::new(), &mut out);
    out.sort();
    out
}

fn walk(a: &serde_json::Value, b: &serde_json::Value, path: &mut String, out: &mut Vec<String>) {
    match (a, b) {
        // Recurse only into objects that are settings *groups*. A map-valued
        // setting like `shell.env` is one setting, not a group of them, and
        // descending into it would report keys that are not in the schema.
        (serde_json::Value::Object(ma), serde_json::Value::Object(mb))
            if is_group(path.as_str()) =>
        {
            let mut names: Vec<&String> = ma.keys().chain(mb.keys()).collect();
            names.sort();
            names.dedup();
            for name in names {
                let len = path.len();
                if !path.is_empty() {
                    path.push('.');
                }
                path.push_str(name);
                let null = serde_json::Value::Null;
                walk(ma.get(name).unwrap_or(&null), mb.get(name).unwrap_or(&null), path, out);
                path.truncate(len);
            }
        }
        _ => {
            if a != b {
                out.push(path.clone());
            }
        }
    }
}

/// Whether a path names a group of settings rather than a single setting.
///
/// The root is a group, and so is anything that is a struct in the tree. Keeping
/// this explicit is what stops `shell.env` and `profiles` — both maps — from
/// being walked into and producing per-entry keys the schema has never heard of.
fn is_group(path: &str) -> bool {
    matches!(
        path,
        "" | "typography"
            | "appearance"
            | "window"
            | "tabs"
            | "shell"
            | "scrolling"
            | "cursor"
            | "motion"
    )
}

/// What a change from `old` to `new` costs.
#[must_use]
pub fn diff(old: &Settings, new: &Settings) -> Invalidation {
    changed_keys(old, new)
        .iter()
        .map(|k| class_of(k))
        .fold(Invalidation::None, Invalidation::max)
}

/// The class for one key, or the most expensive class if it is unknown.
///
/// An unrecognized key means someone added a setting without adding it to
/// [`KEYS`]. Assuming `Restart` is the honest answer: the change may well not
/// have applied, and saying so beats silently reporting `Free` and leaving the
/// user to wonder why nothing happened. The schema-coverage test makes this
/// unreachable in practice.
#[must_use]
pub fn class_of(key: &str) -> Invalidation {
    KEYS.iter()
        .find(|(k, _)| *k == key)
        .map_or(Invalidation::Restart, |(_, c)| *c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::Backdrop;

    #[test]
    fn no_change_costs_nothing() {
        let s = Settings::default();
        assert_eq!(diff(&s, &s), Invalidation::None);
        assert!(changed_keys(&s, &s).is_empty());
    }

    #[test]
    fn a_theme_change_is_free() {
        let a = Settings::default();
        let mut b = a.clone();
        b.appearance.theme = "nord".into();
        assert_eq!(changed_keys(&a, &b), vec!["appearance.theme"]);
        assert_eq!(diff(&a, &b), Invalidation::Free);
    }

    #[test]
    fn line_height_resizes_the_pty() {
        // The whole point of the Geometry class: the shell has to be told, or
        // it keeps drawing for a screen that no longer exists.
        let a = Settings::default();
        let mut b = a.clone();
        b.typography.line_height = 1.6;
        assert_eq!(diff(&a, &b), Invalidation::Geometry);
    }

    #[test]
    fn a_font_feature_bumps_the_atlas_but_not_the_grid() {
        let a = Settings::default();
        let mut b = a.clone();
        b.typography.features = vec!["ss01".into()];
        assert_eq!(diff(&a, &b), Invalidation::AtlasBump);
    }

    #[test]
    fn opacity_rebuilds_the_surface() {
        let a = Settings::default();
        let mut b = a.clone();
        b.window.backdrop = Backdrop::Mica;
        assert_eq!(diff(&a, &b), Invalidation::SurfaceRebuild);
    }

    #[test]
    fn moving_the_tab_strip_is_geometry() {
        // The strip takes its space from the grid: moving it from top to left
        // trades rows for columns, and the shell has to hear about both.
        let a = Settings::default();
        let mut b = a.clone();
        b.tabs.position = crate::settings::TabsPosition::Left;
        assert_eq!(changed_keys(&a, &b), vec!["tabs.position"]);
        assert_eq!(diff(&a, &b), Invalidation::Geometry);
    }

    #[test]
    fn tab_restore_costs_nothing_until_the_next_launch() {
        // Not `Restart`: nothing in the running process is stale — the flag is
        // simply read at the moment it matters.
        let a = Settings::default();
        let mut b = a.clone();
        b.tabs.restore = false;
        assert_eq!(diff(&a, &b), Invalidation::None);
    }

    #[test]
    fn a_mixed_change_costs_the_worst_of_them() {
        let a = Settings::default();
        let mut b = a.clone();
        b.appearance.theme = "nord".into(); // Free
        b.typography.size_pt = 18.0; // Geometry
        assert_eq!(diff(&a, &b), Invalidation::Geometry);
        assert_eq!(changed_keys(&a, &b).len(), 2);
    }

    #[test]
    fn a_map_valued_setting_is_one_key_not_many() {
        // Descending into `shell.env` would report `shell.env.PATH`, which the
        // schema has never heard of, and the coverage test would fail on a key
        // that is not a setting.
        let a = Settings::default();
        let mut b = a.clone();
        b.shell.env.insert("FOO".into(), "1".into());
        assert_eq!(changed_keys(&a, &b), vec!["shell.env"]);
    }

    #[test]
    fn an_unknown_key_is_reported_as_needing_a_restart() {
        assert_eq!(class_of("something.nobody.wired"), Invalidation::Restart);
    }

    #[test]
    fn a_profile_edit_is_free() {
        // A profile is data the launcher and the profiles editor re-read on the
        // next resolve; nothing in the running process caches it. Restart here
        // would make every edit in the profiles tab claim a relaunch it does
        // not need.
        let a = Settings::default();
        let mut b = a.clone();
        b.profiles.insert("k8s-prod".into(), toml::Table::new());
        assert_eq!(
            diff(&a, &b),
            Invalidation::Free,
            "a profile edit must re-resolve live, not demand a restart"
        );
    }
}

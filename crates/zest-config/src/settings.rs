//! The settings tree.
//!
//! Every field carries a doc comment, and that is not decoration: `schemars`
//! turns `#[doc]` into the JSON Schema `description`, and the web and phone
//! settings UIs are *generated* from that schema. A field without a doc comment
//! ships to three clients as a bare key with no explanation, and nobody notices
//! until a user asks what it does.
//!
//! The `x_zest_*` extensions carry UI metadata — grouping, widget hint, whether
//! a change needs a restart — next to the field rather than in a separate file
//! that would rot the first time someone adds a setting.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Bumped when a migration is needed, never for an additive change.
///
/// Adding a field with a `#[serde(default)]` is not a schema change: old files
/// keep loading. Only renames, removals and meaning changes need a bump and a
/// matching step in [`crate::migrate`].
pub const CURRENT_SCHEMA_VERSION: u32 = 1;

/// The whole configuration, after merging every layer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Settings {
    /// Config format version. Files without one are assumed to be current.
    pub schema_version: u32,
    /// Fonts, size, and the metrics derived from them.
    pub typography: Typography,
    /// Colours and the theme they come from.
    pub appearance: Appearance,
    /// Window shape, transparency, and chrome.
    pub window: Window,
    /// The tab strip: where it lives and how it behaves.
    pub tabs: Tabs,
    /// What to run, and where.
    pub shell: Shell,
    /// Scrollback and scrolling behaviour.
    pub scrolling: Scrolling,
    /// Cursor shape and blink.
    pub cursor: Cursor,
    /// Animation, and how much of it.
    pub motion: Motion,
    /// Named overrides, selected at launch.
    ///
    /// A profile is the whole settings tree again, partially specified. The
    /// `k8s-prod` profile with a red theme is a genuinely useful safety feature,
    /// not a toy.
    ///
    /// Schematized as free-form objects: a profile is the settings tree again,
    /// partially specified, and a schema that said so would be recursive. The
    /// settings UI edits profiles through the same generated form as the root.
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    #[schemars(with = "std::collections::BTreeMap<String, serde_json::Value>")]
    pub profiles: std::collections::BTreeMap<String, toml::Table>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            typography: Typography::default(),
            appearance: Appearance::default(),
            window: Window::default(),
            tabs: Tabs::default(),
            shell: Shell::default(),
            scrolling: Scrolling::default(),
            cursor: Cursor::default(),
            motion: Motion::default(),
            profiles: std::collections::BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Typography {
    /// Font families in preference order; the first one with a glyph wins.
    #[schemars(extend("x_zest_group" = "Text", "x_zest_widget" = "font-list"))]
    pub families: Vec<String>,
    /// Font size in points.
    #[schemars(range(min = 4.0, max = 144.0))]
    #[schemars(extend("x_zest_group" = "Text", "x_zest_widget" = "number"))]
    pub size_pt: f32,
    /// Line height as a multiple of the font's natural height.
    ///
    /// This is a *geometry* setting: changing it changes the cell height, which
    /// changes how many rows fit, which must resize the pty.
    #[schemars(range(min = 0.5, max = 3.0))]
    #[schemars(extend("x_zest_group" = "Text", "x_zest_widget" = "number"))]
    pub line_height: f32,
    /// Extra space between cells, in pixels. Also geometry.
    #[schemars(range(min = -5.0, max = 20.0))]
    #[schemars(extend("x_zest_group" = "Text", "x_zest_widget" = "number"))]
    pub letter_spacing: f32,
    /// OpenType features, `liga`-style. Prefix with `-` to disable.
    ///
    /// Part of the shaped-run cache key — a stale entry here produces glyphs
    /// that are correct for the previous setting and maddening to debug.
    #[schemars(extend("x_zest_group" = "Text", "x_zest_widget" = "tag-list"))]
    pub features: Vec<String>,
    /// Programming ligatures. Off by default; it is a per-run decision, so
    /// turning it on later costs nothing.
    #[schemars(extend("x_zest_group" = "Text", "x_zest_widget" = "toggle"))]
    pub ligatures: bool,
}

impl Default for Typography {
    fn default() -> Self {
        Self {
            // Ordered so each platform hits its own preferred face before the
            // generic. The macOS entries are only ever reached when the Windows
            // ones are absent, so this leaves the Windows result unchanged.
            // `JetBrainsMono Nerd Font` sits ahead of `Menlo` because Menlo has
            // no Private Use Area coverage, and a prompt built out of Nerd Font
            // icons renders as blank boxes without it.
            families: [
                "Cascadia Mono",
                "Consolas",
                "JetBrainsMono Nerd Font",
                "Menlo",
                "DejaVu Sans Mono",
                "monospace",
            ]
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
            size_pt: 13.0,
            line_height: 1.25,
            letter_spacing: 0.0,
            features: Vec::new(),
            ligatures: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Appearance {
    /// Theme id, from the built-ins or the user's theme directory.
    #[schemars(extend("x_zest_group" = "Appearance", "x_zest_widget" = "theme-picker"))]
    pub theme: String,
    /// Theme used when the OS reports a light appearance.
    ///
    /// Empty means "follow `theme` regardless", which is what someone who has
    /// deliberately chosen a dark theme expects.
    #[schemars(extend("x_zest_group" = "Appearance", "x_zest_widget" = "theme-picker"))]
    pub light_theme: String,
    /// Follow the OS light/dark setting.
    #[schemars(extend("x_zest_group" = "Appearance", "x_zest_widget" = "toggle"))]
    pub follow_system_theme: bool,
    /// Stem darkening for text on dark backgrounds.
    ///
    /// Applied once in the resolve pass rather than baked into the atlas, so
    /// tuning it is a repaint rather than a re-rasterization.
    #[schemars(range(min = 0.5, max = 2.5))]
    #[schemars(extend("x_zest_group" = "Appearance", "x_zest_widget" = "number"))]
    pub text_gamma: f32,
    /// Additional contrast applied at resolve time.
    #[schemars(range(min = 0.0, max = 1.0))]
    #[schemars(extend("x_zest_group" = "Appearance", "x_zest_widget" = "number"))]
    pub text_contrast: f32,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme: "obsidian".to_string(),
            light_theme: String::new(),
            follow_system_theme: false,
            text_gamma: 1.0,
            text_contrast: 0.0,
        }
    }
}

/// What the compositor puts behind the window.
///
/// Separate from [`Window::opacity`] on purpose. Merging them makes "opaque
/// Mica-tinted chrome over an opaque grid" inexpressible, and that is a real and
/// popular look.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum Backdrop {
    /// No platform backdrop; `opacity` alone decides.
    None,
    /// Windows 11 Mica.
    Mica,
    /// Windows 11 Mica Alt, the tabbed-app variant.
    MicaAlt,
    /// Acrylic blur. Undocumented on Windows; best-effort on Linux.
    Acrylic,
    /// macOS `NSVisualEffectView` vibrancy.
    Vibrancy,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Window {
    /// Background opacity, applied *only* to cells with a default background.
    ///
    /// Applying it to explicit backgrounds too would double-darken and make
    /// `ls` colours and TUI panels see-through, which is the single most common
    /// transparency bug.
    #[schemars(range(min = 0.0, max = 1.0))]
    #[schemars(extend("x_zest_group" = "Window", "x_zest_widget" = "slider"))]
    pub opacity: f32,
    /// Chrome opacity, independent of the grid's.
    #[schemars(range(min = 0.0, max = 1.0))]
    #[schemars(extend("x_zest_group" = "Window", "x_zest_widget" = "slider"))]
    pub chrome_opacity: f32,
    /// Platform backdrop material.
    #[schemars(extend("x_zest_group" = "Window", "x_zest_widget" = "select"))]
    pub backdrop: Backdrop,
    /// Padding between the window edge and the grid, in logical pixels.
    #[schemars(range(min = 0, max = 64))]
    #[schemars(extend("x_zest_group" = "Window", "x_zest_widget" = "number"))]
    pub padding: u32,
    /// Draw our own titlebar and tab strip instead of the system's.
    #[schemars(extend(
        "x_zest_group" = "Window",
        "x_zest_widget" = "toggle",
        "x_zest_restart" = true
    ))]
    pub custom_chrome: bool,
    /// Initial size in cells.
    #[schemars(extend("x_zest_group" = "Window", "x_zest_widget" = "number"))]
    pub columns: u16,
    /// Initial size in cells.
    #[schemars(extend("x_zest_group" = "Window", "x_zest_widget" = "number"))]
    pub rows: u16,
}

impl Default for Window {
    fn default() -> Self {
        Self {
            opacity: 1.0,
            chrome_opacity: 1.0,
            backdrop: Backdrop::None,
            padding: 8,
            custom_chrome: false,
            columns: 100,
            rows: 30,
        }
    }
}

/// Where the tab strip lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TabsPosition {
    /// A horizontal strip along the top of the window.
    Top,
    /// A vertical sidebar on the left, wide enough to show each tab's host.
    Left,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Tabs {
    /// Where the tab strip lives: a horizontal strip at the top, or a vertical
    /// sidebar on the left.
    #[schemars(extend("x_zest_group" = "Tabs", "x_zest_widget" = "select"))]
    pub position: TabsPosition,
    /// Height of the horizontal tab strip, in logical pixels.
    #[schemars(range(min = 24, max = 64))]
    #[schemars(extend("x_zest_group" = "Tabs", "x_zest_widget" = "number"))]
    pub strip_height: u32,
    /// Width of the vertical tab sidebar, in logical pixels.
    #[schemars(range(min = 120, max = 400))]
    #[schemars(extend("x_zest_group" = "Tabs", "x_zest_widget" = "number"))]
    pub sidebar_width: u32,
    /// Show the tab strip when only one tab is open.
    ///
    /// On by default: the strip doubles as the titlebar, and a window whose
    /// chrome appears and disappears as tabs come and go is unsettling.
    #[schemars(extend("x_zest_group" = "Tabs", "x_zest_widget" = "toggle"))]
    pub show_single_tab: bool,
    /// Reattach this window's tabs on the next launch.
    ///
    /// When off, every launch starts with one fresh local shell and existing
    /// sessions are reachable only through the picker.
    #[schemars(extend("x_zest_group" = "Tabs", "x_zest_widget" = "toggle"))]
    pub restore: bool,
}

impl Default for Tabs {
    fn default() -> Self {
        Self {
            // 46 / 262: the client-UI design's fixed sizes
            // (docs/design/client-ui/README.md, "Spacing, radii, shadows").
            position: TabsPosition::Top,
            strip_height: 46,
            sidebar_width: 262,
            show_single_tab: true,
            restore: true,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Shell {
    /// Command line to run. Empty means the platform's default shell.
    ///
    /// This is why a workspace-local config file cannot be trusted
    /// automatically: it is remote code execution wearing a settings hat.
    #[schemars(extend("x_zest_group" = "Shell", "x_zest_widget" = "text"))]
    pub command: String,
    /// Working directory. Empty inherits.
    #[schemars(extend("x_zest_group" = "Shell", "x_zest_widget" = "path"))]
    pub cwd: String,
    /// Extra environment entries. An empty value *unsets* the variable.
    #[schemars(extend("x_zest_group" = "Shell", "x_zest_widget" = "key-value"))]
    pub env: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Scrolling {
    /// Lines of history retained per session.
    #[schemars(range(min = 0, max = 10_000_000))]
    #[schemars(extend("x_zest_group" = "Scrolling", "x_zest_widget" = "number"))]
    pub scrollback: usize,
    /// Lines moved per wheel notch.
    #[schemars(range(min = 1, max = 50))]
    #[schemars(extend("x_zest_group" = "Scrolling", "x_zest_widget" = "number"))]
    pub lines_per_notch: usize,
    /// Jump to the bottom whenever the program writes.
    ///
    /// Off by default: with it on, scrollback cannot be read while anything is
    /// running, because every emitted line yanks the view away.
    #[schemars(extend("x_zest_group" = "Scrolling", "x_zest_widget" = "toggle"))]
    pub scroll_on_output: bool,
    /// Jump to the bottom when a key is pressed. On by default, as everywhere.
    #[schemars(extend("x_zest_group" = "Scrolling", "x_zest_widget" = "toggle"))]
    pub scroll_on_keypress: bool,
}

impl Default for Scrolling {
    fn default() -> Self {
        Self {
            scrollback: 10_000,
            lines_per_notch: 3,
            scroll_on_output: false,
            scroll_on_keypress: true,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CursorShape {
    Block,
    Underline,
    Bar,
}

/// How the cursor moves between cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CursorTrail {
    /// Jump straight to the target.
    None,
    /// Spring the cursor rect toward the target.
    Smooth,
    /// The Neovide-style tapered smear. Deliberately not the default.
    Smear,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Cursor {
    /// Shape, unless the program sets one with DECSCUSR.
    #[schemars(extend("x_zest_group" = "Cursor", "x_zest_widget" = "select"))]
    pub shape: CursorShape,
    /// Blink the cursor.
    ///
    /// Blinking is the most common way a terminal loses its 0%-idle claim: it
    /// must stop on focus loss and wake on a timer, never spin.
    #[schemars(extend("x_zest_group" = "Cursor", "x_zest_widget" = "toggle"))]
    pub blink: bool,
    /// Blink interval in milliseconds.
    #[schemars(range(min = 100, max = 5000))]
    #[schemars(extend("x_zest_group" = "Cursor", "x_zest_widget" = "number"))]
    pub blink_interval_ms: u64,
    /// Motion between cells.
    #[schemars(extend("x_zest_group" = "Cursor", "x_zest_widget" = "select"))]
    pub trail: CursorTrail,
}

impl Default for Cursor {
    fn default() -> Self {
        Self {
            shape: CursorShape::Block,
            blink: true,
            blink_interval_ms: 530,
            trail: CursorTrail::None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Motion {
    /// Animate at all. The OS accessibility setting can still override this.
    #[schemars(extend("x_zest_group" = "Motion", "x_zest_widget" = "toggle"))]
    pub enabled: bool,
    /// Honour the OS "reduce motion" setting.
    #[schemars(extend("x_zest_group" = "Motion", "x_zest_widget" = "toggle"))]
    pub respect_system_reduce_motion: bool,
    /// Smooth scrolling as a fractional row offset.
    ///
    /// Suppressed in the alternate screen regardless: `vim` and `less` scroll by
    /// design, and animating that fights the program.
    #[schemars(extend("x_zest_group" = "Motion", "x_zest_widget" = "toggle"))]
    pub smooth_scroll: bool,
    /// Spring response in seconds — roughly, time to reach the target.
    #[schemars(range(min = 0.01, max = 2.0))]
    #[schemars(extend("x_zest_group" = "Motion", "x_zest_widget" = "number"))]
    pub spring_response: f32,
    /// Spring damping ratio. 1.0 is critically damped; below that overshoots.
    #[schemars(range(min = 0.1, max = 2.0))]
    #[schemars(extend("x_zest_group" = "Motion", "x_zest_widget" = "number"))]
    pub spring_damping: f32,
}

impl Default for Motion {
    fn default() -> Self {
        Self {
            enabled: true,
            respect_system_reduce_motion: true,
            smooth_scroll: true,
            spring_response: 0.16,
            spring_damping: 1.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip_through_toml() {
        let s = Settings::default();
        let text = toml::to_string(&s).expect("serialize");
        let back: Settings = toml::from_str(&text).expect("deserialize");
        assert_eq!(s, back);
    }

    #[test]
    fn an_empty_file_is_the_defaults() {
        // Someone creating `config.toml` and saving it empty must get exactly
        // what they had before, not a crash and not a different terminal.
        let s: Settings = toml::from_str("").expect("empty file");
        assert_eq!(s, Settings::default());
    }

    #[test]
    fn a_partial_file_only_changes_what_it_names() {
        let s: Settings =
            toml::from_str("[typography]\nsize_pt = 20.0\n").expect("partial file");
        assert!((s.typography.size_pt - 20.0).abs() < f32::EPSILON);
        assert_eq!(s.typography.line_height, Typography::default().line_height);
        assert_eq!(s.appearance, Appearance::default());
    }

    #[test]
    fn unknown_keys_warn_rather_than_fail() {
        // A typo, or a key from a newer version, must not cost the user their
        // whole config -- and must not stop the terminal from starting.
        let s: Settings = toml::from_str("[typography]\nsize_pt = 20.0\nnonsense = 3\n")
            .expect("unknown keys are tolerated");
        assert!((s.typography.size_pt - 20.0).abs() < f32::EPSILON);
    }
}

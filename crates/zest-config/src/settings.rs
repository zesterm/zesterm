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
    /// The prompt's context chips.
    pub prompt: Prompt,
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
            prompt: Prompt::default(),
            profiles: std::collections::BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Typography {
    /// Font families in preference order; the first one with a glyph wins.
    ///
    /// A preference order, *not* a coverage mechanism. A character the chosen
    /// face lacks is resolved per-character against the system — DirectWrite,
    /// CoreText, fontconfig — Nerd Font icons come from the discovered symbol
    /// families, and emoji have their own path; none of that consults this
    /// list. The shipped default names several because it has to work on
    /// Windows, macOS and Linux without knowing which it will start on. Choose
    /// a font and one entry is enough, which is what the settings screen
    /// writes.
    #[schemars(extend("x_zest_group" = "Text", "x_zest_widget" = "font-list"))]
    pub families: Vec<String>,
    /// Font size in points, converted to pixels at 96 logical DPI.
    ///
    /// 12pt is 16 physical pixels at 100% scaling, which is what Windows
    /// Terminal, WezTerm and kitty all give you at their defaults. This field
    /// once meant *pixels* despite its name, so the same number produced text
    /// a quarter smaller than every peer — if a config from before that fix
    /// looks too large now, it was previously too small.
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
    /// Cell width as a multiple of the font size. `0` uses the font's own.
    ///
    /// Stated against the size rather than against the face's natural advance,
    /// which is how Windows Terminal states it — most monospace faces sit near
    /// 0.6. Use it to tighten or loosen the grid without changing the type
    /// size, or to make two fonts occupy the same columns.
    ///
    /// `0` means "whatever this face says", which is the only sane default: the
    /// right absolute number depends on the font, so a fixed one would be wrong
    /// for every face but the one it was chosen against. Geometry — changing it
    /// changes how many columns fit, and resizes the pty.
    #[schemars(range(min = 0.0, max = 2.0))]
    #[schemars(extend("x_zest_group" = "Text", "x_zest_widget" = "number"))]
    pub cell_width: f32,
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
    /// Draw box-drawing and block characters at cell size instead of taking the
    /// font's glyphs.
    ///
    /// On by default because the font's glyphs are the wrong shape: one is as wide
    /// as the font's advance, the cell is that advance *rounded*, and the
    /// difference is a gap at every cell boundary — a run of `█` renders as a
    /// picket fence. Turn it off only to get a specific font's own box drawing
    /// back, and expect the seams with it.
    #[schemars(extend("x_zest_group" = "Text", "x_zest_widget" = "toggle"))]
    pub builtin_box_drawing: bool,
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
            size_pt: 12.0,
            line_height: 1.25,
            cell_width: 0.0,
            letter_spacing: 0.0,
            features: Vec::new(),
            ligatures: false,
            builtin_box_drawing: true,
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
    /// Stem darkening for text. 1.0 is perceptually linear; higher is heavier.
    ///
    /// Applied to glyph coverage each frame rather than baked into the atlas, so
    /// tuning it is a repaint rather than a re-rasterization. It affects text
    /// only — backgrounds and chrome come out exactly as the theme specifies.
    ///
    /// Coverage is linearized in the shader, so this composes to
    /// `apparent = pow(coverage, 1/gamma)` and means exactly one thing: how
    /// much heavier than perceptually-linear a stroke should be. More coverage
    /// is more contrast, and contrast is what reads as sharpness — which is why
    /// the default is 2.5 and not something cautious.
    ///
    /// One number serves light and dark alike. That is not what the theory
    /// predicts — dark-on-light is supposed to need far less stem darkening
    /// than light-on-dark, and there is a comment on `ThemeEffects` proposing a
    /// per-theme value on exactly that basis — but 2.5 was tested against a
    /// white background and a dark one and preferred on both. Measured beats
    /// predicted, so there is no per-theme value and no reason to add one.
    ///
    /// The default must stay equal to `zest_render_wgpu::TextTuning::DEFAULT_GAMMA`.
    /// It cannot reference it — `zest-config` does not depend on the renderer —
    /// so `zest-app`'s `the_two_defaults_agree` asserts it instead. Before this
    /// was wired the two numbers were 1.0 and 1.3 and nothing compared them, so
    /// the schema documented a value the renderer never used.
    #[schemars(range(min = 0.5, max = 4.0))]
    #[schemars(extend("x_zest_group" = "Appearance", "x_zest_widget" = "number"))]
    pub text_gamma: f32,
    /// Additional contrast applied to glyph coverage, not to the frame.
    #[schemars(range(min = 0.0, max = 1.0))]
    #[schemars(extend("x_zest_group" = "Appearance", "x_zest_widget" = "number"))]
    pub text_contrast: f32,
    /// How text is antialiased.
    ///
    /// Also decides whether outlines are grid-fitted, because the two are one
    /// decision: swash hard-codes an LCD hinting target and does not let it be
    /// chosen, so hinting always means "grid-fit for a rasterizer with three
    /// times the horizontal resolution". Pairing that with grayscale coverage
    /// changes glyph *shapes* rather than merely softening them.
    #[schemars(extend("x_zest_group" = "Appearance", "x_zest_widget" = "select"))]
    pub text_antialias: TextAntialias,
    /// Whether glyph outlines are snapped to the pixel grid.
    ///
    /// Independent of `text_antialias`, and the mixed settings are the
    /// interesting ones. Grid-fitting matters most where a stem is about one
    /// pixel wide, so it decides everything at 9pt and almost nothing at 16px.
    ///
    /// Measured against Windows Terminal at size 9: `grayscale` + `full` gives
    /// 12.6% ink coverage and 43% fully-saturated pixels against its 11.7% and
    /// 45%, which is as close as these get.
    #[schemars(extend("x_zest_group" = "Appearance", "x_zest_widget" = "select"))]
    pub text_hinting: TextHinting,
}

/// Whether glyph outlines are snapped to the pixel grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TextHinting {
    /// Use outlines as drawn. Softer at small sizes, always faithful in shape.
    ///
    /// Worth choosing on a HiDPI display, where a stem is several pixels wide
    /// and there is nothing for grid-fitting to buy.
    None,
    /// Grid-fit through the font's own TrueType bytecode. **The default.**
    ///
    /// Crisper at small sizes, and what every Windows application looks like.
    /// This is what makes a one-pixel stem land on one pixel instead of
    /// spreading over two, which is the whole difference at 9pt and nearly
    /// nothing at 16px.
    /// The cost is that swash pins its hinting target to horizontal LCD and
    /// will not let it be chosen, so on a ClearType-aware face this grid-fits
    /// horizontally too — sampled once per pixel that changes glyph shapes,
    /// which is what made `w` render as `W` at 13ppem.
    Full,
}

/// How glyph coverage is sampled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum TextAntialias {
    /// Per-channel coverage at thirds of a pixel.
    ///
    /// Keeps horizontal detail a single sample per pixel throws away. Needs an
    /// RGB-striped panel and a GPU that can blend per channel; where either is
    /// missing, grayscale is used instead.
    Subpixel,
    /// One coverage value per pixel. **The default.**
    ///
    /// What Windows Terminal does — measurably: its channel spread on inked
    /// pixels is zero. Paired with `text_hinting = "full"` it matches it
    /// closely at small sizes, 12.6% ink coverage against 11.7% and 43% of
    /// inked pixels fully saturated against 45%. Also the right choice on a
    /// rotated or non-RGB panel, where thirds of a pixel are simply wrong.
    Grayscale,
}

impl Default for Appearance {
    fn default() -> Self {
        Self {
            theme: "obsidian".to_string(),
            light_theme: String::new(),
            follow_system_theme: false,
            text_gamma: 2.5,
            text_contrast: 0.0,
            text_antialias: TextAntialias::Grayscale,
            text_hinting: TextHinting::Full,
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

/// How a background picture is placed inside the pane it decorates.
///
/// Three variants and no more, deliberately: `select_is_segmented` renders a
/// `Select` of three or fewer as a segmented control, which is the shape the
/// client-UI handoff draws for this row (§12). A fourth would silently demote
/// it to a dropdown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum BackgroundFit {
    /// Scale to cover the pane, cropping whichever axis overhangs. The default:
    /// a photograph with a letterbox around it reads as a bug rather than as a
    /// choice.
    Fill,
    /// Scale to fit inside the pane; the slack stays the plain background.
    Fit,
    /// Natural size, in the bottom-right corner. The design's own
    /// recommendation — a watermark in the corner reads better than a
    /// full-bleed photo behind text.
    Watermark,
}

/// Who draws the titlebar.
///
/// Three states rather than a bool, because the right answer differs by
/// platform and the schema may not: `schemars` derives the default from
/// [`Window::default`], so a `cfg!(windows)` default would make
/// `schemas/zesterm.schema.json` itself platform-dependent and
/// `cargo xtask check-schema` would fail on two of the three CI legs. `Auto`
/// is one value everywhere and resolves per platform where it is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CustomChrome {
    /// The platform's own answer: borderless on Windows, where the OS caption
    /// otherwise sits above our tab strip and the window wears two titlebars;
    /// a transparent full-size titlebar on macOS, which keeps the traffic
    /// lights, native fullscreen and Sequoia tiling; server-side decorations
    /// on Linux, where the compositor is the only thing that knows what it
    /// draws and winit has already asked it.
    Auto,
    On,
    Off,
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
    /// Picture drawn behind the cells. Empty draws none.
    ///
    /// A relative path resolves against the config directory, so a config that
    /// travels with its pictures keeps working on another machine.
    #[schemars(extend("x_zest_group" = "Window", "x_zest_widget" = "path"))]
    pub background_image: String,
    /// How the picture is placed in the pane.
    #[schemars(extend("x_zest_group" = "Window", "x_zest_widget" = "select"))]
    pub background_fit: BackgroundFit,
    /// How far the picture is faded toward the background. 1 hides it entirely.
    #[schemars(range(min = 0.0, max = 1.0))]
    #[schemars(extend("x_zest_group" = "Window", "x_zest_widget" = "slider"))]
    pub background_dim: f32,
    /// Padding between the window edge and the grid, in logical pixels.
    #[schemars(range(min = 0, max = 64))]
    #[schemars(extend("x_zest_group" = "Window", "x_zest_widget" = "number"))]
    pub padding: u32,
    /// Draw our own titlebar and tab strip instead of the system's.
    #[schemars(extend(
        "x_zest_group" = "Window",
        "x_zest_widget" = "select",
        "x_zest_restart" = true
    ))]
    pub custom_chrome: CustomChrome,
    /// Initial size in cells, for a window that remembers no size of its
    /// own: a restored window comes back the size it was closed at.
    #[schemars(extend("x_zest_group" = "Window", "x_zest_widget" = "number"))]
    pub columns: u16,
    /// Initial size in cells, for a window that remembers no size of its
    /// own: a restored window comes back the size it was closed at.
    #[schemars(extend("x_zest_group" = "Window", "x_zest_widget" = "number"))]
    pub rows: u16,
    /// What a second `zesterm` launch does while one is already running.
    /// `--new-window`, `--new-tab` and `--new-instance` override it for one
    /// launch.
    #[schemars(extend("x_zest_group" = "Window", "x_zest_widget" = "select"))]
    pub launch: LaunchTarget,
}

/// Where a launch lands when zesterm is already running. The values are
/// the flag suffixes (`--new-window` ⇔ `"window"`), so the flag and the
/// setting need no table between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum LaunchTarget {
    /// A new window of the running zesterm — what Warp and Windows Terminal
    /// do by default.
    Window,
    /// A new tab in the running zesterm's focused window.
    Tab,
    /// A separate process, exactly as if none were running.
    Instance,
}

impl Default for Window {
    fn default() -> Self {
        Self {
            opacity: 1.0,
            chrome_opacity: 1.0,
            backdrop: Backdrop::None,
            background_image: String::new(),
            background_fit: BackgroundFit::Fill,
            // Half, not zero. Nobody sets a picture and then goes looking for
            // the dim slider; they set one, find the text unreadable over it,
            // and conclude the feature is broken. The first thing this can
            // show has to be legible, which is the design copy's own point
            // about a watermark reading better than a full-bleed photo.
            background_dim: 0.5,
            padding: 8,
            custom_chrome: CustomChrome::Auto,
            columns: 100,
            rows: 30,
            launch: LaunchTarget::Window,
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

/// What closing a tab does to a session that runs on this machine.
///
/// Only local sessions have a choice to make. A remote one always detaches —
/// a window here closing must not end a shell there, which is the whole point
/// of the fleet (ADR-007) — and a dead one has nothing left to end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum CloseAction {
    /// End the shell, as every ordinary terminal does. The default, because
    /// it is what the hand already expects from ⌘W.
    Kill,
    /// Leave it running in the daemon and stop watching it. The session is
    /// still there in the picker, and on the next launch, and from a phone.
    Detach,
    /// Ask every time.
    Ask,
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
    /// Mark a tab when a program in it rings the bell (`BEL`).
    ///
    /// The oldest "look at me" a terminal has, and the one most agent and
    /// build tools already emit — which is the point: nothing here knows the
    /// name of any program, only that one chose to say something.
    #[schemars(extend("x_zest_group" = "Tabs", "x_zest_widget" = "toggle"))]
    pub attention_bell: bool,
    /// Mark a tab when a program in it asks for a desktop notification
    /// (`OSC 9`, `OSC 777;notify`).
    ///
    /// Its own switch rather than sharing the bell's, because the two are
    /// noisy in different situations — a bell fires on tab-completion in some
    /// shells, a notification almost never fires by accident.
    #[schemars(extend("x_zest_group" = "Tabs", "x_zest_widget" = "toggle"))]
    pub attention_notify: bool,
    /// What closing a tab does to a shell that runs on this machine: end it,
    /// leave it running in the daemon, or ask.
    ///
    /// Closing the *window* has always detached everything, this machine's
    /// shells included — so `kill` is ⌘W disagreeing with ⌘Q on purpose,
    /// because that is what the hand expects of it.
    #[schemars(extend("x_zest_group" = "Tabs", "x_zest_widget" = "select"))]
    pub close_action: CloseAction,
    /// Ask before closing a tab with a command running or a full-screen
    /// program on the alternate screen.
    ///
    /// Independent of `close_action`: it is the question "are you sure", not
    /// the question "which of these did you mean", and someone who wants
    /// `kill` still wants to be stopped before ⌘W ends a build.
    #[schemars(extend("x_zest_group" = "Tabs", "x_zest_widget" = "toggle"))]
    pub confirm_close_when_busy: bool,
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
            attention_bell: true,
            attention_notify: true,
            // Today's behaviour, kept as the default: someone who never opens
            // Settings must not find ⌘W has quietly started meaning something
            // else.
            close_action: CloseAction::Kill,
            confirm_close_when_busy: true,
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

/// Whether a keystroke's echo is guessed before the host confirms it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "kebab-case")]
pub enum PredictEcho {
    /// Guess once the link measures slow enough for it to help (above ~40 ms).
    Auto,
    /// Guess on every remote session, however fast the link.
    Always,
    /// Never guess.
    Off,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Cursor {
    /// Shape, unless the program sets one with DECSCUSR.
    #[schemars(extend("x_zest_group" = "Cursor", "x_zest_widget" = "select"))]
    pub shape: CursorShape,
    /// Show what a keystroke will echo as before the remote host confirms it
    /// — dim and underlined, taken back if the host disagrees. What makes a
    /// session over the relay feel local. Local sessions never guess.
    #[schemars(extend("x_zest_group" = "Cursor", "x_zest_widget" = "select"))]
    pub predict_echo: PredictEcho,
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
            predict_echo: PredictEcho::Auto,
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

/// The context chips beside the live prompt: which show, in what order.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct Prompt {
    /// Which chips show, in this order. Known names: `cwd`, `git`, `venv`,
    /// `conda`, `kube`, `aws`, `node`, `ssh`, `exit`, `link`. An unknown name shows
    /// nothing rather than erroring, so a list written for a newer zesterm
    /// still loads. Empty turns the chips off entirely.
    ///
    /// This chooses what this window *shows*; what is *true* is computed by
    /// each session's own daemon and shipped on the listing, which is what
    /// lets a browser render the same chips for a machine on the other side
    /// of the relay.
    #[schemars(extend("x_zest_group" = "Prompt", "x_zest_widget" = "tag-list"))]
    pub widgets: Vec<String>,
    /// Let the chips *be* the prompt: new shells get a PS1 of just `❯`, on
    /// its own line, so the cwd and branch live only in the chips above it.
    ///
    /// On by default (#435): the chips are the product's prompt, and off by
    /// default they were invisible to exactly the people they were built
    /// for. The blast radius is only the default-PS1 crowd — the injected
    /// integration declines whenever a framework owns PS1 (powerlevel10k,
    /// starship, oh-my-posh rebuild it every prompt, and a fight the user
    /// did not pick is worse than a long prompt), and anyone attached to
    /// their stock PS1 turns this off. Existing sessions keep the prompt
    /// they started with; the shell reads this once, at spawn, on the
    /// machine that runs it.
    #[schemars(extend("x_zest_group" = "Prompt", "x_zest_widget" = "toggle"))]
    pub compact_ps1: bool,
}

impl Default for Prompt {
    fn default() -> Self {
        Self {
            // `cwd` and `git` are the two everyone crams into PS1; `venv` is
            // the one people forget they are in; `exit` only shows on
            // failure. `kube`/`aws` stay opt-in — a chip that is noise for
            // most people is how the whole row gets turned off.
            widgets: ["cwd", "git", "venv", "exit"].map(String::from).to_vec(),
            compact_ps1: true,
        }
    }
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

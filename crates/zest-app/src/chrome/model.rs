//! What the chrome shows, as data.
//!
//! The model is built by the app from live state (tabs, roster, hover) and
//! consumed by `layout`, which is pure. Nothing here knows about the GPU, the
//! window, or the network — that is what makes the layout tests meaningful.

use zest_config::settings::TabsPosition;
use zest_proto::SessionAddr;

use super::hit::HitRegion;

/// How reachable a tab's host currently looks.
///
/// A projection of `zest_mesh::Presence` so the chrome does not grow a mesh
/// dependency for four names. `Unreachable` is the one that changes drawing:
/// the tab stays put and says so, because a session on a sleeping laptop is
/// not gone (#22, #23).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabPresence {
    Online,
    Away,
    Unseen,
    Unreachable,
}

/// Which machine a tab's shell runs on, as the chrome should say it.
///
/// Origin is displayed with *text*, not colour alone — the class of mistake
/// this UI exists to prevent is acting on the wrong machine, and colour is
/// the first thing a theme change or colour-blindness takes away.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabOrigin {
    Local,
    Remote { host_label: String },
}

/// One tab, ready to draw.
#[derive(Debug, Clone, PartialEq)]
pub struct TabModel {
    pub addr: SessionAddr,
    /// Already derived: OSC title, else cwd basename, else "shell".
    pub title: String,
    pub origin: TabOrigin,
    pub presence: TabPresence,
    /// An attach or restore is in flight; the tab shows itself but cannot be
    /// typed into yet.
    pub connecting: bool,
}

/// One row of the fleet picker, ready to draw.
///
/// Display-only: the app keeps a parallel list of *actions* built in the
/// same pass, so row index `n` here and there mean the same thing by
/// construction — the drift the hit map exists to prevent, applied to rows.
#[derive(Debug, Clone, PartialEq)]
pub enum PickerRow {
    /// A machine, with its presence spelled out.
    Host { label: String, presence: TabPresence },
    /// A session on the host above.
    Session { title: String, detail: String, attached: bool, attached_here: bool },
    /// "New session on <label>".
    CreateOn { label: String },
}

/// The fleet picker, when open.
#[derive(Debug, Clone, PartialEq)]
pub struct PickerModel {
    pub rows: Vec<PickerRow>,
    /// Index into `rows` the keyboard is on.
    pub selected: usize,
    /// The live filter string, drawn in the search line.
    pub filter: String,
    /// Scroll offset of the row list, physical pixels; layout clamps it.
    pub scroll: f32,
}

/// One line of the shortcuts sheet: a name and the chord that does it.
#[derive(Debug, Clone, PartialEq)]
pub struct ShortcutRow {
    pub name: String,
    /// Already platform-spelled by `keymap::chord_label` — the chrome draws
    /// strings, it does not know what a modifier is.
    pub chord: String,
}

/// A titled group of shortcut rows, with an optional footnote.
#[derive(Debug, Clone, PartialEq)]
pub struct ShortcutSection {
    pub title: String,
    pub rows: Vec<ShortcutRow>,
    /// A faint line under the section — the "both chords work" fact.
    pub note: Option<String>,
}

/// The shortcuts sheet, when open.
#[derive(Debug, Clone, PartialEq)]
pub struct ShortcutsModel {
    /// Pre-filtered by the app; empty sections never arrive here.
    pub sections: Vec<ShortcutSection>,
    pub filter: String,
    /// Scroll offset, physical pixels; layout clamps it.
    pub scroll: f32,
}

/// The value half of a settings row, as it should be drawn.
///
/// Which cell a field gets is the row builder's decision (from the schema's
/// widget hint); the chrome just draws what arrives.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingsValueCell {
    Toggle { on: bool },
    /// A chosen option, e.g. a theme id or an enum variant.
    Select { value: String },
    /// A bounded number: the filled fraction and its numeric text.
    Slider { frac: f32, text: String },
    /// A scalar drawn as plain text (numbers, strings, paths).
    Text { text: String },
    /// A list-shaped value the overlay displays but does not edit (yet);
    /// drawn faint to say so.
    ReadOnly { text: String },
}

/// One row of the settings overlay, ready to draw.
///
/// Display-only, like [`PickerRow`]: the app keeps a parallel action list
/// built in the same pass, so index `n` means the same thing to the renderer
/// and the input path by construction.
#[derive(Debug, Clone, PartialEq)]
pub enum SettingsRowModel {
    /// A group header ("Text", "Window", …).
    Group { title: String },
    /// One setting.
    Setting {
        /// Humanized field name ("Font size").
        label: String,
        /// The dotted key, drawn faint — it is what the user greps their
        /// config for.
        key: String,
        /// First line of the field's doc comment.
        description: String,
        value: SettingsValueCell,
        /// `("set by profile `k8s`", warn)` — warn when the source outranks
        /// the user's file, because an edit there would be shadowed.
        provenance: Option<(String, bool)>,
        /// Changing this applies on the next launch.
        restart: bool,
        /// Declared in the schema but not consumed by the app yet.
        inert: bool,
        /// Differs from the schema default.
        modified: bool,
    },
}

/// The settings overlay, when open.
#[derive(Debug, Clone, PartialEq)]
pub struct SettingsModel {
    pub rows: Vec<SettingsRowModel>,
    /// Index into `rows` the keyboard is on.
    pub selected: usize,
    pub filter: String,
    /// Scroll offset, physical pixels; layout clamps it.
    pub scroll: f32,
    /// Bring the selected row into view this pass. Set after keyboard
    /// navigation only — the wheel must scroll freely without the view
    /// snapping back to the selection.
    pub ensure_visible: bool,
}

/// Everything `layout` needs to draw the chrome once.
#[derive(Debug, Clone, PartialEq)]
pub struct ChromeModel {
    pub tabs: Vec<TabModel>,
    /// Index into `tabs`.
    pub active: usize,
    pub position: TabsPosition,
    /// Scroll offset of the tab strip contents, physical pixels. Layout
    /// clamps it and reports the clamped value back.
    pub strip_scroll: f32,
    /// What the pointer is over, from last frame's hit map. Only used for
    /// hover fills, so one frame of lag is invisible.
    pub hover: Option<HitRegion>,
    /// Size of the macOS traffic-light cluster in physical pixels, when the
    /// buttons overlap the chrome. `None` in fullscreen (they auto-hide) and
    /// on every other platform.
    pub traffic_inset: Option<[f32; 2]>,
    pub focused: bool,
    /// The fleet picker, drawn over everything when open.
    pub picker: Option<PickerModel>,
    /// The shortcuts sheet, likewise modal. The app enforces that at most
    /// one overlay is open, so layout never has to rank them.
    pub shortcuts: Option<ShortcutsModel>,
    /// The settings overlay, likewise modal and likewise exclusive.
    pub settings: Option<SettingsModel>,
}

/// The knobs `layout` reads, resolved to physical pixels by the caller.
///
/// Text measurement comes in as data too: the pure layout cannot shape, so
/// the app measures the strings it is about to lay out (via
/// `zest_render_wgpu::measure_ui_run`) and the tests measure with arithmetic.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChromeMetrics {
    /// Window size, physical pixels.
    pub width: f32,
    pub height: f32,
    /// Physical pixels per logical pixel.
    pub scale: f32,
    /// `tabs.strip_height`, logical.
    pub strip_height: f32,
    /// `tabs.sidebar_width`, logical.
    pub sidebar_width: f32,
    /// Height of one line of UI text, physical (the grid's cell height).
    pub line_height: f32,
    /// Baseline offset from the top of a text line, physical.
    pub baseline: f32,
}

impl ChromeMetrics {
    /// The strip's extent in physical pixels along its defining axis.
    #[must_use]
    pub fn strip_extent(&self, position: TabsPosition) -> f32 {
        match position {
            TabsPosition::Top => self.strip_height * self.scale,
            TabsPosition::Left => self.sidebar_width * self.scale,
        }
    }
}

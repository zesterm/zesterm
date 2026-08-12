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

/// What kind of thing a tab is: a session, or one of the app's own screens.
///
/// App tabs (design §11) are ordinary tabs in the strip — same geometry, same
/// active treatment — but they size to their content, carry no close
/// affordance and no accent wash: they are places, not shells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TabKind {
    Session,
    Settings,
    Profiles,
}

/// One tab, ready to draw.
#[derive(Debug, Clone, PartialEq)]
pub struct TabModel {
    pub addr: SessionAddr,
    pub kind: TabKind,
    /// Already derived: OSC title, else cwd basename, else "shell".
    pub title: String,
    /// The machine's display name ("studio"). Layout composes the chip's
    /// `host · cwd` line and the sidebar's grouping from this.
    pub host: String,
    /// Working directory, home-shortened for local sessions only.
    pub cwd: String,
    pub origin: TabOrigin,
    pub presence: TabPresence,
    /// Which slot of the host-accent cycle this tab's machine draws in.
    /// Slot 0 is always the local machine; remotes take the next slots in
    /// first-seen order, so a host keeps its colour while the window lives.
    pub accent: usize,
    /// A command is currently running in this session — the sidebar's
    /// pulsing dot.
    pub running: bool,
    /// How long since this session last produced output, pre-formatted
    /// ("2m", "12h"); empty when unknown.
    pub age: String,
    /// An attach or restore is in flight; the tab shows itself but cannot be
    /// typed into yet.
    pub connecting: bool,
    /// How this tab's host is currently reached. The one fact the deleted
    /// status bar owned alone was link degradation, and it surfaces here:
    /// the chip's glyph tile takes warn ink when stalled, danger when
    /// reconnecting.
    pub link: LinkKind,
}

impl TabModel {
    /// `host · cwd`, or just the host when the cwd is unknown — the vertical
    /// header's identity line. (The horizontal chip deliberately does not
    /// draw this: a second line on a 34px chip was unreadable at 9.5px.)
    #[must_use]
    pub fn detail(&self) -> String {
        if self.cwd.is_empty() {
            self.host.clone()
        } else {
            format!("{} · {}", self.host, self.cwd)
        }
    }
}

/// One host group of the sidebar (design screen 2).
#[derive(Debug, Clone, PartialEq)]
pub struct HostGroup {
    pub label: String,
    /// Host-accent slot, matching the tabs it groups.
    pub accent: usize,
    /// Mono sub-label — path and latency ("LAN 0.4 ms"); empty when unknown.
    pub sub: String,
    pub online: bool,
    /// Indices into `ChromeModel::tabs`, in strip order.
    pub tabs: Vec<usize>,
}

/// What the vertical sidebar shows beyond the tabs themselves.
#[derive(Debug, Clone, PartialEq)]
pub struct SidebarModel {
    pub groups: Vec<HostGroup>,
    /// Fleet-wide counts for the footer — every known host, tabbed or not.
    pub hosts_online: usize,
    pub hosts_asleep: usize,
}

/// One row of the ⌘K palette (design screen 6), ready to draw.
///
/// Display-only: the app keeps a parallel list of *actions* built in the
/// same pass, so row index `n` here and there mean the same thing by
/// construction — the drift the hit map exists to prevent, applied to rows.
#[derive(Debug, Clone, PartialEq)]
pub enum PickerRow {
    /// A group label: Blocks, Sessions, Hosts, Actions — in that order,
    /// blocks first, because the palette is primarily a history of what ran
    /// anywhere in the fleet.
    Group { title: String },
    /// A command from that history. `ok` colours the ↺ glyph.
    Block { command: String, provenance: String, ok: bool },
    /// A session somewhere in the fleet. `host` is the right-aligned
    /// provenance.
    Session {
        title: String,
        detail: String,
        host: String,
        attached: bool,
        attached_here: bool,
    },
    /// A machine; Enter opens a fresh shell on it. `detail` says how it is
    /// reached.
    Host { label: String, presence: TabPresence, detail: String },
    /// A command from the keymap, chord already platform-spelled.
    Action { name: String, chord: String },
    /// Nothing matched the filter; drawn so an empty panel never reads as
    /// broken.
    Nothing,
}

/// The ⌘K palette, when open.
#[derive(Debug, Clone, PartialEq)]
pub struct PickerModel {
    pub rows: Vec<PickerRow>,
    /// Index into `rows` the keyboard is on.
    pub selected: usize,
    /// The live filter string, drawn in the search line.
    pub filter: String,
    /// Scroll offset of the row list, physical pixels; layout clamps it.
    pub scroll: f32,
    /// Bring the selection into view this pass — keyboard only, so wheel
    /// scrolling never snaps back.
    pub ensure_visible: bool,
    /// How many hosts the query ran over — the query row's right-hand fact.
    pub hosts_searched: usize,
    /// The blink cycle's on half, from the animation clock.
    pub caret_on: bool,
}

/// One host card of the fleet view (design screen 7).
#[derive(Debug, Clone, PartialEq)]
pub struct FleetCard {
    pub name: String,
    /// The window's own machine — accent border, "this machine" note.
    pub local: bool,
    pub online: bool,
    /// A warn pill ("via tunnel"), when the path warrants one.
    pub pill: Option<String>,
    /// Label/value rows: path, key, sessions — only what is actually known.
    /// The value colour is by role: 0 plain, 1 success, 2 warn.
    pub rows: Vec<(String, String, u8)>,
}

/// One card of the theme gallery (design screen 8). Colours arrive as raw
/// theme values because the preview renders in *that* theme, not the UI's.
#[derive(Debug, Clone, PartialEq)]
pub struct ThemeCard {
    pub id: String,
    pub name: String,
    /// "dark", "light · default".
    pub qualifier: String,
    pub bg: [u8; 3],
    pub fg: [u8; 3],
    pub accent: [u8; 3],
    pub danger: [u8; 3],
    pub green: [u8; 3],
    /// The normal ANSI row, index order — the swatch strip.
    pub ansi: [[u8; 3]; 8],
    pub active: bool,
}

/// One pane's header facts (design screen 5).
#[derive(Debug, Clone, PartialEq)]
pub struct PaneModel {
    pub host: String,
    /// Mono sub-label: cwd, and the path cost for a remote pane.
    pub sub: String,
    pub focused: bool,
    /// Host-accent slot for the header dot.
    pub accent: usize,
}

/// A full-pane screen replacing the grid (fleet directory, theme gallery).
#[derive(Debug, Clone, PartialEq)]
pub enum ScreenModel {
    Fleet { cards: Vec<FleetCard> },
    Themes { cards: Vec<ThemeCard> },
}

/// One row of the command palette, ready to draw.
///
/// Display-only, like [`PickerRow`]: the app keeps a parallel list of the
/// actions each row runs, built in the same pass, so index `n` means the
/// same thing to the renderer and the input path by construction.
#[derive(Debug, Clone, PartialEq)]
pub enum PaletteRow {
    /// A category header ("Tabs", "Mouse").
    Group { title: String },
    /// A command. `chord` is already platform-spelled by
    /// `keymap::chord_label` (empty when the command has none — the chrome
    /// draws strings, it does not know what a modifier is). Reference rows
    /// — mouse gestures, footnotes — are `runnable: false`, drawn without a
    /// selection affordance and skipped by the keyboard.
    Command { name: String, chord: String, runnable: bool },
}

/// The command palette, when open.
#[derive(Debug, Clone, PartialEq)]
pub struct PaletteModel {
    /// Pre-filtered by the app; empty groups never arrive here.
    pub rows: Vec<PaletteRow>,
    /// Index into `rows` the keyboard is on.
    pub selected: usize,
    pub filter: String,
    /// Scroll offset, physical pixels; layout clamps it.
    pub scroll: f32,
    /// Bring the selected row into view this pass — keyboard navigation
    /// only, so wheel scrolling never snaps back to the selection.
    pub ensure_visible: bool,
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
    /// A typed edit in progress; drawn as the buffer with a caret, in warn
    /// colours after a failed parse.
    Editing { buffer: String, error: bool },
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
    /// A banner pinned above the list — restart owed, or a failed write.
    Notice { text: String },
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

/// How a tab's host is currently reached, carried per tab since the status
/// bar's deletion (design §1: "no status bar").
///
/// `Stalled` and `Reconnecting` describe the *link*, not the path — a LAN
/// session mid-hiccup is stalled, whatever route it normally takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkKind {
    Loopback,
    Lan,
    Tunnel,
    Stalled,
    Reconnecting,
}

/// The animation clock's current phases, computed by the app per rebuild.
/// The four design animations, minus hover (instant by spec): caret blink,
/// spinner rotation, running-dot pulse.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct AnimPhase {
    /// The blink cycle's on half — the palette caret and the grid cursor.
    pub caret_on: bool,
    /// Spinner rotation, 0..1 of a 0.9s turn.
    pub spin: f32,
    /// Running-dot opacity, 0.35..1 on a 1.6s ease.
    pub pulse: f32,
}

impl Default for AnimPhase {
    fn default() -> Self {
        Self { caret_on: true, spin: 0.0, pulse: 1.0 }
    }
}

/// Everything `layout` needs to draw the chrome once.
/// What the window's own controls take out of the chrome, whoever draws them.
///
/// The asymmetry between the two platforms is the point, not an accident.
/// macOS injects a *measurement*, because AppKit is the only authority on
/// where the traffic lights are — they move with OS version and localization.
/// Windows injects only a *flag*, because we draw the buttons ourselves and
/// their width is a layout constant the layout pass already knows. One
/// `Option` and one `bool`, rather than a third concept for each edge.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct WindowControls {
    /// Physical px the *OS* draws over our chrome at the leading edge, and the
    /// bar height it wants: `[width, height]`. The macOS traffic lights.
    /// `None` in fullscreen, where they auto-hide, and on every other platform.
    pub native_leading: Option<[f32; 2]>,
    /// We draw minimise / maximise / close ourselves at the trailing edge.
    pub drawn_caption: bool,
    /// The maximise affordance shows the restore glyph instead.
    pub maximized: bool,
    /// The window can be resized by dragging its edges.
    ///
    /// False when maximized, in fullscreen, and on the native-frame path where
    /// the OS owns them — a borderless window has no non-client area left for
    /// `DefWindowProc` to answer `HTLEFT` from, so the edges have to come out
    /// of our own hit map or they do not exist at all.
    pub resizable_edges: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChromeModel {
    pub tabs: Vec<TabModel>,
    /// Index into `tabs`.
    pub active: usize,
    pub position: TabsPosition,
    /// Scroll offset of the tab strip contents, physical pixels. Layout
    /// clamps it and reports the clamped value back.
    pub strip_scroll: f32,
    /// Bring the active chip into the strip's viewport this pass — set on
    /// activation paths only, so wheel scrolling never snaps back. The same
    /// discipline as the picker's `ensure_visible`.
    pub ensure_active_visible: bool,
    /// What the pointer is over, from last frame's hit map. Only used for
    /// hover fills, so one frame of lag is invisible.
    pub hover: Option<HitRegion>,
    /// What the window's own controls take out of the chrome.
    pub controls: WindowControls,
    pub focused: bool,
    /// Sidebar grouping and footer counts; `None` in the horizontal layout.
    pub sidebar: Option<SidebarModel>,
    /// A full-pane screen over the grid area (fleet, themes); Esc returns.
    pub screen: Option<ScreenModel>,
    /// The split tab's two pane headers, left then right; `None` unsplit.
    pub panes: Option<[PaneModel; 2]>,
    /// Animation phases for this rebuild.
    pub anim: AnimPhase,
    /// Where the grid area is, physical pixels — the rectangle a screen
    /// covers. Computed by the app from its insets.
    pub grid_area: [f32; 4],
    /// The palette pill's chord, platform-spelled ("⌘K") — composed by the
    /// app because the chrome does not know what a modifier is.
    pub palette_chord: String,
    /// The fleet picker, drawn over everything when open.
    pub picker: Option<PickerModel>,
    /// The command palette, likewise modal. The app enforces that at most
    /// one overlay is open, so layout never has to rank them.
    pub palette: Option<PaletteModel>,
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
    /// The grid font's size, physical pixels — the default for any text run
    /// the design does not size explicitly.
    pub font_px: f32,
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

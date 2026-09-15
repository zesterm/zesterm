//! The winit application: window, surface, and the frame loop.
//!
//! The crate's largest file, being split one concern at a time (#554). The
//! rule for what goes in a child and what stays here: a child module can see
//! this module's private items, while this module cannot see a child's — so the
//! *state* stays beside `App` and the *code* moves out. No field on `App` and no
//! item outside this module changes visibility; a moved item gains exactly the
//! `pub(super)` that sharing one file used to give it implicitly.

mod account;
mod dispatch;
mod gpu;
mod panes;
use panes::pane_is_covered;
mod profiles_tab;
mod prompts;
mod settings_tab;

pub(crate) use gpu::GpuHost;
use gpu::{alpha_mode_for, capture_frame, init_gpu};

use std::sync::Arc;

use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowId};

use crate::process::{FirstTab, Shared, TabOpen, WindowRequests, WindowSpec};
use crate::windows_state::{Geometry, SavedWindow};

use zest_font::{Fonts, Typography};
use zest_pty::{CommandSpec, PtySize};
use zest_render_wgpu::{Chrome, Renderer, Scene, Viewport};

use zest_input::{key, mouse, select, MouseState};
use crate::block_actions;
use crate::pipeline_cache;
use crate::chrome::hit::{self, CaptionButton, HitRegion, WheelTarget};
use crate::chrome::layout::ChromeLayout;
use crate::chrome::model::{
    ChromeMetrics, ChromeModel, PaneKind, TabModel, TabOrigin, TabPresence, WindowControls,
};
use crate::chrome::theme::ChromeColors;
use crate::chrome::Insets;
use crate::keymap;
use crate::platform;
use crate::route::{best_route, Dial, HostRoute};
use crate::session::{Session, Wakeup};
use crate::source::{Origin, SessionSource};
use crate::tabs::{Tab, TabStrip};
use crate::text_field::{command_for, TextCommand, TextField};


/// How long the window may take to appear, in milliseconds.
///
/// Measured on this machine at ~50ms. The budget is twice that: tight enough
/// that adding real work before the first paint fails it, loose enough that a
/// slower machine or a cold file cache does not. Checked by
/// `zesterm --startup-probe`.
const STARTUP_BUDGET_MS: u64 = 100;

/// How long to wait for a freshly spawned daemon to start listening.
///
/// Only ever paid on the very first launch on a machine; after that the daemon
/// is already running and finding it is a `connect` call.
pub(crate) const DAEMON_START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// How many sessions one fleet card lists before it says "+N more".
///
/// The grid is uniform-height, so one machine running thirty shells would make
/// every card in it thirty rows tall. Four is enough to recognise what is on a
/// machine; the count row above it states the real total, and the overflow row
/// points at ⌘K, which holds them all.
const FLEET_CARD_SESSIONS: usize = 4;

pub struct Config {
    pub font_families: Vec<String>,
    pub typography: Typography,
    /// OpenType features to shape the grid with, `liga`-style.
    ///
    /// Parsed once here rather than per frame: the grid path consults this for
    /// every run of every row.
    pub features: Vec<zest_font::Feature>,
    /// Let ligatures form on the grid.
    ///
    /// Together with `features`, this is what turns run shaping on at all —
    /// `glyph_for` is a charmap lookup with no GSUB, so neither setting can
    /// work without it, and neither costs anything when both are off.
    pub ligatures: bool,
    /// Generate U+2500–U+259F at cell size rather than taking the font's.
    ///
    /// Not part of [`Typography`] because it changes what a glyph *is*, not how
    /// large a cell is — an atlas bump, never a pty resize.
    pub builtin_box_drawing: bool,
    /// Stem darkening, applied to glyph coverage.
    pub text_gamma: f32,
    /// Extra contrast on glyph coverage.
    pub text_contrast: f32,
    pub text_antialias: zest_font::TextAntialias,
    pub text_hinting: zest_font::Hinting,
    pub theme: String,
    /// Theme to use when the OS reports a light appearance.
    ///
    /// Empty means "follow `theme` regardless", which is what someone who has
    /// deliberately chosen a dark theme expects — so it is not a fallback that
    /// needs filling in, it is an answer.
    pub light_theme: String,
    /// Whether the OS light/dark setting is consulted at all.
    pub follow_system_theme: bool,
    pub scrollback: usize,
    pub opacity: f32,
    /// Space between the window edge and the grid, in logical pixels.
    pub padding: u32,
    /// The strip's own alpha, independent of the grid's (ADR-003).
    pub chrome_opacity: f32,
    /// Who draws the window frame, resolved once from the tri-state setting.
    ///
    /// A type rather than a `bool` because the two questions asked of it --
    /// "does the system decorate?" and "do we draw a caption?" -- were
    /// separately decided and could disagree, which is exactly the window
    /// wearing two titlebars (#472).
    pub chrome: crate::window_chrome::WindowChrome,
    /// What the compositor puts behind the window (Mica and friends).
    pub backdrop: zest_config::settings::Backdrop,
    /// Picture drawn behind the cells; empty draws none. A relative path
    /// resolves against the config directory (`background::resolve_path`).
    pub background_image: String,
    pub background_fit: zest_config::settings::BackgroundFit,
    /// How far the picture fades toward the background. 1 hides it entirely.
    pub background_dim: f32,
    /// Initial window size in cells. Read once, at window creation.
    pub columns: u16,
    /// Initial window size in cells. Read once, at window creation.
    pub rows: u16,
    /// The tab strip's knobs, taken whole — the chrome reads all of them.
    pub tabs: zest_config::settings::Tabs,
    pub shell: Option<String>,
    /// Shape the cursor takes unless a program sets one with DECSCUSR.
    pub cursor_shape: zest_core::CursorShape,
    /// Whether the cursor springs toward its new cell instead of jumping.
    ///
    /// `smear` is not a third behaviour here: the tapered Neovide trail needs a
    /// quad whose corners lag by velocity, and this renderer's rect pipeline
    /// draws axis-aligned rectangles. It springs like `smooth` and says so once
    /// per process — the same degrade-and-warn the Windows backdrop materials
    /// get on macOS, rather than a control that silently does nothing. -> #329.
    pub cursor_trail: bool,
    /// Blink the cursor (and the palette caret), on the shared clock.
    pub cursor_blink: bool,
    /// Half-cycle of the blink, milliseconds.
    pub cursor_blink_interval_ms: u32,
    /// Jump back to the bottom whenever the program writes something.
    ///
    /// Off by default, and that is the interesting half: with it on, scrollback
    /// is unreadable while anything is running, because every line emitted yanks
    /// the view away. It exists because a minority genuinely prefer never
    /// missing live output.
    pub scroll_on_output: bool,
    /// Jump back to the bottom when a key is pressed.
    ///
    /// Typing only. A block re-run and a paste jump regardless: they are things
    /// the user deliberately did to the *session*, and someone who turned this
    /// off asked not to be yanked away while typing, not to watch a command
    /// they started scroll past somewhere off screen.
    pub scroll_on_keypress: bool,
    /// Rows the view moves per wheel notch.
    pub lines_per_notch: usize,
    /// Animate at all.
    pub motion_enabled: bool,
    /// Honour the OS "reduce motion" accessibility setting.
    pub respect_reduce_motion: bool,
    /// Ease scrolling as a fractional row offset.
    pub smooth_scroll: bool,
    /// Spring response in seconds — roughly, time to reach the target.
    pub spring_response: f32,
    /// Spring damping ratio. 1.0 is critically damped; below that overshoots.
    pub spring_damping: f32,
    /// Where a bare local shell starts. `None` inherits this process's.
    ///
    /// A *fallback*, not an override: a profile's `starting_directory` is
    /// resolved by the machine that spawns and overwrites this afterwards.
    pub shell_cwd: Option<std::path::PathBuf>,
    /// Environment entries layered over the shell's, in `shell.env` order.
    ///
    /// An empty value *unsets*, which both pty backends already implement —
    /// the same convention `zest_pty::terminal_env` uses to strip another
    /// terminal's stale identity out of an inherited environment.
    pub shell_env: Vec<(String, String)>,
}

impl Config {
    /// Whether this window's surface has to carry per-pixel alpha.
    ///
    /// One function because *four* places decide it — `with_transparent` and
    /// the swapchain's `want_transparency`, both in [`App::open_window`],
    /// [`App::apply_transparency`] on a reload, and [`App::antialias_for`] —
    /// and a copy of `opacity < 1.0` per site is how the second opacity gets
    /// added to some of them and forgotten in the rest. Written as three, the
    /// count was itself wrong by one, and that one was the swapchain: a first
    /// launch with only `chrome_opacity` below 1 came up opaque.
    ///
    /// *Either* opacity below 1 needs it: chrome and grid each own their
    /// pixels' alpha now, so a glass titlebar over a solid grid is a
    /// translucent surface even though `window.opacity` is 1.
    #[must_use]
    pub fn translucent_surface(&self) -> bool {
        self.opacity < 1.0 || self.chrome_opacity < 1.0
    }
}

impl From<&zest_config::Settings> for Config {
    /// Project the settings tree onto what the app actually runs on.
    ///
    /// Kept as a projection rather than using `Settings` directly, so the
    /// renderer and the window never reach into a user-editable tree: anything
    /// they need has been validated and clamped exactly once, here.
    fn from(s: &zest_config::Settings) -> Self {
        Self {
            font_families: s.typography.families.clone(),
            typography: Typography {
                // Clamped rather than trusted. These come from a file a user
                // edits by hand, and a zero or negative size reaches
                // `f32::clamp` in the metrics code as a panic.
                size_pt: s.typography.size_pt.clamp(4.0, 144.0),
                line_height: s.typography.line_height.clamp(0.5, 3.0),
                cell_width: s.typography.cell_width.clamp(0.0, 2.0),
                letter_spacing: s.typography.letter_spacing.clamp(-5.0, 20.0),
                ..Default::default()
            },
            features: s
                .typography
                .features
                .iter()
                .filter_map(|f| {
                    let parsed = zest_font::Feature::parse(f);
                    if parsed.is_none() {
                        // Dropped, not fatal: a typo in one tag must not cost
                        // the user their whole config, and the schema calls
                        // this a list of tags rather than a grammar.
                        tracing::warn!(tag = %f, "not an OpenType feature tag; ignoring");
                    }
                    parsed
                })
                .collect(),
            ligatures: s.typography.ligatures,
            builtin_box_drawing: s.typography.builtin_box_drawing,
            // Carried, not clamped: `resolve_text_tuning` owns the range, so
            // there is one place that decides what an out-of-range gamma means.
            text_gamma: s.appearance.text_gamma,
            text_contrast: s.appearance.text_contrast,
            text_antialias: match s.appearance.text_antialias {
                zest_config::TextAntialias::Subpixel => zest_font::TextAntialias::Subpixel,
                zest_config::TextAntialias::Grayscale => zest_font::TextAntialias::Grayscale,
            },
            text_hinting: match s.appearance.text_hinting {
                zest_config::TextHinting::None => zest_font::Hinting::None,
                zest_config::TextHinting::Full => zest_font::Hinting::Full,
            },
            theme: s.appearance.theme.clone(),
            light_theme: s.appearance.light_theme.clone(),
            follow_system_theme: s.appearance.follow_system_theme,
            scrollback: s.scrolling.scrollback,
            opacity: s.window.opacity.clamp(0.0, 1.0),
            padding: s.window.padding.min(64),
            chrome_opacity: s.window.chrome_opacity.clamp(0.0, 1.0),
            // The whole matrix, and the reasons for it, live on
            // `WindowChrome::resolve` -- including why `Auto` defers to the
            // compositor on unix rather than guessing.
            chrome: crate::window_chrome::WindowChrome::resolve(
                s.window.custom_chrome,
                crate::window_chrome::Host::current(),
            ),
            backdrop: s.window.backdrop,
            background_image: s.window.background_image.clone(),
            background_fit: s.window.background_fit,
            // `finite_or` rather than a bare clamp: `clamp` preserves NaN, and
            // a hand-edited `nan` here reaches the vertex stage as a quad at
            // infinity rather than as a wrong pixel.
            background_dim: finite_or(s.window.background_dim, 0.5).clamp(0.0, 1.0),
            columns: s.window.columns,
            rows: s.window.rows,
            tabs: s.tabs.clone(),
            shell: (!s.shell.command.is_empty()).then(|| s.shell.command.clone()),
            cursor_trail: match s.cursor.trail {
                zest_config::settings::CursorTrail::None => false,
                zest_config::settings::CursorTrail::Smooth => true,
                zest_config::settings::CursorTrail::Smear => {
                    // Once per process: `Config::from` runs on every reload,
                    // and the settings tab writes on every keystroke of a
                    // slider drag. A "not implemented yet" notice repeated
                    // twenty times is how a log stops being read.
                    static SAID: std::sync::Once = std::sync::Once::new();
                    SAID.call_once(|| {
                        tracing::warn!(
                            "cursor.trail = \"smear\" is not implemented yet (#329); \
                             using \"smooth\""
                        );
                    });
                    true
                }
            },
            cursor_shape: match s.cursor.shape {
                zest_config::settings::CursorShape::Block => zest_core::CursorShape::Block,
                zest_config::settings::CursorShape::Underline => zest_core::CursorShape::Underline,
                zest_config::settings::CursorShape::Bar => zest_core::CursorShape::Bar,
            },
            cursor_blink: s.cursor.blink,
            cursor_blink_interval_ms: s.cursor.blink_interval_ms.clamp(100, 5000) as u32,
            scroll_on_output: s.scrolling.scroll_on_output,
            scroll_on_keypress: s.scrolling.scroll_on_keypress,
            // Clamped to the schema's own range: a hand-edited `0` would make
            // the wheel do nothing at all, which reads as a broken mouse.
            lines_per_notch: s.scrolling.lines_per_notch.clamp(1, 50),
            motion_enabled: s.motion.enabled,
            respect_reduce_motion: s.motion.respect_system_reduce_motion,
            smooth_scroll: s.motion.smooth_scroll,
            // Sanitized, then clamped to the schema's own ranges: these reach
            // an integrator, a zero or negative response is a division by zero
            // wearing a preference's clothes -- and `f32::clamp` *preserves*
            // NaN, while TOML accepts `nan` as a float literal. A config typo
            // must not be able to make a spring that never settles.
            spring_response: finite_or(
                s.motion.spring_response,
                zest_config::settings::Motion::default().spring_response,
            )
            .clamp(0.01, 2.0),
            spring_damping: finite_or(
                s.motion.spring_damping,
                zest_config::settings::Motion::default().spring_damping,
            )
            .clamp(0.1, 2.0),
            // Empty means "inherit", which is not the same as a cwd of `""` —
            // that would be an invalid directory and fail every spawn.
            shell_cwd: (!s.shell.cwd.trim().is_empty())
                .then(|| std::path::PathBuf::from(s.shell.cwd.trim())),
            shell_env: s.shell.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        }
    }
}

impl Default for Config {
    /// Delegated to the settings defaults, so there is exactly one place a
    /// default lives. Two lists of defaults drift, and the one that drifts is
    /// always the one nobody is looking at.
    fn default() -> Self {
        Self::from(&zest_config::Settings::default())
    }
}

/// The picker's transient state while it is open, and the action list
/// parallel to the drawn rows — built in the same pass as the row models, so
/// index `n` means the same thing in both by construction.
struct PickerState {
    selected: usize,
    filter: TextField,
    scroll: f32,
    /// Bring the selection into view on the next layout — keyboard only.
    scroll_to_selected: bool,
    actions: Vec<PickerAction>,
    /// Something waiting for this picker to choose a machine.
    ///
    /// On the picker's state, not the app's, so it dies with the picker — a
    /// stale pending launch surviving a dismissal would hijack the next ⌘K's
    /// host row.
    pending: Option<Pending>,
}

/// What the next host or session row will carry (design §12's `ask_host`, and
/// §6's `⇧⏎ run on host…`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pending {
    /// A host-agnostic profile: picking a machine launches it there instead of
    /// a bare shell.
    Profile(String),
    /// A command from a block: picking a machine runs it there.
    ///
    /// The command cannot be written until a session exists, and opening one
    /// is a worker dial — so this is armed on the tab and written when it
    /// settles, never at click time.
    Command(String),
    /// ⌘H: the next machine or session picked becomes a pane of the active
    /// tab rather than a tab of its own (#436).
    Split,
}

/// The command palette's transient state while it is open, and the action
/// list parallel to the drawn rows — built in the same `keymap::palette`
/// pass, so index `n` means the same thing in both by construction. `None`
/// entries are headers and reference rows the selection skips.
struct PaletteState {
    selected: usize,
    filter: TextField,
    scroll: f32,
    /// Bring the selection into view on the next layout — keyboard
    /// navigation only, never the wheel.
    scroll_to_selected: bool,
    actions: Vec<Option<keymap::Action>>,
}

/// The cwd chip's directory browser (#439): the palette's chassis around a
/// host-answered listing. `rows` is the answer list parallel to the drawn
/// rows — built in the same pass as the model, so index `n` means one thing
/// to the renderer and the input path by construction (`None` is the `..`
/// row, which *navigates*; `Some(path)` switches).
struct DirPickerState {
    /// The directory whose children are listed — the browse position.
    path: String,
    /// Its parent, from the answer; `None` at a root, which drops the `..`
    /// row rather than drawing one that goes nowhere.
    parent: Option<String>,
    /// The children, as the host sent them.
    dirs: Vec<String>,
    /// No answer for `path` yet.
    loading: bool,
    truncated: bool,
    error: String,
    filter: TextField,
    selected: usize,
    scroll: f32,
    scroll_to_selected: bool,
    rows: Vec<Option<String>>,
}

/// The Settings tab's state — created when the tab opens, dropped when it
/// closes, surviving activation changes in between: the tab is a place you
/// sit in (design §11), and its selection, filter and buffers belong to it.
struct SettingsUiState {
    selected: usize,
    /// The rail's selected category, by label — a label, not an index,
    /// because the filter hides empty categories and an index would slide.
    category: String,
    filter: TextField,
    scroll: f32,
    /// Bring the selection into view on the next layout — set by keyboard
    /// navigation, never by the wheel, so free scrolling does not snap back.
    scroll_to_selected: bool,
    /// Parallel to the drawn rows, same-pass built (the picker discipline).
    actions: Vec<crate::settings_ui::RowAction>,
    /// The schema walk, cached at open: the schema cannot change while the
    /// tab is open, and re-walking it per hover would be pure waste.
    fields: Vec<zest_config::ui::UiField>,
    /// A typed edit in progress; while `Some`, characters belong to it.
    editing: Option<crate::settings_ui::EditBuffer>,
    /// Installed families, scanned once at open — the font rows' fallback
    /// tags read this instead of re-scanning the system per rebuild.
    installed: Vec<String>,
    /// The open dropdown menu, when there is one.
    menu: Option<MenuState>,
    /// A font-list drag in progress: (row index, item being dragged).
    /// Order is the setting; crossing another item reorders through the
    /// same write path as everything else.
    list_drag: Option<(usize, usize)>,
}

impl SettingsUiState {
    /// Composed (IME) text, routed exactly where a typed character goes:
    /// the open dropdown swallows it (the key path's arm ignores
    /// `Key::Character` there too), an open edit buffer takes it, and
    /// otherwise it lands in the filter. The Settings tab holds the
    /// keyboard — a commit written to the concealed session would type a
    /// composed word into a shell the user cannot see.
    fn commit_text(&mut self, text: &str) {
        self.text_key(TextCommand::Insert(text.to_string()), None);
    }

    /// One text command, routed exactly where [`Self::commit_text`] routes a
    /// composed word — and the seam the key path drives, so "⌘V into the
    /// settings filter" is a test rather than a thing to try in a window.
    fn text_key(&mut self, cmd: TextCommand, clipboard: Option<&str>) -> Option<String> {
        if self.menu.is_some() {
            return None;
        }
        if let Some(edit) = self.editing.as_mut() {
            let out = edit.buffer.apply(cmd, clipboard);
            if out.changed {
                // New input clears a stale parse error, as typing does.
                edit.error = false;
            }
            out.copied
        } else {
            let out = self.filter.apply(cmd, clipboard);
            if out.changed {
                self.selected = 0;
                self.scroll_to_selected = true;
            }
            out.copied
        }
    }
}

/// The Profiles tab's state — created when the tab opens, dropped when it
/// closes (design §12; the same lifetime rule as `SettingsUiState`).
struct ProfilesUiState {
    /// The profile being edited, by table name (`defaults` for Defaults) —
    /// a name, not an index, because a reload can grow or shrink the rail.
    profile: String,
    /// Index into the drawn rows the keyboard is on.
    selected: usize,
    filter: TextField,
    scroll: f32,
    /// Bring the selection into view on the next layout — keyboard only.
    scroll_to_selected: bool,
    /// Parallel to the drawn rows, same-pass built (the picker discipline).
    actions: Vec<crate::settings_ui::RowAction>,
    /// `profiles::fields()`, cached at open like the settings walk.
    fields: Vec<zest_config::ui::UiField>,
    /// A typed edit in progress; while `Some`, characters belong to it.
    editing: Option<crate::settings_ui::EditBuffer>,
    /// A rename of the header name in progress (#283). Separate from
    /// `editing`, which is keyed by field index and structurally cannot name
    /// the profile itself. The two are never both open: opening either closes
    /// the other, so "where do characters go" has one answer.
    renaming: Option<TextField>,
    /// Why the typed name cannot be used, while it cannot.
    rename_error: Option<String>,
    /// The open dropdown menu, when there is one — backdrop's, and §12's
    /// theme and font rosters.
    menu: Option<MenuState>,
    /// The last profile write that failed, shown as a banner.
    error: Option<String>,
}

impl ProfilesUiState {
    /// Take an open edit so the caller can write it — see
    /// [`crate::settings_ui::take_pending_edit`], which both editors share
    /// (#272, #275).
    fn take_pending_edit(&mut self) -> crate::settings_ui::Pending {
        crate::settings_ui::take_pending_edit(&mut self.editing, &self.fields)
    }

    /// Composed (IME) text, routed exactly like the Settings tab's: menu
    /// swallows, an open buffer takes it, otherwise the filter.
    fn commit_text(&mut self, text: &str) {
        self.text_key(TextCommand::Insert(text.to_string()), None);
    }

    /// The Settings tab's [`SettingsUiState::text_key`], for the same reason.
    fn text_key(&mut self, cmd: TextCommand, clipboard: Option<&str>) -> Option<String> {
        if self.menu.is_some() {
            return None;
        }
        // The name entry outranks both, so a rename cannot leak characters
        // into the filter behind it (#283).
        if let Some(name) = self.renaming.as_mut() {
            let out = name.apply(cmd, clipboard);
            if out.changed {
                // Typing clears the complaint, exactly as it does for a field
                // that failed to parse — the name is being fixed.
                self.rename_error = None;
            }
            out.copied
        } else if let Some(edit) = self.editing.as_mut() {
            let out = edit.buffer.apply(cmd, clipboard);
            if out.changed {
                edit.error = false;
            }
            out.copied
        } else {
            let out = self.filter.apply(cmd, clipboard);
            if out.changed {
                self.selected = 0;
                self.scroll_to_selected = true;
            }
            out.copied
        }
    }
}

/// The + launcher menu's transient state while it is open (design §1), and
/// the action list parallel to the drawn rows — built in the same
/// `launcher::build_rows` pass, so index `n` means the same thing in both by
/// construction.
struct LauncherState {
    selected: usize,
    /// Which `+` opened it — decides where the panel hangs (§1 vs §2).
    anchor: crate::chrome::model::LauncherAnchor,
    actions: Vec<crate::launcher::LauncherAction>,
}

/// What a profile launch at another machine carries: the tab's identity and
/// the four strings the far daemon is told.
///
/// A struct for `zest_daemon::client::Launch`'s reason: three of these are
/// `String`s and a fourth made it eight arguments, which is where a swapped
/// `command`/`cwd` stops being something the compiler can catch.
struct ProfileLaunch {
    /// The profile's name, which is also the launch's `profile` on the wire.
    name: String,
    identity: crate::tabs::ProfileIdentity,
    command: String,
    cwd: String,
    /// The profile's own entries, unexpanded, when *this* machine holds the
    /// profile; empty for a published one, whose host applies its own (#559).
    env: Vec<(String, String)>,
}

/// A block's open ⋯ menu (design §3), and the action list parallel to its
/// drawn rows — built in one `block_menu::build_rows` pass, so index `n` means
/// the same thing in both by construction, exactly as the launcher's does.
struct BlockMenuState {
    /// The block it acts on. Opening the menu also *selects* that block, so
    /// the accent rail and the menu can never name different ones.
    block: u32,
    /// What the panel hangs off, physical px: the `⋯` rect the block pass drew
    /// for a left click, or a zero-size rect at the pointer for a right click.
    anchor: [f32; 4],
    selected: usize,
    actions: Vec<crate::block_menu::BlockMenuAction>,
}

/// Which full-pane screen the window shows in place of the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppScreen {
    Terminal,
    Fleet,
    Themes,
}


/// The rail, the wash and the header span one pane's blocks come out with.
///
/// Driven through real OSC 133 transcripts rather than hand-built `BlockBand`s:
/// what these pin is the *ladder*, and a ladder is only wrong in the arm nobody
/// wrote down. The states are the only thing the rail says, so a state that
/// stops reaching it goes silent — every block simply looks like every other.
#[cfg(test)]
mod block_band_tests {
    use super::*;
    use zest_core::Terminal;

    fn colors() -> ChromeColors {
        let theme = zest_theme::builtin::obsidian();
        ChromeColors::new(&theme.ui, &theme.effects, 1.0, 1.0)
    }

    /// One block per state: printed output and succeeded, failed, printed
    /// nothing at all, and one still running.
    fn session() -> Terminal {
        let mut t = Terminal::new(40, 12, 200);
        t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07echo hi\x1b]133;C\x07\r\n");
        t.advance(b"hi\r\n\x1b]133;D;0\x07");
        t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07nope\x1b]133;C\x07\r\n");
        t.advance(b"not found\r\n\x1b]133;D;127\x07");
        t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07cd ..\x1b]133;C\x07\r\n");
        t.advance(b"\x1b]133;D;0\x07");
        t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07make\x1b]133;C\x07\r\n");
        t.advance(b"building\r\n");
        t
    }

    #[test]
    fn every_block_state_reaches_the_rail() {
        let c = colors();
        let t = session();
        let bands = App::block_bands(&c, &t, None, false);
        assert_eq!(bands.len(), 4, "one band per block that has started output");
        assert_eq!(bands[0].rail, c.success, "exit 0");
        assert_eq!(bands[1].rail, c.danger, "non-zero exit");
        assert_eq!(bands[2].rail, c.text_faint, "printed nothing");
        assert_eq!(bands[3].rail, c.warn, "still running");

        // A session whose host went away: the last block is not running
        // anywhere, and the rail is the only thing that can say so.
        let dead = App::block_bands(&c, &t, None, true);
        assert_eq!(dead[3].rail, c.text_faint, "interrupted");
    }

    #[test]
    fn a_full_screen_program_has_no_rails_and_leaving_it_brings_them_back() {
        // The alt screen is a separate grid whose ids restart at zero, so a
        // primary block would land on whatever alt rows collide with it --
        // the shell's history drawn down a program's UI. And the program's
        // own `ESC[2J` on that screen erases no primary row, so the exit
        // restores every rail with the screen it restores. (#539)
        let c = colors();
        let mut t = session();
        t.advance(b"\x1b[?1049h\x1b[2J\x1b[Hvim\r\n");
        assert!(t.in_alt_screen());
        assert!(
            App::block_bands(&c, &t, None, false).is_empty(),
            "no block is drawn over a full-screen program"
        );
        t.advance(b"\x1b[?1049l");
        assert_eq!(
            App::block_bands(&c, &t, None, false).len(),
            4,
            "leaving the program brings every rail back: its clear touched nothing on the primary screen"
        );
    }

    #[test]
    fn the_wash_belongs_to_the_output_and_a_selection_overrides_its_tint() {
        let c = colors();
        let t = session();
        let bands = App::block_bands(&c, &t, None, false);
        for (i, b) in bands.iter().enumerate() {
            assert!(b.from <= b.header_to, "block {i}: a header cannot start before its block");
            assert!(b.header_to <= b.to, "block {i}: nor outlast it");
        }
        assert!(bands[0].wash.is_some(), "a block with output is washed");
        assert!(
            bands[2].wash.is_none(),
            "`cd ..` printed nothing, so there is no output to wash — the rail alone says it ran"
        );
        assert_eq!(bands[2].header_to, bands[2].to, "and every row it has is header");

        let id = t.blocks().blocks()[0].id.0;
        let lit = App::block_bands(&c, &t, Some(id), false);
        assert_eq!(lit[0].rail, c.accent, "the selected block takes the accent");
        assert_eq!(
            lit[0].wash,
            Some(crate::chrome::layout::washed(c.accent, 0.10)),
            "and its wash the accent at 10%, which is the whole of the selection now"
        );
        assert_eq!(lit[1].rail, bands[1].rail, "its neighbours are untouched");
    }

    #[test]
    fn a_block_wash_is_visible_in_every_builtin_theme() {
        // The bug this exists for: a *fixed* linear-light alpha that reads
        // correctly on `obsidian`'s near-black ground moves `paper`'s
        // near-white one by a single 8-bit step, and a light theme ships with
        // blocks whose only edge is the rail — which looks deliberate, so
        // nothing reports it. Same trap `oklch::contrast_shift` documents for
        // opaque surfaces, met by something that has to stay translucent.
        //
        // Asserted as a composite in sRGB, because "visible" is a fact about
        // what reaches the screen, not about the alpha that produced it.
        for theme in zest_theme::builtin::all() {
            let c = ChromeColors::new(&theme.ui, &theme.effects, 1.0, 1.0);
            for (state, name) in
                [(c.success, "success"), (c.danger, "danger"), (c.warn, "warn"), (c.text_faint, "faint")]
            {
                let wash = state_wash(state, &c);
                assert!(
                    wash.0[3] < 0.5,
                    "{}/{name}: a wash that opaque would flatten a TUI's own colours",
                    theme.name
                );
                let over = |i: usize| wash.0[i] + c.bg_opaque.0[i] * (1.0 - wash.0[3]);
                let step = (0..3)
                    .map(|i| (linear_to_srgb(over(i)) - linear_to_srgb(c.bg_opaque.0[i])).abs())
                    .fold(0.0f32, f32::max);
                assert!(
                    step * 255.0 >= 2.0,
                    "{}/{name}: the wash lands {:.2} of an sRGB step from the background,                      which is a block with no edges",
                    theme.name,
                    step * 255.0
                );
            }
        }
    }

    /// The sRGB opto-electronic transfer function, for asserting in the space
    /// the eye is in rather than the one the blend happens in.
    fn linear_to_srgb(v: f32) -> f32 {
        let v = v.clamp(0.0, 1.0);
        if v <= 0.003_130_8 {
            v * 12.92
        } else {
            1.055 * v.powf(1.0 / 2.4) - 0.055
        }
    }

    #[test]
    fn a_header_replaces_only_the_prompt_lines_above_the_output() {
        assert_eq!(header_span(4, Some(5), 9), 5, "a one-line prompt, replaced");
        assert_eq!(header_span(4, Some(6), 9), 6, "a two-line prompt, both replaced");
        assert_eq!(header_span(4, None, 9), 4, "a block that never started output has no header");
    }

    #[test]
    fn a_block_whose_output_starts_on_its_prompt_line_replaces_nothing() {
        // `build_block_views` widens its header to at least one row so a header
        // is never zero rows tall. Done here that would blank the first line
        // the command printed — better a double-printed prompt for one odd
        // block than output that is simply not there.
        assert_eq!(header_span(4, Some(4), 9), 4);
        assert_eq!(header_span(4, Some(3), 9), 4, "and an output line *above* the prompt likewise");
    }

    #[test]
    fn a_wildly_wide_header_replaces_nothing_at_all() {
        // A corrupt index can name a wide line range; `build_block_views`
        // clamps its header to the first contiguous run of rows for exactly
        // that reason. Unclamped here, one bad block would blank every row
        // between its two ends with nothing drawn over them.
        assert_eq!(header_span(0, Some(400), 500), 0, "past any prompt anyone ships");
        assert_eq!(header_span(0, Some(4), 500), 4, "but four lines is still a prompt");
    }

    #[test]
    fn a_header_never_outruns_its_own_band() {
        // `to` bounds it: a running block's end is resolved against the cursor,
        // and a prompt whose output line has not been reached yet would
        // otherwise name lines the band does not contain.
        assert_eq!(header_span(4, Some(7), 6), 6, "clamped to the band's end");
    }
}

/// The lines a block's header replaces, inside a band `[prompt_line, to)`.
///
/// The chrome draws a compact restatement of the command over the shell's own
/// prompt rows, and since #465 it does that on bare background rather than on
/// an opaque fill — so the grid must not draw those rows at all. This is how
/// wide "those rows" is, and there are two ways `output_line` can lie about it.
///
/// **Output that starts on the prompt line has no prompt row to spare.**
/// `build_block_views` widens its header to `out_line.max(prompt_line + 1)` so
/// that a header is never zero rows tall; done here that would suppress the
/// first line the command printed. Better a double-printed prompt for one odd
/// block than output that is simply not there.
///
/// **A corrupt index can name a wide line range.** `build_block_views` clamps
/// the header to the first contiguous *run of rows* for exactly that reason
/// (see there) — and a block reaching this far unclamped would blank every row
/// between its two ends, with nothing drawn over them. That clamp cannot be
/// reused here: it is in viewport rows and this is in lines. The cap is the
/// cheap half of it.
///
/// Free and pure so it can be tested without an `App`, like `next_wake` and
/// `host_slot` above it.
/// The selected block's wash. Louder than a state wash on purpose: it answers
/// "what does ⌘⇧O copy", and with the header's fill gone it is the only thing
/// left saying so.
const SELECTED_WASH: f32 = 0.10;

/// The state colour at the alpha that moves this theme's background one small
/// perceptual step — the wash under a finished block's output.
///
/// **Solved, not tabulated, and deliberately not the design's flat "4%".** The
/// mock is CSS, which composites in sRGB; this pipeline blends in linear light,
/// where the same alpha of a bright ink over a near-black background lifts
/// several times as far — and over a *paper-white* one lifts by nothing at all.
/// A constant would therefore have to be wrong on one end or the other, and the
/// end it was wrong on would be the light themes, silently: a rail with no wash
/// still looks deliberate.
///
/// `ChromeColors::wash_target` is where `oklch::contrast_shift` would have put
/// an opaque panel. This asks what alpha of `ink` reaches that luminance, which
/// also evens the states out — `danger` is a darker ink than `warn`, and a flat
/// alpha would make a failed block's wash the fainter of the two.
///
/// Clamped at both ends: an ink too close to the background in luminance (a
/// faint rail on a mid-grey theme) would otherwise ask for an opaque wash, and
/// this is painted over every cell background under it, so an opaque one would
/// flatten a TUI's colours to one wash.
fn state_wash(
    ink: zest_render_wgpu::LinearRgba,
    c: &ChromeColors,
) -> zest_render_wgpu::LinearRgba {
    /// Enough to see, on a theme whose ink barely differs from its ground.
    const MIN: f32 = 0.004;
    /// Past this the wash stops being a wash. `paper` asks for about half of it.
    const MAX: f32 = 0.22;

    let ink_lum = crate::chrome::theme::luminance(ink);
    let reach = ink_lum - c.wash_from;
    let alpha = if reach.abs() < f32::EPSILON {
        MAX
    } else {
        ((c.wash_target - c.wash_from) / reach).clamp(MIN, MAX)
    };
    crate::chrome::layout::washed(ink, alpha)
}

fn header_span(prompt_line: u64, output_line: Option<u64>, to: u64) -> u64 {
    /// No shipped prompt is four lines tall. Raise it when one is.
    const MAX_HEADER_LINES: u64 = 4;

    match output_line {
        Some(o) if o > prompt_line && o - prompt_line <= MAX_HEADER_LINES => o.min(to),
        _ => prompt_line,
    }
}

/// How long an enrolment code is — the server's `ENROLL_CODE_LENGTH`
/// (`cloud/packages/web/src/enroll/codes.ts`), pinned here because the two
/// ends are separate projects and nothing compiles both. The entry clamps at
/// this, so a held key or a stray paste cannot outgrow the box the fleet
/// header sizes for it.
const ENROLL_CODE_LENGTH: usize = 8;

/// How often the browser hand-off polls its claim (#226). Three seconds:
/// fast enough that "I clicked Approve" and "the app noticed" feel like one
/// event, slow enough that a ten-minute grant costs two hundred cheap
/// pending answers, not a hammer.
const LINK_POLL: std::time::Duration = std::time::Duration::from_secs(3);

/// The characters a code can contain — the server's `ENROLL_CODE_ALPHABET`
/// (`cloud/packages/web/src/enroll/codes.ts`), pinned like the length above.
/// No `0/O`, `1/I/L` or `U`: the confusables were excluded so a code read off
/// one screen cannot be mis-typed into another.
const ENROLL_CODE_ALPHABET: &str = "23456789ABCDEFGHJKMNPQRSTVWXYZ";

/// The theme dropdown's last row. The gallery (design screen 8) shows each
/// theme's swatches, which a 288px menu cannot; the dropdown is the quick
/// in-place choice and this keeps the browsing one click away.
const BROWSE_THEMES: &str = "Browse all themes\u{2026}";

/// Feed text into the code entry — one filter for typing and paste (#228).
///
/// Uppercase first — a person reading a code off a screen may well type it
/// lowercase, and making them notice would be a refusal about nothing — then
/// keep only the alphabet's own characters. Everything else is dropped rather
/// than sent to be refused: that is what lets a paste carrying whitespace or
/// a stray word around the code still land the code itself, and what keeps a
/// `0` or an `I` (which no real code contains) from occupying a slot the
/// real character then cannot fill.
/// An open dropdown menu, on the Settings tab or the Profiles editor.
///
/// One menu for both kinds of choice. The schema's variants are read live off
/// the field; a **roster** — themes, installed font families — is captured
/// here when the menu opens, because it is neither in the schema nor cheap:
/// scanning installed families is a real system call, and per-frame is what
/// `open_value_picker`'s comment existed to avoid before this replaced it.
struct MenuState {
    /// Row index the menu hangs off.
    row: usize,
    /// Index into the *filtered* options the keyboard is on.
    selected: usize,
    /// Empty means "the field's schema variants".
    roster: Vec<String>,
    filter: TextField,
    scroll: f32,
    /// Bring the selection into view on the next layout — keyboard only.
    scroll_to_selected: bool,
    /// The font list's dashed ＋ row: choosing grows the stack instead of
    /// replacing the value.
    append: bool,
}

impl MenuState {
    /// A menu over the field's own schema variants — the documented selects.
    fn variants(row: usize) -> Self {
        Self {
            row,
            selected: 0,
            roster: Vec::new(),
            filter: TextField::default(),
            scroll: 0.0,
            scroll_to_selected: true,
            append: false,
        }
    }

    /// A menu over a roster the client brought, starting on `current` so
    /// opening it and pressing Enter is a no-op rather than a surprise.
    fn roster(row: usize, roster: Vec<String>, current: Option<&str>) -> Self {
        let selected =
            current.and_then(|c| roster.iter().position(|o| o == c)).unwrap_or(0);
        Self {
            row,
            selected,
            roster,
            filter: TextField::default(),
            scroll: 0.0,
            scroll_to_selected: true,
            append: false,
        }
    }

    /// The roster entries a filter admits, matched case-insensitively on a
    /// substring.
    ///
    /// Prefix matching would be useless here: the family is "MesloLGM NF" and
    /// the words someone reaches for are "meslo", "nerd" or "mono", only one
    /// of which starts it.
    fn matching(&self) -> Vec<String> {
        let needle = self.filter.text().to_lowercase();
        if needle.is_empty() {
            return self.roster.clone();
        }
        self.roster.iter().filter(|o| o.to_lowercase().contains(&needle)).cloned().collect()
    }

    /// A roster big enough that scanning it beats scrolling it. Four
    /// documented variants under a search box is noise; 266 families without
    /// one is the reason this menu exists.
    fn searchable(&self) -> bool {
        self.roster.len() > 8
    }
}

/// The open dropdown, resolved against the row it hangs off — same-pass, so
/// a menu can never outlive its row.
///
/// Shared by the Settings tab and the Profiles editor, which is the point:
/// the two builders were copies, and the copy is where the roster support
/// would have gone into only one of them. `current` is the field's live
/// value; the caller resolves it, because only it knows whether the field is
/// a string or the font list's array.
fn menu_model(
    menu: &MenuState,
    actions: &[crate::settings_ui::RowAction],
    fields: &[zest_config::ui::UiField],
    current: Option<&str>,
    footer: Option<String>,
) -> Option<crate::chrome::model::SettingsMenuModel> {
    use crate::chrome::model::{SettingsMenuModel, SettingsMenuOption};
    let field_idx = match actions.get(menu.row) {
        Some(crate::settings_ui::RowAction::Field(i)) => *i,
        _ => return None,
    };
    let field = fields.get(field_idx)?;
    // A roster the client brought, or the schema's own variants. Both empty
    // is a field with nothing to choose from, and no menu.
    let options: Vec<SettingsMenuOption> = if menu.roster.is_empty() {
        field
            .variants
            .iter()
            .map(|v| SettingsMenuOption {
                label: crate::settings_ui::humanize_value(&v.value),
                value: v.value.clone(),
                doc: v.description.lines().next().unwrap_or_default().to_string(),
            })
            .collect()
    } else {
        menu.matching()
            .into_iter()
            .map(|value| SettingsMenuOption {
                label: crate::settings_ui::humanize_value(&value),
                value,
                doc: String::new(),
            })
            .collect()
    };
    if options.is_empty() && menu.roster.is_empty() {
        return None;
    }
    Some(SettingsMenuModel {
        row: menu.row,
        current: current.and_then(|c| options.iter().position(|o| o.value == c)),
        selected: menu.selected.min(options.len().saturating_sub(1)),
        searchable: menu.searchable(),
        filter: menu.filter.text().to_string(),
        filter_caret: caret_of(&menu.filter),
        scroll: menu.scroll,
        ensure_visible: menu.scroll_to_selected,
        footer,
        options,
    })
}

/// A field's caret, as the chrome model wants it.
fn caret_of(field: &TextField) -> crate::chrome::model::Caret {
    crate::chrome::model::Caret { at: field.caret(), selection: field.selection() }
}

fn push_code_chars(edit: &mut crate::settings_ui::EditBuffer, text: &str) {
    for ch in text.chars() {
        // The code is ASCII, so bytes and characters agree here. The clamp
        // counts what will *remain*: with a selection open — ⌘A before a
        // paste, the obvious way to replace a code — the first insert
        // removes it, and comparing the whole buffer would break the loop
        // before it, leaving a full box that can never be retyped.
        let selected = edit.buffer.selection().map_or(0, |(a, b)| b - a);
        if edit.buffer.text().len().saturating_sub(selected) >= ENROLL_CODE_LENGTH {
            break;
        }
        let up = ch.to_ascii_uppercase();
        if ENROLL_CODE_ALPHABET.contains(up) {
            let mut utf8 = [0u8; 4];
            edit.buffer.insert(up.encode_utf8(&mut utf8));
            edit.error = false;
        }
    }
}

/// Whether this window's user is signed in to an account (issue #190).
///
/// `Unknown` is the startup state and stays it until the Fleet screen is
/// first shown: reading the token means touching the keychain, and the
/// keychain stays off the startup path (the `remote_identity` discipline,
/// applied to the token).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AccountState {
    /// Never asked the credential store.
    Unknown,
    SignedOut,
    /// An enrolment worker is in flight; the header says so and offers
    /// nothing clickable until it settles.
    Enrolling,
    /// The browser hand-off (#226) is waiting for someone to click Approve.
    /// Carries the app key's fingerprint — the eight hex characters the
    /// approval page also shows, for the person to compare.
    Linking { fingerprint: String },
    /// A token is stored. The name is `None` when only the token is known —
    /// the account's display name is not persisted, so a restart shows
    /// "signed in" until an enrolment or a hosts fetch supplies it again.
    SignedIn { account: Option<String> },
    /// The account revoked this app (#371): the stored token is refused with
    /// that cause. Not `SignedOut` — the person did not sign out, and their
    /// next move is the fleet screen's Revoked section, not another code.
    Revoked,
    /// This app's device row is `pending` (#371): the token exists and will
    /// work the moment another device approves it. Rendering it as
    /// "not signed in" sent people re-enrolling in circles.
    PendingApproval,
    /// The credential store itself could not be read (#371) — a locked
    /// keychain, a session with no bus. Distinct from `SignedOut` because
    /// "not signed in" about a fully-enrolled machine is the lie that costs
    /// the diagnosis; the message names the store's own error.
    StoreUnreadable(String),
    /// The last enrolment failed; the message is what the header shows
    /// beside the retry affordance.
    Failed(String),
}

/// Where "Enroll this machine" stands (issue #227) — the local card's own
/// little state machine, beside [`AccountState`] rather than inside it: the
/// header describes the *app's* sign-in, this describes the *daemon's*
/// membership, and a signed-in app on an unenrolled machine is the whole
/// point of the button.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LocalEnroll {
    Idle,
    /// The worker is minting a code and carrying it to the daemon; the
    /// button says so and stops being clickable until this settles.
    InFlight,
    /// The daemon claimed the code. Shown on the card until the account
    /// listing refreshes and its `enrolled` row takes over.
    Enrolled { account: Option<String> },
    /// What went wrong, verbatim — the daemon and the control plane both
    /// phrase their refusals as the person's next move.
    Failed(String),
}

/// A daemon-side enrolment failure, as the card should say it.
///
/// The one translation is an old daemon: it answers a message it cannot
/// decode with `Error { "could not understand that message" }` and keeps
/// serving (`on_bytes`), which without this would surface as exactly that
/// string — true, and useless. It becomes the person's actual next move,
/// carrying the already-minted code so nothing is wasted. Everything else
/// is shown verbatim.
fn enroll_failure_text(e: &zest_daemon::DaemonError, code: &str) -> String {
    match e {
        zest_daemon::DaemonError::Refused(m) if m.contains("could not understand") => format!(
            "this daemon predates in-app enrolment; run: zest-daemon --enroll {code}"
        ),
        other => other.to_string(),
    }
}

/// How a machine reads right now, for anything that draws a dot or a word
/// about it.
///
/// **Reachability first, discovery's word second.** `is_online` is the rule
/// #237 established and the fleet cards, the sidebar counts and the ⌘K picker
/// all read — a machine reachable only through the relay has no discovery
/// presence to be `Online` in, and calling it *unseen* while clicking it opens
/// a shell is what that issue was reported for.
///
/// Shared with the tab strip since #297, where `presence` had been
/// hard-coded `Online` since the type was written: three of its four variants
/// had never been produced for a tab, so a session on a machine whose port had
/// stopped answering looked exactly like one on a healthy machine until you
/// typed into it.
fn presence_of(host: &crate::fleet::FleetHost) -> TabPresence {
    if host.is_online() {
        return TabPresence::Online;
    }
    match host.presence {
        zest_mesh::discovery::Presence::Online => TabPresence::Online,
        zest_mesh::discovery::Presence::Away => TabPresence::Away,
        zest_mesh::discovery::Presence::Unseen => TabPresence::Unseen,
        zest_mesh::discovery::Presence::Unreachable => TabPresence::Unreachable,
    }
}

/// The fleet row a tab's shell actually runs on.
///
/// **By id, not by display label.** Two machines may share one — the keying
/// bug #268 fixed in the launcher's group map and again in its provenance
/// lookup. `TabOrigin::Remote` carries the id beside the label since #304,
/// so the origin alone answers this.
///
/// One lookup, because there were two and they disagreed: `presence` was
/// resolved by id here while `LinkKind` a few lines away still matched on the
/// label, so with duplicate labels a tab could report host A's presence beside
/// host B's route — contradictory chrome about one machine, which is worse than
/// either fact being missing.
///
/// A placeholder id (a launch still connecting) falls back to the label,
/// case-insensitively like every other label match here.
fn fleet_host_of<'a>(
    origin: &TabOrigin,
    fleet: &'a [crate::fleet::FleetHost],
) -> Option<&'a crate::fleet::FleetHost> {
    let TabOrigin::Remote { host, label } = origin else { return None };
    if crate::tabs::is_placeholder_host(*host) {
        // `!h.local` is not belt-and-braces: the origin is already `Remote`, so
        // a local match is definitionally wrong — and it is the *worst* wrong
        // answer available, since the local row is loopback and `Online`. A
        // connecting tab to a machine that happens to share this one's display
        // name would read as reaching the desk it is sitting on.
        fleet.iter().find(|h| !h.local && h.label.eq_ignore_ascii_case(label))
    } else {
        fleet.iter().find(|h| h.host == *host)
    }
}

/// First-seen accent slots for remote hosts (#273).
///
/// Rebuilt per chrome refresh in strip order, so a host keeps its colour for
/// the life of the window as long as its *key* is stable. The key is the
/// host id where the tab has a real address — a renamed machine is the same
/// machine, and two machines sharing a display name are still two machines —
/// and the display label only for a placeholder (a launch still connecting),
/// whose address is all-zero and says nothing. Keying placeholders on the id
/// instead would collapse every in-flight cross-host launch into one slot,
/// which is the regression #268 declined.
struct HostSlots {
    /// Each slot's keys, in first-seen order: the host id once a tab of this
    /// slot has a real address, and the display label either way — the label
    /// is a placeholder's only key, and the bridge a settling launch crosses
    /// without changing colour.
    entries: Vec<(Option<zest_proto::HostId>, String)>,
}

impl HostSlots {
    fn new() -> Self {
        Self { entries: Vec::new() }
    }

    /// The slot for this tab's host, minting the next one on first sight.
    fn slot(&mut self, addr: zest_proto::SessionAddr, host_label: &str) -> usize {
        if crate::tabs::is_placeholder(addr) {
            // A launch still connecting has only its label. Matching a slot
            // that already carries an id is deliberate: a second launch to a
            // live host should wear that host's colour from the start.
            if let Some(i) = self.entries.iter().position(|(_, label)| label == host_label) {
                return i;
            }
        } else {
            if let Some(i) = self.entries.iter().position(|(id, _)| *id == Some(addr.host)) {
                return i;
            }
            // No slot knows this id yet, but one may have been opened by this
            // tab's own placeholder a refresh ago. Claiming it — and stamping
            // the id on it — is what keeps a tab's colour across the moment
            // its launch settles; a sibling launch still in flight keeps
            // matching the same slot by label above.
            if let Some(i) =
                self.entries.iter().position(|(id, label)| id.is_none() && label == host_label)
            {
                self.entries[i].0 = Some(addr.host);
                return i;
            }
        }
        let id = (!crate::tabs::is_placeholder(addr)).then_some(addr.host);
        self.entries.push((id, host_label.to_string()));
        self.entries.len() - 1
    }
}

/// A tab's machine, as the fleet sees it.
///
/// **Presence is about the machine; `LinkKind` is about our connection to it.**
/// A daemon that is up while our socket is down is `Online` and
/// `Reconnecting`, and saying both is the point — collapsing them would lose
/// exactly the distinction that tells you whether to wait or to go and look.
fn tab_presence(origin: &TabOrigin, fleet: &[crate::fleet::FleetHost]) -> TabPresence {
    if matches!(origin, TabOrigin::Local) {
        // The window's own machine: we are talking to it, and a broken socket
        // to it is a link fact, not a presence one.
        return TabPresence::Online;
    }
    // A host the fleet has nothing to say about — no discovery record, no
    // account row — is `Unseen` rather than `Online`: we are attached to it, so
    // it was reachable, but nothing here can vouch that it still is.
    fleet_host_of(origin, fleet).map_or(TabPresence::Unseen, presence_of)
}

/// The host dropdown's row for "no pin at all" (#297).
///
/// Parenthesised to make a collision *unlikely*, never impossible: a label is
/// whatever someone typed into their config or advertised over mDNS, so a
/// machine really called `(this machine)` is legal. The menu carries one string
/// per option, so the choice comes back as text with no way to tell the two
/// apart — `profiles_open_host_menu` therefore checks for the collision and
/// declines to open, rather than risking a pick that clears the pin it meant
/// to set. Naming it here rather than trusting the parentheses, because the
/// first version of this comment claimed they were enough.
const HOST_MENU_LOCAL: &str = "(this machine)";

/// The host dropdown's rows: this machine, then the fleet, then whatever the
/// profile already pins if that is neither (#297).
///
/// Pure, so the two rules that matter are `cargo test`ed rather than argued:
/// the local machine appears once and by one spelling, and an existing pin is
/// never dropped from the list that is about to overwrite it.
/// What the host `▾` should open, or `None` to leave it a text field (#297).
///
/// Everything the dropdown decides lives here, pure, because both rules it
/// enforces were wrong first and a comment is not a test:
///
/// - **The local machine appears once**, as [`HOST_MENU_LOCAL`], which writes an
///   empty `host` — the spelling the field's own description already uses, and
///   the one `launch::resolve_host` reads as `Local` and `bucket_for` files
///   under "this machine". Listing it again under its own label would be two
///   rows doing one thing.
/// - **An existing pin is never dropped from the list about to overwrite it.** A
///   profile hand-written for a machine that is off must not become uneditable.
///   Matched case-insensitively, like every other label comparison
///   (`resolve_host`, `bucket_for`): a pin spelled `Forge` finds the host
///   advertising `forge` rather than sitting beside it as a second row.
///
/// `None` in two cases, both of which mean *keep typing*:
///
/// - **Anything already spelled `(this machine)`.** Labels are arbitrary text,
///   so a machine really called that is legal, and the menu carries one string
///   per option — the choice comes back as text with no way to tell the two
///   apart. It bites from **both directions**, which is the part worth
///   remembering: a *fleet host* with that label would clear the pin when
///   picked, and a *profile pinned to* that name — a machine that is simply
///   offline and absent from the snapshot — folds onto the local row, so Enter
///   silently rewrites a real pin to empty. One guard, both sides. A wrong
///   write is worse than no dropdown, and a menu whose rows a person could not
///   tell apart either is not much of a menu.
/// - **Nothing but this machine.** A one-row dropdown is worse than the text
///   field it replaced.
fn host_menu_roster(
    fleet: &[crate::fleet::FleetHost],
    current: Option<&str>,
) -> Option<Vec<String>> {
    let collides = |label: &str| label.eq_ignore_ascii_case(HOST_MENU_LOCAL);
    if fleet.iter().any(|h| !h.local && collides(&h.label))
        || current.is_some_and(collides)
    {
        return None;
    }
    let mut roster = vec![HOST_MENU_LOCAL.to_string()];
    roster.extend(fleet.iter().filter(|h| !h.local).map(|h| h.label.clone()));
    if let Some(current) = current.filter(|c| !c.is_empty()) {
        if !roster.iter().any(|r| r.eq_ignore_ascii_case(current)) {
            roster.push(current.to_string());
        }
    }
    (roster.len() > 1).then_some(roster)
}

/// Which row the host dropdown opens on, in the roster's *own* spelling.
///
/// `MenuState` selects and marks the ✓ by exact string equality, so a pin
/// written `Forge` against a host advertising `forge` matches nothing: the menu
/// opens on row 0 and Enter rewrites the pin to "(this machine)". Losing a ✓ is
/// cosmetic; **a destructive default is not**, which is why this folds rather
/// than handing the profile's spelling straight through.
fn host_menu_selection(roster: &[String], current: Option<&str>) -> String {
    current
        .filter(|c| !c.is_empty())
        .and_then(|c| roster.iter().find(|r| r.eq_ignore_ascii_case(c)).cloned())
        .unwrap_or_else(|| HOST_MENU_LOCAL.to_string())
}

/// A command as a shell would receive it from a person: the bytes, then the
/// Return that runs them.
fn with_return(command: &str) -> Vec<u8> {
    let mut bytes = command.as_bytes().to_vec();
    bytes.push(b'\r');
    bytes
}

/// A card row's value, bounded: a control-plane refusal can run long, and a
/// value that overruns its own label reads as two broken rows.
fn clip_row(text: &str) -> String {
    const MAX: usize = 58;
    if text.chars().count() <= MAX {
        return text.to_string();
    }
    let cut: String = text.chars().take(MAX - 1).collect();
    format!("{cut}\u{2026}")
}

/// Block rows on an empty filter: a glance, so Sessions and Hosts stay above
/// the fold.
const PALETTE_BLOCKS_IDLE: usize = 6;
/// Block rows once something is typed: a list, and the panel scrolls.
const PALETTE_BLOCKS_FILTERED: usize = 24;

/// The query row's fact (#527): hosts that have *answered* the query the
/// palette shows, over the hosts the question reached. An in-process shell
/// is this machine, searched by reading its replica directly.
fn hosts_searched(
    view: Option<&crate::fleet::BlockSearchView>,
    query: &str,
    in_process: bool,
) -> crate::chrome::model::HostsSearched {
    let (answered, asked) = match view {
        Some(v) if v.query == query => (v.answered.len(), v.asked),
        _ => (0, 0),
    };
    let own = usize::from(in_process);
    crate::chrome::model::HostsSearched { answered: answered + own, asked: asked + own }
}

#[cfg(test)]
mod hosts_searched_tests {
    use super::hosts_searched;
    use crate::chrome::model::HostsSearched;
    use crate::fleet::BlockSearchView;

    /// The number beside the list used to be the fleet listing's length
    /// while the list came from attached tabs. It is now the hosts that
    /// answered *this* query — not the fleet, not an older query's answers,
    /// not a host the question never reached.
    #[test]
    fn hosts_searched_counts_answers_not_known_hosts() {
        let host = |n: u8| zest_proto::HostId::from_bytes([n; 32]);
        let view = BlockSearchView {
            query: "cargo".into(),
            asked: 3,
            answered: vec![(host(1), Vec::new()), (host(2), Vec::new())],
        };
        assert_eq!(
            hosts_searched(Some(&view), "cargo", false),
            HostsSearched { answered: 2, asked: 3 },
            "two of the three asked have spoken; the third is pending, not searched"
        );
        assert_eq!(
            hosts_searched(Some(&view), "cargo b", false),
            HostsSearched { answered: 0, asked: 0 },
            "answers to an older query say nothing about this one"
        );
        assert_eq!(
            hosts_searched(None, "", true),
            HostsSearched { answered: 1, asked: 1 },
            "no daemon at all, and an in-process shell: this machine, searched by hand"
        );
    }
}

#[derive(Clone)]
enum PickerAction {
    /// A group label or an empty-state row; Enter does nothing.
    None,
    /// Focus the tab that already shows this session.
    Activate(zest_proto::SessionAddr),
    /// Attach this window to the session.
    Attach { addr: zest_proto::SessionAddr, route: HostRoute },
    /// Create a fresh session on the host.
    Create { host: zest_proto::HostId, route: HostRoute },
    /// Re-run a command from the fleet's history — §6's two gestures, and
    /// only those two: `⏎` types it into the *current* session ("run here"),
    /// `⇧⏎` re-opens the picker to choose a machine ("run on host…").
    ///
    /// **No `origin`.** ⇧⏎ used to activate the session the command came from
    /// and run it there, which was a useful thing and was not what the footer
    /// promised — the point of the gesture is to take something you already
    /// ran and run it *somewhere else*. The origin host is in the chooser's
    /// list like any other, so "run it back where it came from" is still one
    /// keystroke further rather than gone.
    RunBlock { command: String },
    /// A keymap command, dispatched through the same `perform` its chord is.
    Perform(keymap::Action),
    /// Open a full-pane screen (fleet, themes).
    ShowScreen(AppScreen),
}

/// A tab's wake callback: forward to the event loop, translating the
/// window-scoped `Exited` into a tab-scoped `TabExited`.
///
/// The address lives in a shared cell because it is not always known when the
/// callback is built — a created session learns its address from the daemon —
/// and because a supervisor rebind moves it.
fn wake_for(
    proxy: &EventLoopProxy<Wakeup>,
    addr: Arc<parking_lot::Mutex<zest_proto::SessionAddr>>,
    activity: ActivityMap,
) -> impl Fn(Wakeup) + Send + 'static {
    let proxy = proxy.clone();
    move |w| {
        let w = match w {
            Wakeup::Exited => Wakeup::TabExited(*addr.lock()),
            // Stamped here rather than trusted from the sender: the cell is
            // what a supervisor rebind moves, so it is the authority on which
            // session this callback belongs to. For the daemon path this
            // rewrites the address to itself; for the in-process one it is the
            // only place the address exists at all.
            Wakeup::Attention(_, cause) => Wakeup::Attention(*addr.lock(), cause),
            other => other,
        };
        if matches!(w, Wakeup::Redraw) {
            // The sidebar's age column: last time this session produced
            // anything. Stamped here because this callback is the one place
            // every kind of session already reports output through.
            activity.lock().insert(*addr.lock(), std::time::Instant::now());
        }
        let _ = proxy.send_event(w);
    }
}

/// Whether a signal is *news* to this viewer.
///
/// The whole feature in one line: a signal in the tab you are looking at is
/// not news. And "looking at" needs both halves — the same bell arriving while
/// zesterm sits behind a browser is precisely the case the dot exists for, so
/// a rule written on the active tab alone would be silent exactly when it
/// mattered.
///
/// Free, so the rule can be checked without an event loop or a window; the
/// only reason it is not obvious is that it is easy to write as one condition
/// and lose the second.
const fn attention_is_news(is_active: bool, window_focused: bool) -> bool {
    !(is_active && window_focused)
}

/// Last-output instants by session, shared with every tab's wake callback.
pub(crate) type ActivityMap =
    Arc<parking_lot::Mutex<std::collections::HashMap<zest_proto::SessionAddr, std::time::Instant>>>;

/// Park an account state for the event loop and wake it. The cell holds one
/// state — last write wins — because only the newest answer is ever true.
fn post_account(
    update: &Arc<parking_lot::Mutex<Option<AccountState>>>,
    proxy: &EventLoopProxy<Wakeup>,
    state: AccountState,
) {
    *update.lock() = Some(state);
    let _ = proxy.send_event(Wakeup::AccountChanged);
}

/// An enrolment failure as the fleet header should say it — short, and
/// pointed at the person's next move rather than at the mechanism.
/// Why an approval did not happen — `SignedOut` apart, because it also has
/// to flip the account header, which a bare message cannot ask for.
enum ApproveFailure {
    SignedOut,
    Message(String),
}

/// What closing a tab should actually do, once the settings and the tab's own
/// state have both been consulted (#381).
///
/// `Close` is the existing rule and not a synonym for "kill": it drops a
/// remote, dead or exited tab and kills a live local one, exactly as before.
enum CloseDecision {
    Close,
    Detach,
    Ask(crate::chrome::model::ConfirmCloseModel),
}

/// The same three outcomes without the words, so the *policy* can be decided
/// by a function that needs no event loop, no window and no terminal — the
/// rule `visible_approval` and `chrome::hit::wheel_target` already follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClosePolicy {
    Close,
    Detach,
    Ask,
}

/// What the app knows about a tab at the moment something asks to close it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TabFacts {
    /// The child is already gone — this close is bookkeeping.
    already_exited: bool,
    /// The host answered and said the session no longer exists.
    dead: bool,
    /// The session runs on this machine.
    local: bool,
    /// A worker is still dialling; there is no session yet.
    connecting: bool,
    /// A command is running, or a full-screen program has the alt screen.
    busy: bool,
    /// A daemon is holding this session, so letting go of it leaves it
    /// running. False for an in-process pty, which this window owns outright.
    can_detach: bool,
}

/// What closing a tab should do.
///
/// The three short-circuits come first and they are the same fact three times:
/// there is nothing of *ours* to end. An exited child is gone, a dead session
/// is gone, a remote one is somebody else's machine's business, and a
/// connecting one has not started. All four are `Close`, which drops rather
/// than kills — and a question with one answer is not a question.
const fn close_policy(
    action: zest_config::settings::CloseAction,
    confirm_when_busy: bool,
    facts: TabFacts,
) -> ClosePolicy {
    use zest_config::settings::CloseAction;
    if facts.already_exited || facts.dead || !facts.local || facts.connecting {
        return ClosePolicy::Close;
    }
    // Nothing to detach *to*. `finish_close_tab` would drop the tab, which for
    // an in-process pty is a kill however it is spelled — so answering
    // `Detach` here would be the setting quietly doing the one thing it exists
    // to prevent. Closing is all closing can mean without a daemon, and the
    // fallback is rare enough (`--no-daemon`, or one that would not start)
    // that a modal on every ⌘W would be noise about a fact that will not
    // change while the window lives.
    if !facts.can_detach && matches!(action, CloseAction::Detach) {
        return ClosePolicy::Close;
    }
    match action {
        CloseAction::Detach => ClosePolicy::Detach,
        CloseAction::Ask => ClosePolicy::Ask,
        // Independent of the setting on purpose: someone who wants ⌘W to end
        // a shell still wants to be stopped before it ends a build.
        CloseAction::Kill if facts.busy && confirm_when_busy => ClosePolicy::Ask,
        CloseAction::Kill => ClosePolicy::Close,
    }
}

/// What the modal calls the thing it would end.
///
/// Named rather than described: "something is running" is the sentence this
/// modal exists to avoid, because the whole reason to stop someone is that
/// they may have forgotten what.
fn what_is_running(command: Option<&str>, alt_screen: bool) -> String {
    match command.map(str::trim) {
        Some(c) if !c.is_empty() => c.to_string(),
        // Either a running block whose command text never arrived (OSC 133;C
        // with no B, and nothing readable off the grid) or — far more often —
        // no block at all, because the alternate screen records no markers.
        _ if alt_screen => "A full-screen program".to_string(),
        _ => "A command".to_string(),
    }
}

/// The faint line under the close question, when there is a daemon holding
/// the session and Detach is therefore a real answer.
const DETACH_HINT: &str =
    "Detaching leaves it running: the session stays in \u{2318}K, and comes back on the next launch.";

/// …and when there is not. An in-process pty is this window's, so there is
/// nothing on the other side of letting go of it.
const NO_DAEMON_HINT: &str =
    "This tab's shell is owned by this window, so there is nothing to leave it with.";

/// The approval ladder, on the worker's thread: token → `/api/me` (the
/// `userId` the signed statement must name — deliberately fetched per
/// approval, since #210 chose to persist nothing but the token) → build,
/// sign and encode the attestation with this app's key → POST it.
///
/// The window is the full [`zest_mesh::attest::ATTESTATION_TTL_MS`]: the
/// voucher outlives this screen by design — daemons re-verify it for a year
/// — and the control plane clamps anything wider.
fn approve_on_account(
    identity: &zest_mesh::identity::ClientIdentity,
    device: &crate::fleet::AccountDevice,
) -> Result<(), ApproveFailure> {
    use zest_mesh::attest::{
        encode_attestation, sign_attestation, Attestation, ATTESTATION_TTL_MS,
        ATTESTATION_VERSION,
    };
    let signed_out = |e: &crate::cloud::CloudError| {
        matches!(e, crate::cloud::CloudError::SignedOut)
    };
    let failure = |e: crate::cloud::CloudError| {
        if signed_out(&e) {
            ApproveFailure::SignedOut
        } else {
            ApproveFailure::Message(e.to_string())
        }
    };

    let token = crate::cloud::stored_app_token(&zest_mesh::keystore::OsKeyStore)
        .map_err(|e| ApproveFailure::Message(e.to_string()))?
        .ok_or(ApproveFailure::SignedOut)?;
    let api = crate::cloud::HttpsAccountApi::new(
        zest_daemon::enroll::DEFAULT_CONTROL_PLANE,
        zest_cloud::tls::Roots::Platform,
    )
    .map_err(|e| ApproveFailure::Message(e.to_string()))?;
    let account = crate::cloud::fetch_me(&api, &token).map_err(failure)?;

    // A clock before the epoch would mint `iat = 0` — an attestation born
    // expired, refused server-side with an error naming the signature window
    // rather than the actual problem. Refuse here, where the cause can be
    // named.
    let iat = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| {
            ApproveFailure::Message("this machine's clock is set before 1970 — fix it first".into())
        })?
        .as_millis() as u64;
    let attestation = Attestation {
        v: ATTESTATION_VERSION,
        account,
        device: device.id,
        label: device.label.clone(),
        by: identity.client_id(),
        iat,
        exp: iat + ATTESTATION_TTL_MS,
    };
    let sig = sign_attestation(identity, &attestation)
        .map_err(|e| ApproveFailure::Message(e.to_string()))?;
    let blob = encode_attestation(&attestation, &sig)
        .map_err(|e| ApproveFailure::Message(e.to_string()))?;
    crate::cloud::approve_device(&api, &token, device.id, &blob).map_err(failure)
}

/// The devices section's rows, shaped from the account listing and what this
/// window knows about its own key (issue #190: the app as approver).
///
/// Pure, and the verb table lives here alone: a pending row offers Approve,
/// an approved row that is not this app's own key offers Vouch, and the own
/// key offers nothing — a key cannot vouch for itself, and the server would
/// refuse the statement anyway. `own` is the *cached* identity: `None` means
/// the keychain has not been consulted yet, in which case the own row shows
/// a Vouch this window will refuse at click time with the identity loaded —
/// a late refusal with a name, never a keychain read per chrome rebuild.
fn fleet_device_rows(
    devices: &[crate::fleet::AccountDevice],
    own: Option<zest_proto::ClientId>,
) -> Vec<crate::chrome::model::FleetDeviceRow> {
    use crate::chrome::model::{FleetDeviceAction, FleetDeviceRow};
    devices
        .iter()
        .map(|d| {
            let mine = own == Some(d.id);
            let status = if mine {
                "this app"
            } else if d.approved {
                "approved"
            } else {
                "pending"
            };
            FleetDeviceRow {
                label: d.label.clone(),
                detail: format!("{} · {status}", d.kind),
                action: if mine {
                    FleetDeviceAction::None
                } else if d.approved {
                    FleetDeviceAction::Vouch
                } else {
                    FleetDeviceAction::Approve
                },
            }
        })
        .collect()
}

fn enroll_failure(e: &zest_daemon::enroll::EnrollError) -> String {
    use zest_daemon::enroll::EnrollError;
    match e {
        // The one named refusal (#228): an "Add a machine" code typed in
        // here. "Mint a fresh one" would send the person straight back to the
        // same wrong button — twice, as it happened.
        EnrollError::Refused { message, .. } if message == "wrong_kind" => {
            "that code is for a machine — in the browser use Add a device instead".into()
        }
        // The 409 the catch-all below used to swallow (#368): a fresh code
        // hits it identically forever, so "mint a fresh one" was a loop. The
        // Worker's `detail` (#367) says which way out; without one (an older
        // deployment) both ways are named, because either beats the loop.
        EnrollError::Refused { message, detail, .. } if message == "already_enrolled" => {
            match detail.as_deref() {
                Some("other_account") => "this app's key is enrolled with a different account — \
                     manage it from that account's fleet screen"
                    .into(),
                Some("revoked") => "this app was revoked — restore it in the browser (fleet \
                     screen, Revoked section), then sign in again"
                    .into(),
                _ => "this app's key is already enrolled — if it was revoked, restore it in \
                     the browser (fleet screen, Revoked section)"
                    .into(),
            }
        }
        // The Worker deliberately answers a dead code and a bad signature
        // identically (no liveness oracle), so the next move is the same
        // whatever the refusal said: mint a fresh code.
        EnrollError::Refused { .. } => "code not accepted — mint a fresh one".into(),
        EnrollError::Transport(_) => "could not reach the control plane".into(),
        // The store's and the parser's own words are the actionable part.
        other => other.to_string(),
    }
}

/// What a credential-store read means for the header — pure, so the `Err`
/// mapping is testable: for years an unreadable keychain rendered as "not
/// signed in" (#371), which on a fully-enrolled machine is the lie that costs
/// the diagnosis. An `Err` from the store is a fact about the *store*, and
/// the header says so; only a store that answered "nothing there" is signed
/// out.
fn probed_account_state(
    read: Result<Option<String>, zest_daemon::enroll::EnrollError>,
) -> AccountState {
    match read {
        // A stored token is "signed in"; the display name is not persisted,
        // so it stays unnamed until an enrolment or a hosts fetch supplies
        // one.
        Ok(Some(_)) => AccountState::SignedIn { account: None },
        Ok(None) => AccountState::SignedOut,
        Err(e) => {
            tracing::warn!(error = %e, "could not read the app's cloud token");
            AccountState::StoreUnreadable(e.to_string())
        }
    }
}

/// Whether this machine's own daemon says it holds an account token (#537).
///
/// Read off the fleet snapshot's local row rather than asked anywhere: the
/// loopback watcher's offer already carries the fact, keychain-free. Only
/// `Some(true)` counts — `Some(false)` is a readable store holding nothing,
/// and `None` is a daemon that did not say (too old, or a locked store),
/// neither of which justifies a token read.
fn local_daemon_is_enrolled(fleet: &[crate::fleet::FleetHost]) -> bool {
    fleet
        .iter()
        .any(|h| h.local && h.offer.as_ref().and_then(|o| o.has_account_token) == Some(true))
}

/// Settled profile launches, parked by workers for `Wakeup::TabsChanged` —
/// the connecting tab's placeholder address, and the session (or the error
/// its pane will carry).
type PendingLaunches = Arc<
    parking_lot::Mutex<Vec<(zest_proto::SessionAddr, Result<crate::remote::RemoteSession, String>)>>,
>;

/// The approval a remote host is waiting on, shared with the attach workers
/// that learn of it (`Wakeup::PairingChanged` announces every change). One
/// cell for the window rather than per tab: approvals are human-paced, and
/// the prompt names its host.
type PairingCell = Arc<parking_lot::Mutex<Option<PairingPrompt>>>;

/// One pending approval: the host being dialled, the six-digit matching code
/// a person compares on both machines, and when the host stops accepting it.
struct PairingPrompt {
    host: String,
    code: String,
    expires_at: std::time::Instant,
}

/// Drop the prompt once an attach settles — approved, refused, or given up.
/// A free function because the worker threads that call it hold no `&App`.
fn clear_pairing(cell: &PairingCell, proxy: &EventLoopProxy<Wakeup>) {
    if cell.lock().take().is_some() {
        let _ = proxy.send_event(Wakeup::PairingChanged);
    }
}

/// Store a prompt and arm its clock.
///
/// The clock exists because the chrome snapshots the prompt into a *cached*
/// layout: `refresh_chrome` returns early while `chrome_layout` is `Some`,
/// so without one more wake the countdown never moves and an **expired code
/// stays painted for ever** unless some unrelated event happens to
/// invalidate the chrome (found by review on #208). The thread posts at
/// each boundary where the displayed "Xm left" changes, and at expiry it
/// clears the cell itself — nothing else will — and wakes once more.
///
/// A replaced or cleared prompt must not inherit the old clock: every tick
/// re-checks that the cell still holds *this* prompt (code + expiry is
/// identity enough — two prompts can't share a monotonic `expires_at`) and
/// exits silently otherwise, so at most one clock is ever speaking.
fn arm_pairing_prompt(
    cell: &PairingCell,
    host: String,
    code: String,
    expires_in_secs: u32,
    post: Arc<dyn Fn() + Send + Sync>,
) {
    let expires_at = std::time::Instant::now()
        + std::time::Duration::from_secs(u64::from(expires_in_secs));
    *cell.lock() = Some(PairingPrompt { host, code: code.clone(), expires_at });
    post();
    let mine = move |p: &PairingPrompt| p.code == code && p.expires_at == expires_at;
    let remaining = {
        let cell = Arc::clone(cell);
        let mine = mine.clone();
        move || {
            let lock = cell.lock();
            lock.as_ref()
                .filter(|p| mine(p))
                .map(|_| expires_at.saturating_duration_since(std::time::Instant::now()))
        }
    };
    let expire = {
        let cell = Arc::clone(cell);
        move || {
            let mut lock = cell.lock();
            if lock.as_ref().is_some_and(&mine) {
                *lock = None;
                true
            } else {
                false
            }
        }
    };
    spawn_pairing_clock(remaining, expire, post);
}

/// Devices waiting for THIS machine's approval — the inbound half of
/// pairing, as [`PairingCell`] is the outbound wait. Separate on purpose: a
/// window attaching somewhere while its daemon is asked to approve
/// something else must show both honestly. Written by the fleet watcher's
/// thread; `Wakeup::PairingChanged` announces every change here too — the
/// event means "pairing state moved, look again", whichever side.
///
/// A queue, not a slot: the daemon announces each device exactly once, so a
/// second device arriving while the first is on screen must wait its turn
/// rather than overwrite it — an overwritten request could never be
/// answered from the modal at all. Arrival order; [`visible_approval`] says
/// which entry the modal shows.
pub(crate) type ApprovalCell = Arc<parking_lot::Mutex<Vec<ApprovalRequest>>>;

/// One inbound request: who is asking, as the daemon pushed it.
pub(crate) struct ApprovalRequest {
    client: zest_proto::ClientId,
    label: String,
    remote: String,
    code: String,
    expires_at: std::time::Instant,
    /// Esc was pressed: keep the entry — the daemon's tombstone still clears
    /// it — but stop drawing it. Per request, so the next in the queue (and
    /// any later arrival) shows.
    dismissed: bool,
}

/// Which queued request the modal shows: the oldest that is neither
/// dismissed nor expired. One at a time on purpose — two codes on screen is
/// an invitation to compare the wrong one — and every way a request ends
/// (answered, dismissed, expired, tombstoned) advances by making this
/// predicate move on.
fn visible_approval(queue: &[ApprovalRequest], now: std::time::Instant) -> Option<usize> {
    queue.iter().position(|r| !r.dismissed && r.expires_at > now)
}

/// Store an inbound request and arm its clock; the fleet watcher's
/// `PairingEvent::Requested` handler.
fn arm_approval_request(
    cell: &ApprovalCell,
    client: zest_proto::ClientId,
    label: String,
    remote: String,
    code: String,
    expires_in_secs: u32,
    post: Arc<dyn Fn() + Send + Sync>,
) {
    // 0 is "expiry unknown" (a daemon predating the field). Assume the
    // pairing window rather than showing a modal that can never close —
    // its request will have left that daemon's queue by then anyway.
    let secs = if expires_in_secs == 0 {
        zest_mesh::pairing::APPROVAL_TIMEOUT.as_secs()
    } else {
        u64::from(expires_in_secs)
    };
    let expires_at = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    {
        let mut queue = cell.lock();
        // A device asking again replaces its older self (the daemon keys
        // its queue by client for the same reason): one device, one prompt,
        // and the fresh code is the one its screen is showing.
        queue.retain(|r| r.client != client);
        queue.push(ApprovalRequest { client, label, remote, code, expires_at, dismissed: false });
    }
    post();
    // Identity for the clock is (client, expires_at): a replacement carries
    // a fresh monotonic expiry, so the replaced entry's clock finds nothing
    // and dies without touching the newcomer.
    let remaining = {
        let cell = Arc::clone(cell);
        move || {
            let queue = cell.lock();
            queue
                .iter()
                .find(|r| r.client == client && r.expires_at == expires_at)
                .map(|_| expires_at.saturating_duration_since(std::time::Instant::now()))
        }
    };
    let expire = {
        let cell = Arc::clone(cell);
        move || {
            let mut queue = cell.lock();
            let before = queue.len();
            queue.retain(|r| !(r.client == client && r.expires_at == expires_at));
            queue.len() != before
        }
    };
    spawn_pairing_clock(remaining, expire, post);
}

/// The clock both pairing cells share (#208's staleness lesson, held once).
///
/// It exists because the chrome snapshots a prompt into a *cached* layout:
/// `refresh_chrome` returns early while `chrome_layout` is `Some`, so
/// without one more wake a countdown never moves and an **expired code
/// stays painted for ever** unless some unrelated event happens to
/// invalidate the chrome. The thread posts at each boundary where a
/// displayed "Xm" changes, and at expiry it clears the cell itself —
/// nothing else will — and wakes once more.
///
/// A replaced or cleared prompt must not inherit the old clock: every tick
/// re-asks `remaining` — the caller's statement of whether its prompt still
/// stands, and for how long — and exits silently on `None`, so at most one
/// clock is ever speaking for any prompt. `expire` removes exactly the
/// caller's prompt, answering whether it was still there to remove; only
/// then is the removal worth a wake. Closures rather than a cell type so
/// the single-slot prompt and the approval queue share one clock — the
/// staleness rules live once.
fn spawn_pairing_clock(
    remaining: impl Fn() -> Option<std::time::Duration> + Send + 'static,
    expire: impl FnOnce() -> bool + Send + 'static,
    post: Arc<dyn Fn() + Send + Sync>,
) {
    let clock = std::thread::Builder::new().name("zest-pairing-clock".into()).spawn(move || {
        let mut first = true;
        loop {
            // Replaced or cleared: a newer prompt armed its own clock.
            let Some(left) = remaining() else { return };
            // Checked before waking: a clock that outlived its prompt must
            // not keep poking the event loop. The arming call already posted
            // for the first paint.
            if !first {
                post();
            }
            first = false;
            if left.is_zero() {
                if expire() {
                    post();
                }
                return;
            }
            // To the next whole-minute boundary of the displayed countdown
            // (`div_ceil(60)` in both renderings), or to expiry if sooner.
            let secs = (left.as_secs().saturating_sub(1) % 60) + 1;
            std::thread::sleep(std::time::Duration::from_secs(secs).min(left));
        }
    });
    if let Err(e) = clock {
        // The prompt still shows; it just cannot count down or self-clear.
        tracing::warn!(error = %e, "no thread for the pairing clock");
    }
}

/// A window size computed from `window.columns` / `window.rows`, and the font
/// stack it was measured with so the caller need not build one twice.
struct SizedFromCells {
    width: u32,
    height: u32,
    /// The scale the metrics were taken at — the *primary monitor's*, since
    /// there is no window to ask yet. Kept so the caller can tell whether the
    /// window opened somewhere else and the fonts have to be rebuilt.
    scale: f32,
    fonts: Box<Fonts>,
}

/// The live GPU state, created once the window exists.
struct Gpu {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    renderer: Renderer,
    /// The transparency intent this surface was last configured for.
    ///
    /// Remembered so `apply_transparency` can tell a real change from a
    /// reload that happened to share its invalidation class with
    /// `window.backdrop`, without consulting a list of key names.
    transparent: bool,
    /// What this surface can do about alpha, kept from the capability query.
    ///
    /// Asked once and remembered because the answer is a property of the
    /// adapter and cannot change, while the *question* is asked again every
    /// time `window.opacity` moves. Dropping it is what made opacity a
    /// restart-only setting that claimed otherwise.
    alpha_modes: Vec<wgpu::CompositeAlphaMode>,
}

pub struct App {
    config: Config,
    proxy: EventLoopProxy<Wakeup>,
    /// What is one per process (ADR-018): the fleet model, the clipboard,
    /// the placeholder counter. Everything else on this struct is the
    /// window's own.
    shared: std::rc::Rc<Shared>,
    /// What this window wants the process to do — close it, open another —
    /// drained by the process after every dispatch. A window never exits
    /// the loop itself: with several, that would be quitting.
    requests: WindowRequests,

    /// Above `window` on purpose: the surface holds an `Arc<Window>`, and
    /// fields drop in declaration order, so this way the surface goes first.
    gpu: Option<Gpu>,
    window: Option<Arc<Window>>,
    /// The sessions this window holds open, wherever their shells run.
    ///
    /// Each tab's terminal is behind [`SessionSource`], because a window on
    /// this machine showing a shell on the Mac is the point of the whole
    /// project — the renderer cannot tell and must not care.
    tabs: TabStrip,
    /// The full-pane screen over the grid, when one is open; Esc returns.
    screen: AppScreen,
    /// The animation clock's origin. Phases are derived, never stored, so a
    /// missed tick cannot desynchronize anything.
    anim_epoch: std::time::Instant,
    /// A running spinner is on screen — set by the redraw that drew it.
    anim_spin: bool,
    /// A tab is showing a progress ring — set by the *chrome* rebuild.
    ///
    /// Its own flag rather than sharing `anim_spin`, which the block pass owns
    /// and rewrites on every redraw: one value written by two passes is one
    /// pass clearing what the other just set, and the symptom would be a ring
    /// that turns only while a block header happens to be spinning too.
    anim_spin_tabs: bool,
    /// A pulsing session dot is on screen — set by the chrome rebuild.
    anim_pulse: bool,
    /// Where the cursor is *drawn*, in cells, when `cursor.trail` is on.
    ///
    /// Two springs because a cursor moves in two axes and they settle
    /// independently — a caret returning to column 0 on a new line travels
    /// much further horizontally than vertically, and one spring would make
    /// the shorter axis wait for the longer.
    ///
    /// Carries the *visual* position only. The grid's cursor is wherever the
    /// program put it, so input, IME placement and hit testing are unaffected
    /// by an animation still in flight.
    cursor_trail: Option<(crate::motion::Spring, crate::motion::Spring)>,
    /// The visual residue of a scroll, in fractional rows.
    ///
    /// The grid's own `display_offset` still moves in whole rows the moment
    /// the wheel turns — the session, the selection and every hit test stay
    /// integral — and this carries only how far behind the *drawing* is. It
    /// runs from a non-zero offset back to zero, so at rest it contributes
    /// nothing and costs nothing.
    scroll_spring: crate::motion::Spring,
    /// When the last animation frame was integrated, for `dt`.
    ///
    /// One clock: per-animator `Instant::now()` desynchronizes motion, which
    /// is the same argument `anim_epoch` already makes for the blink phase.
    last_anim: Option<std::time::Instant>,
    /// The daemon link is down (`Wakeup::Detached` .. `Reattached`): the
    /// status bar says "reconnecting" in danger until it heals.
    link_down: bool,
    /// Hit regions of the per-frame block headers, consulted where the
    /// cached chrome layout says nothing. Rebuilt every redraw.
    block_hits: crate::chrome::hit::ChromeHitMap,
    /// The live prompt's chips as this frame drew them, and their hit map.
    /// A click resolves its value against exactly what was drawn — the same
    /// one-computation rule the block chrome follows.
    prompt_chips_view: Option<crate::chrome::prompt_chips::PromptChipsView>,
    chip_hits: crate::chrome::hit::ChromeHitMap,
    /// Folded blocks, per session — a view preference, never on the wire:
    /// two clients watching one session may disagree.
    folded_blocks: std::collections::HashMap<
        zest_proto::SessionAddr,
        std::collections::BTreeSet<u32>,
    >,
    /// The selected block, per session. A view decision like `folded_blocks`
    /// beside it, and never on the wire for the same reason: two clients
    /// watching one session may disagree about which block is "the" block.
    /// Keyed by `focused_addr`, so a split tab's two panes select apart.
    ///
    /// **An id, not a row.** The selection has to survive scrolling, folding
    /// and a reflow; all three move rows and none of them moves a `BlockId`.
    selected_block: std::collections::HashMap<zest_proto::SessionAddr, u32>,
    /// When each session last produced output, stamped by the wake callbacks.
    /// Feeds the sidebar's age column; never pruned aggressively — a closed
    /// tab's entry is a few bytes and the map is per-window.
    activity: ActivityMap,
    /// How to reach the daemon this window's tabs live on, for ⌘T and the
    /// redials it implies. One daemon today; the fleet model generalizes it.
    route: Option<HostRoute>,
    /// The identity every daemon tab proves. One per window, minted or loaded
    /// once — N tabs are one client holding N sessions, not N clients.
    client_identity: Option<Arc<zest_mesh::identity::ClientIdentity>>,
    /// Answers to a *local* file read (#464), each carrying the pane it was
    /// for. A remote answer parks on its own session instead; these come from
    /// a worker thread, so they need somewhere of their own to land.
    local_file_replies:
        Arc<parking_lot::Mutex<Vec<(zest_proto::SessionAddr, crate::editor::FileReply)>>>,
    /// The fleet picker's transient state, while open.
    picker: Option<PickerState>,
    palette_ui: Option<PaletteState>,
    /// The cwd chip's directory browser, when open (#439).
    dir_picker: Option<DirPickerState>,
    /// The "Open file…" prompt, while it is up (#464). One of the
    /// mutually-exclusive overlays the app allows at most one of.
    open_file: Option<TextField>,
    /// The find bar's query, while it is open (#519).
    ///
    /// Deliberately **not** exclusive with the other overlays: searching is
    /// something you do *to* the grid you are looking at, so it stays up while
    /// a block menu or the palette is used over it, and the grid keeps its own
    /// selection underneath.
    find: Option<TextField>,
    /// What the last scan found for `find`'s query.
    find_state: crate::find::FindState,
    settings_ui: Option<SettingsUiState>,
    /// The Profiles tab's editor state, while that tab exists (§12).
    profiles_ui: Option<ProfilesUiState>,
    /// Open over the settings overlay while a long-list field is being chosen.
    /// The + launcher menu's transient state, while open — one of the
    /// mutually exclusive overlays, like the three above.
    launcher: Option<LauncherState>,
    /// A block's ⋯ menu, while open — one of the mutually exclusive overlays.
    block_menu: Option<BlockMenuState>,
    /// Which tabs have asked to be noticed since you last looked at them.
    ///
    /// **This viewer's own bit, not the host's.** With two devices watching
    /// one shell there is no answer to who should clear a flag kept over
    /// there, so the host reports the moment and every client keeps its own
    /// idea of what it has seen. Keyed by address, so a tab closed and
    /// reopened does not inherit one.
    attention: std::collections::HashMap<zest_proto::SessionAddr, zest_proto::AttentionCause>,
    /// A close that is waiting for an answer (#381): ⌘W or a chip × landed on
    /// a tab that is still running something.
    ///
    /// Holds the model rather than just the address, so what the person was
    /// answering about is fixed at the moment the question was asked — a tab
    /// whose command finishes while the modal is up must not silently become
    /// a different question.
    confirm_close: Option<crate::chrome::model::ConfirmCloseModel>,
    /// The `⋯` rect the block pass drew last frame, and whose block. The menu
    /// hangs off it, so the affordance and its menu come from one computation
    /// and cannot drift apart.
    block_menu_anchor: Option<(u32, [f32; 4])>,
    /// Where each non-default setting came from, kept from the last resolve —
    /// the settings tab's "set by profile `k8s`" chips read it.
    provenance: std::collections::BTreeMap<String, zest_config::Source>,
    /// Keys the cascade kept that the schema does not know — the settings
    /// tab's ninth category. Kept from the last resolve, like provenance.
    unknown_keys: Vec<String>,
    /// The last settings write that failed, shown as a banner in the overlay.
    settings_error: Option<String>,
    /// A slider drag in progress, by settings row index. The pointer keeps
    /// setting the value until the button releases, even off the track.
    slider_drag: Option<usize>,
    /// Tabs opened by worker threads, waiting for the event loop to adopt
    /// them (`Wakeup::TabsChanged`). The flag says whether the tab takes the
    /// keyboard: picked tabs do, restored ones arrive in the background.
    pending_tabs: Arc<parking_lot::Mutex<Vec<(Tab, bool)>>>,
    /// Profile launches whose worker finished dialling, keyed by the
    /// connecting tab's placeholder address, waiting for the event loop to
    /// settle them live or failed (`Wakeup::TabsChanged`, issue #175).
    pending_launches: PendingLaunches,
    /// A remote host is waiting for a person to approve this device — the
    /// matching code the chrome must show while the attach worker blocks
    /// (#190). Written from worker threads, drawn as the chrome's notice.
    pairing: PairingCell,
    /// A device is waiting for THIS machine's approval — the modal's state,
    /// written by the fleet watcher (`Hello.watch_pairings` pushes). The
    /// process's cell, shared: any window shows it and any may answer.
    approval: ApprovalCell,
    /// Whether this window is signed in to an account. `Unknown` until the
    /// Fleet screen is first shown — reading the token touches the keychain.
    account: AccountState,
    /// The account workers' handoff cell: keychain reads, enrolments and
    /// sign-outs settle here and post `Wakeup::AccountChanged`; the event
    /// loop drains it. Last write wins, which is also the coalescing.
    account_update: Arc<parking_lot::Mutex<Option<AccountState>>>,
    /// Which browser hand-off is current (#226). Every `spawn_link` bumps
    /// it and the poller checks it before each claim and before posting, so
    /// a cancelled or superseded poller stops instead of overwriting the
    /// state its replacement owns. Enrol and sign-out bump it too — any
    /// other door opening closes this one.
    link_generation: Arc<std::sync::atomic::AtomicU64>,
    /// An enrolment code being typed on the Fleet screen. While it exists,
    /// characters belong to it and not to the terminal — the settings tab's
    /// edit-buffer discipline (only `buffer` and `error` are live here;
    /// there is no field index and nothing to append to).
    enroll_entry: Option<crate::settings_ui::EditBuffer>,
    /// Forces an account-listing refresh (the fleet's watcher). `None` until
    /// the Fleet screen first starts it — the fetch reads the stored token,
    /// and the keychain stays off the startup path.
    account_poke: Option<crate::fleet::AccountPoke>,
    /// The fleet snapshot the current chrome model was built from,
    /// index-parallel with the fleet screen's cards, so a card click
    /// resolves against exactly what is on screen — a click racing a fleet
    /// change must not open a different machine.
    fleet_view: Vec<crate::fleet::FleetHost>,
    /// The account devices the current chrome model was built from,
    /// index-parallel with the devices section's rows — the `fleet_view`
    /// discipline, applied to the approver flow (#190): a click racing a
    /// listing refresh must not vouch for a different key.
    devices_view: Vec<crate::fleet::AccountDevice>,
    /// The devices section's last failure, drawn in warn ink under its
    /// title. On App rather than the section model: the model is rebuilt
    /// per chrome pass and would forget it.
    devices_error: Option<String>,
    /// The approve workers' handoff cell (`Some(None)` clears), drained by
    /// the same `Wakeup::AccountChanged` the account cell uses.
    devices_error_update: Arc<parking_lot::Mutex<Option<Option<String>>>>,
    /// The local card's "Enroll this machine" state (issue #227).
    local_enroll: LocalEnroll,
    /// Its worker's handoff cell, riding `Wakeup::AccountChanged` like the
    /// other account workers'.
    local_enroll_update: Arc<parking_lot::Mutex<Option<LocalEnroll>>>,
    fonts: Option<Fonts>,
    palette: zest_core::PaletteSnapshot,

    /// The chrome's resolved palette; rebuilt with the theme.
    chrome_colors: ChromeColors,
    /// Stem darkening, resolved from the user's settings and the theme.
    ///
    /// Stored rather than recomputed per frame: resolving it looks the theme up
    /// by name, and this is read on the render path.
    text_tuning: zest_render_wgpu::TextTuning,
    /// Decoded background pictures, keyed by the settings value that named
    /// them. Owned by the app rather than the renderer because the decoder is,
    /// and re-examined only on a config reload.
    backgrounds: crate::background::Backgrounds,
    /// Last laid-out chrome, shared by redraw and the input path so a click
    /// is tested against exactly what is on screen.
    chrome_layout: Option<ChromeLayout>,
    /// The chrome's own damage latch, beside the session's. Set only by
    /// discrete events (hover, focus, title, config), so 0%-idle holds.
    chrome_dirty: bool,
    /// What the pointer was last over, for hover fills.
    chrome_hover: Option<HitRegion>,
    /// A `Browse…` click waiting for its dialog to be opened.
    ///
    /// **Not opened from the click handler.** A native file dialog runs a
    /// nested modal message loop, which pumps window messages straight back
    /// into winit's dispatch while `window_event` still holds `&mut self` —
    /// re-entering the handler that is already running. It is opened from
    /// `about_to_wait` instead, which is outside every event's dispatch.
    pending_pick: Option<PickRequest>,
    /// What dropping the file currently over the window would do.
    ///
    /// `None` while nothing is being dragged, and while what is being dragged
    /// is something no screen claims. Shown in the window's notice bar, which
    /// is the only feedback available: winit reports a hovered file but not
    /// *where* it is hovering, so there is no row to highlight.
    drop_hint: Option<String>,
    /// The cursor currently set on the window, so a mouse-move that does not
    /// change it costs no Win32 call.
    cursor: winit::window::CursorIcon,
    /// Tab strip scroll offset, physical pixels; layout clamps it.
    strip_scroll: f32,
    /// Bring the active chip into the strip's viewport on the next layout.
    /// Set on activation paths only and cleared once consumed, so wheel
    /// scrolling never snaps back — the overlays' ensure-visible discipline.
    strip_ensure_visible: bool,
    /// Pointer position in physical pixels, for chrome hit tests.
    pointer_pos: (f64, f64),
    /// Debounce for double-clicking the drag area to zoom.
    last_drag_click: Option<std::time::Instant>,
    /// What the OS titlebar currently says, to skip redundant `set_title`s.
    window_title: String,

    scene: Scene,
    modifiers: ModifiersState,
    focused: bool,
    /// Whether the OS currently reports a light appearance.
    ///
    /// Seeded from the window once there is one and updated on
    /// `WindowEvent::ThemeChanged`. Only ever *read* through [`theme_id`],
    /// which decides whether it matters — a window that is not following the
    /// system tracks this anyway, so turning the setting on takes effect
    /// without waiting for the next appearance change.
    system_light: bool,
    /// Which display server this process connected to, asked of the display
    /// handle once the event loop exists. Decides whether `window.opacity`
    /// can move without a relaunch.
    session: crate::platform::Session,
    mouse: MouseState,
    /// Pointer position in cells, updated on every move.
    pointer_cell: (usize, usize),
    /// Why the theme gallery's last clipboard import was refused, shown on
    /// the import card until a retry succeeds or the screen is reopened.
    theme_import_error: Option<String>,
    /// The theme ids the gallery's cards were built from, in card order —
    /// the `fleet_view` rule: a click's index must resolve against the
    /// snapshot the hit map was drawn from, not a roster an import or a
    /// config reload has since reshaped.
    themes_view: Vec<String>,
    /// Composition state for the input method. See `zest_input::ime`.
    ime: zest_input::Ime,
    selection_bg: zest_core::Rgb,
    /// `ui.accent` as the grid speaks it, for the current find hit (#519).
    ///
    /// Cached beside `selection_bg` and refreshed with it: both are theme
    /// facts the renderer wants as `Rgb`, and a second one resolved per frame
    /// would be the same conversion done sixty times a second.
    accent_bg: zest_core::Rgb,
    /// Accumulated fractional wheel lines, so trackpads do not lose precision.
    scroll_accum: f32,

    /// The settings this `config` was projected from.
    ///
    /// Kept alongside rather than discarded, because a reload has to diff
    /// against what is live to decide what the change costs.
    settings: zest_config::Settings,
    /// Flags, replayed on every reload so a `--size` is not lost to a file save.
    cli_layer: toml::Table,
    profile: Option<String>,
    /// Report time-to-first-paint and exit, instead of running a terminal.
    startup_probe: bool,
    /// Own the pty in-process instead of attaching to a daemon.
    ///
    /// For developing against a build whose daemon is broken, and for the
    /// startup measurements that predate the daemon existing.
    no_daemon: bool,
    /// Report the cost of finding and attaching to the daemon, then exit.
    attach_probe: bool,
    /// Start a fresh session rather than picking up an idle one.
    new_session: bool,
    /// Attach to another machine's daemon at `host:port` instead of this
    /// machine's socket. The M3 win condition's last mile: the same window,
    /// the same protocol, a different machine's shell.
    attach_addr: Option<String>,
    /// `--screenshot`: render one frame to a PNG, then exit.
    screenshot: Option<Screenshot>,
    /// When that frame may be taken. Armed once the window exists.
    screenshot_at: Option<std::time::Instant>,
    /// `--screen`: the surface to open the window on. Dispatched once, in
    /// `resumed`, after the session exists and before the first frame.
    start_screen: Option<StartScreen>,
    /// `--screen settings-menu`: open this field's dropdown once the tab has
    /// built its rows. Deferred because a row index only exists after the
    /// same pass that draws it — the same-pass discipline, met from the
    /// other side.
    start_menu_key: Option<String>,
    /// Set once the PNG is written (or has failed to write); the event loop
    /// exits at the next opportunity and `main` returns this.
    exit_code: Option<u8>,
}

/// What `--screenshot` was asked for.
///
/// A whole struct for three fields because they only ever travel together, and
/// because `Option<Screenshot>` says "screenshot mode" in the type rather than
/// leaving three loosely-related fields that have to agree.
#[derive(Debug, Clone)]
pub struct Screenshot {
    pub path: std::path::PathBuf,
    /// How long to let the shell draw a prompt before capturing.
    pub delay: std::time::Duration,
    /// Window size in *logical* pixels; the PNG comes out this times the
    /// display's scale factor.
    pub size: (f64, f64),
}

impl Default for Screenshot {
    fn default() -> Self {
        Self {
            path: "zesterm.png".into(),
            // Enough for a local shell to spawn and print a prompt, measured on
            // this machine at ~120ms. Long enough to be boring, short enough
            // that taking several while iterating is not a wait.
            delay: std::time::Duration::from_millis(400),
            size: (960.0, 600.0),
        }
    }
}

/// What `--screen` opens the window on.
///
/// Not `AppScreen`, deliberately: two of these are overlays rather than
/// full-pane screens, and the flag promises a *surface*, not a rendering
/// mechanism. `Settings` is today's ⌘, overlay and becomes the settings tab
/// when that work item lands, through the same call, with no flag change;
/// `Palette` is the ⌘K fleet picker (design screen 6) — not the keymap's
/// "command palette" (⌘⇧P), which is a different overlay with a confusingly
/// adjacent name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum StartScreen {
    Fleet,
    Themes,
    Settings,
    Palette,
    /// The cwd chip's directory browser, listing the session's own cwd —
    /// in-process at launch, so the listing is real (#439).
    DirPicker,
    /// The + launcher menu, open over the default screen (design §1).
    Launcher,
    /// The Profiles tab (design §12 — the placeholder pane, until the
    /// editor's work item lands).
    Profiles,
    /// The Settings tab with the **theme dropdown open** — the one state
    /// `--screenshot` could not otherwise reach, because opening a menu
    /// takes a click. The dropdown is what #259 rebuilt, so a picture of it
    /// has to be available to anyone reviewing this without a keyboard.
    SettingsMenu,
    /// The Profiles tab with the **name entry open** on the first real
    /// profile — `settings-menu`'s argument, for #283: clicking the header
    /// name is a click, and a review of a rename affordance that cannot be
    /// photographed is a review of the diff only.
    ProfilesRename,
    /// The find bar, open over a session with a query already typed (#519).
    /// ⌘F is a keystroke, so a picture of the bar needs a way in that is not
    /// a keyboard — `settings-menu`'s argument.
    Find,
    /// The "Open file…" prompt (#464) — `settings-menu`'s argument: a prompt
    /// that takes a keystroke to reach cannot otherwise be photographed.
    OpenFile,
    /// A pane holding a file, opened on this repository's own `README.md`, so
    /// the gutter, the wrapping-free long lines and the header all have real
    /// content in the picture rather than a fixture's.
    Editor,
}

impl App {
    pub fn new(
        resolved: zest_config::Resolved,
        cli_layer: toml::Table,
        profile: Option<String>,
        proxy: EventLoopProxy<Wakeup>,
        shared: std::rc::Rc<Shared>,
    ) -> Self {
        // Taken whole rather than as bare settings: provenance is the part
        // of a resolve that is easy to drop and expensive to add back — the
        // settings tab's "set by ..." chips are built from it, and its
        // unknown-keys category from the keys the cascade kept.
        let zest_config::Resolved { settings, provenance, unknown_keys } = resolved;
        let config = Config::from(&settings);
        // Imported themes join the roster before the first resolve, or a
        // window configured onto one flashes the fallback at every launch.
        crate::themes::reload();
        // `false` because there is no window yet to ask, and guessing light on
        // a dark desktop would flash the wrong theme. `resumed` seeds the real
        // answer and `apply_theme` corrects this the moment it can.
        let theme = crate::themes::get(theme_id(&config, false))
            .unwrap_or_else(zest_theme::builtin::obsidian);
        let resolved = zest_theme::resolve(&theme);
        let palette = to_core_palette(&resolved);
        let chrome_colors =
            ChromeColors::new(&theme.ui, &theme.effects, config.chrome_opacity, config.opacity);
        let text_tuning = resolve_text_tuning(&config);
        let selection_bg = zest_core::Rgb::new(
            resolved.selection_bg.r,
            resolved.selection_bg.g,
            resolved.selection_bg.b,
        );
        let accent_bg =
            zest_core::Rgb::new(theme.ui.accent.r, theme.ui.accent.g, theme.ui.accent.b);

        Self {
            config,
            text_tuning,
            proxy,
            approval: Arc::clone(&shared.approval),
            activity: Arc::clone(&shared.activity),
            shared,
            requests: WindowRequests::default(),
            window: None,
            gpu: None,
            tabs: TabStrip::default(),
            screen: AppScreen::Terminal,
            anim_epoch: std::time::Instant::now(),
            anim_spin: false,
            anim_spin_tabs: false,
            anim_pulse: false,
            cursor_trail: None,
            scroll_spring: crate::motion::Spring::at(0.0),
            last_anim: None,
            link_down: false,
            block_hits: crate::chrome::hit::ChromeHitMap::default(),
            prompt_chips_view: None,
            chip_hits: crate::chrome::hit::ChromeHitMap::default(),
            folded_blocks: std::collections::HashMap::new(),
            selected_block: std::collections::HashMap::new(),
            route: None,
            client_identity: None,
            local_file_replies: Arc::default(),
            picker: None,
            palette_ui: None,
            dir_picker: None,
            open_file: None,
            find: None,
            find_state: crate::find::FindState::default(),
            settings_ui: None,
            profiles_ui: None,
            launcher: None,
            block_menu: None,
            block_menu_anchor: None,
            attention: std::collections::HashMap::new(),
            confirm_close: None,
            provenance,
            unknown_keys,
            settings_error: None,
            slider_drag: None,
            pending_tabs: Arc::new(parking_lot::Mutex::new(Vec::new())),
            pending_launches: Arc::new(parking_lot::Mutex::new(Vec::new())),
            pairing: Arc::new(parking_lot::Mutex::new(None)),
            account: AccountState::Unknown,
            account_update: Arc::new(parking_lot::Mutex::new(None)),
            link_generation: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            enroll_entry: None,
            account_poke: None,
            fleet_view: Vec::new(),
            devices_view: Vec::new(),
            devices_error: None,
            devices_error_update: Arc::new(parking_lot::Mutex::new(None)),
            local_enroll: LocalEnroll::Idle,
            local_enroll_update: Arc::new(parking_lot::Mutex::new(None)),
            fonts: None,
            palette,
            chrome_colors,
            backgrounds: crate::background::Backgrounds::default(),
            chrome_layout: None,
            chrome_dirty: true,
            chrome_hover: None,
            pending_pick: None,
            drop_hint: None,
            cursor: winit::window::CursorIcon::Default,
            strip_scroll: 0.0,
            strip_ensure_visible: false,
            pointer_pos: (0.0, 0.0),
            last_drag_click: None,
            window_title: String::new(),
            scene: Scene::default(),
            modifiers: ModifiersState::empty(),
            focused: true,
            // There is no window yet to ask, and guessing light would flash a
            // light theme on a dark desktop. `resumed` seeds it for real.
            system_light: false,
            session: crate::platform::Session::default(),
            mouse: MouseState::default(),
            pointer_cell: (0, 0),
            theme_import_error: None,
            themes_view: Vec::new(),
            ime: zest_input::Ime::new(),
            selection_bg,
            accent_bg,
            scroll_accum: 0.0,
            settings,
            cli_layer,
            profile,
            startup_probe: false,
            no_daemon: false,
            attach_probe: false,
            new_session: false,
            attach_addr: None,
            screenshot: None,
            screenshot_at: None,
            start_screen: None,
            start_menu_key: None,
            exit_code: None,
        }
    }

    /// Render one frame to a PNG and exit, without ever showing the window.
    ///
    /// The point of doing this in the app rather than in `render_dump` is that
    /// this is the *real* path: real `Insets`, real chrome, real theme, real
    /// fonts, real scale factor. A dump that agreed with the renderer but not
    /// with the window would be worse than none — #44 was invisible to the
    /// renderer-level tool precisely because the padding is not the renderer's
    /// business.
    ///
    /// And nothing is ever presented, so this needs no screen-capture
    /// permission, disturbs nothing on screen, and works over SSH or in CI —
    /// none of which is true of asking the OS for a screenshot.
    #[must_use]
    pub fn with_screenshot(mut self, shot: Screenshot) -> Self {
        self.screenshot = Some(shot);
        self
    }

    /// Measure time to first paint, print it, and exit.
    ///
    /// A flag rather than a `#[test]`: first paint means a real window on a real
    /// compositor, which a headless test runner cannot produce. This makes the
    /// number a command anyone — or any CI job with a desktop — can run, instead
    /// of an assertion that would have to be silently skipped.
    #[must_use]
    pub fn with_startup_probe(mut self) -> Self {
        self.startup_probe = true;
        self
    }

    /// Keep the pty in this process rather than attaching to a daemon.
    pub fn with_no_daemon(mut self) -> Self {
        self.no_daemon = true;
        self
    }

    /// What this window's process should exit with, if it has decided.
    ///
    /// `Some` only when `--screenshot` has written its file, or failed to.
    /// Read by the process rather than acted on here, so the exit runs every
    /// destructor on the way out.
    #[must_use]
    pub(crate) fn exit_code_raw(&self) -> Option<u8> {
        self.exit_code
    }

    pub(crate) fn window_id(&self) -> Option<WindowId> {
        self.window.as_ref().map(|w| w.id())
    }

    /// Whether this window's strip holds `addr`, as a tab or a pane — the
    /// process's routing question for a wakeup that names a session.
    pub(crate) fn owns(&self, addr: zest_proto::SessionAddr) -> bool {
        self.tabs.owns(addr)
    }

    pub(crate) fn take_requests(&mut self) -> WindowRequests {
        std::mem::take(&mut self.requests)
    }

    /// Take a tab out of this window, whole, for another window to adopt
    /// (#501). Neither killed nor detached: the session, its connection and
    /// its wake callback go with it. A window emptied by this asks to close,
    /// exactly as the last tab closing does.
    pub(crate) fn take_tab(&mut self, addr: zest_proto::SessionAddr) -> Option<Tab> {
        let was_active = self.tabs.is_active(addr);
        let tab = self.tabs.close(addr)?;
        self.attention.remove(&addr);
        self.requests.persist = true;
        if self.tabs.is_empty() {
            self.request_close();
        } else if was_active {
            self.after_activation();
            self.relayout_grid();
        } else {
            self.mark_chrome_dirty();
        }
        Some(tab)
    }

    pub(crate) fn route(&self) -> Option<&HostRoute> {
        self.route.as_ref()
    }

    pub(crate) fn client_identity(&self) -> Option<Arc<zest_mesh::identity::ClientIdentity>> {
        self.client_identity.clone()
    }

    pub(crate) fn restore_enabled(&self) -> bool {
        self.config.tabs.restore
    }

    /// Bring this window to the front — a second launch's window, or the
    /// tab it opened, must not appear behind the shell it was launched from.
    pub(crate) fn focus(&self) {
        if let Some(w) = self.window.as_ref() {
            w.focus_window();
        }
    }

    /// Open a tab on this window's route: the ⌘T path, a profile launch, or
    /// one of the app tabs — what a second launch asked for, or what a new
    /// window's first tab is.
    pub(crate) fn open_tab(&mut self, open: &TabOpen) {
        match open {
            TabOpen::Shell { command, cwd } => {
                // A plain shell, so no profile and no profile environment.
                // `TabOpen::Profile` below is the arm that carries one, and it
                // goes through `launch_profile` to get it.
                self.open_shell_tab(command.clone(), None, cwd.clone(), Vec::new());
            }
            TabOpen::Profile(name) => self.launch_profile(name),
            TabOpen::Settings => self.open_settings_tab(),
            TabOpen::Profiles => self.open_profiles_tab(),
        }
    }

    /// Where the window stands now, as the OS reports it; the default when
    /// there is no window yet.
    pub(crate) fn current_geometry(&self) -> Geometry {
        let Some(w) = self.window.as_ref() else { return Geometry::default() };
        let size = w.inner_size();
        Geometry {
            inner_size: Some([size.width, size.height]),
            position: w.outer_position().ok().map(|p| [p.x, p.y]),
            maximized: w.is_maximized(),
        }
    }

    /// This window's own daemon, for the fleet listing: its host id and
    /// label from the signed Welcome of the tab it attached. `None` for an
    /// in-process window, which has no daemon to list.
    pub(crate) fn local_host_label(&self) -> Option<(zest_proto::HostId, String)> {
        let tab = self.tabs.active()?;
        if crate::tabs::is_placeholder(tab.addr) {
            return None;
        }
        let label = match tab.source().origin() {
            Origin::Daemon { host, .. } => host,
            Origin::InProcess => return None,
        };
        Some((tab.addr.host, label))
    }

    /// `--screen fleet` showed the screen before the fleet model existed, so
    /// its account watch found nothing to start; the process calls this once
    /// the model is up.
    pub(crate) fn after_fleet_started(&mut self) {
        if self.screen == AppScreen::Fleet {
            self.start_account_watch();
        }
    }

    /// Always start a new session, never adopt an idle one.
    #[must_use]
    pub fn with_new_session(mut self) -> Self {
        self.new_session = true;
        self
    }

    /// Print what attaching to the daemon cost, then exit.
    ///
    /// A failing command rather than a `#[test]`, for the same reason
    /// `--startup-probe` is one: this measures a real process reaching a real
    /// socket, and an assertion CI silently skips for want of a daemon
    /// protects nothing. → ADR-007.
    pub fn with_attach_probe(mut self) -> Self {
        self.attach_probe = true;
        self
    }

    /// Attach to another machine's daemon instead of this machine's.
    #[must_use]
    pub fn with_attach_addr(mut self, addr: String) -> Self {
        self.attach_addr = Some(addr);
        self
    }

    /// Open the window on this surface instead of the terminal.
    ///
    /// Composes with `--screenshot` so every design screen is capturable
    /// headlessly, and stands alone for demos — the window simply opens there.
    #[must_use]
    pub fn with_start_screen(mut self, screen: StartScreen) -> Self {
        self.start_screen = Some(screen);
        self
    }

    /// Find or start this machine's daemon and attach a session to it.
    ///
    /// `None` means fall back to an in-process pty. **Never an error the caller
    /// has to handle**: a terminal that refuses to open because a helper binary
    /// is missing has failed at the only job it has, and both paths already
    /// exist behind `SessionSource`.
    /// Takes no `CommandSpec`: what the daemon is told to run is the *user's*
    /// configured shell, not the one `build_spec` built for an in-process pty.
    /// The two differ once a shell hook has been injected, and the daemon does
    /// its own injecting.
    fn attach_to_daemon(
        &mut self,
        cols: u16,
        rows: u16,
        proxy: &EventLoopProxy<Wakeup>,
        restore: Option<zest_proto::SessionAddr>,
    ) -> Option<Tab> {
        if self.no_daemon {
            if self.attach_probe {
                eprintln!("FAIL: --attach-probe measures the daemon path, which --no-daemon disables");
                std::process::exit(2);
            }
            return None;
        }

        if let Some(addr) = self.attach_addr.clone() {
            return self.attach_remote(&addr, cols, rows, proxy);
        }

        let socket = zest_daemon::default_socket_path();
        let attached = match zest_daemon::find_or_spawn(&socket, DAEMON_START_TIMEOUT) {
            Ok(a) => a,
            Err(e) => {
                // Deliberately not "keeping this session in-process": this is
                // also the reconnect path, where the caller retries instead.
                tracing::warn!(error = %e, "could not reach a daemon");
                // A probe that silently turns into a terminal is worse than one
                // that fails: it hangs, having measured nothing, and the window
                // it leaves behind looks like success.
                if self.attach_probe {
                    eprintln!("FAIL: could not reach a daemon: {e}");
                    std::process::exit(1);
                }
                return None;
            }
        };
        let connect_ms = attached.elapsed.as_secs_f64() * 1000.0;
        let spawned = attached.spawned;

        // An ephemeral key, minted per launch and never stored.
        //
        // On loopback the socket's permissions are the authorization -- a
        // process that can reach it runs as this user and could ptrace the
        // daemon anyway -- so proving a *stored* key would buy nothing and
        // would put the OS keychain, and its prompt, on the startup path. The
        // handshake still runs, because the wire is uniform.
        let identity = match zest_mesh::identity::ClientIdentity::generate().map(Arc::new) {
            Ok(i) => i,
            Err(e) => {
                tracing::warn!(error = %e, "no randomness for a client key; staying in-process");
                return None;
            }
        };

        let started = std::time::Instant::now();
        let addr_cell = Arc::new(parking_lot::Mutex::new(crate::tabs::placeholder_addr(
            self.shared.mint_placeholder(),
        )));
        let wake = wake_for(proxy, Arc::clone(&addr_cell), Arc::clone(&self.activity));
        // The first connection is already open — it was made above so that its
        // cost could be measured and so a failure is a fallback rather than a
        // window that opens onto nothing. So the dialer hands those halves over
        // once and dials for itself every time after, which is exactly the
        // reconnect path. One code path, not two that drift.
        let first = std::sync::Mutex::new(Some((attached.read, attached.write)));
        let redial_socket = socket.clone();
        let dial: crate::remote::Dialer = Box::new(move || {
            if let Some(halves) = first.lock().expect("dial lock").take() {
                return Ok(halves);
            }
            let a = zest_daemon::find_or_spawn(&redial_socket, DAEMON_START_TIMEOUT)
                .map_err(|e| crate::remote::RemoteError::Io(e.to_string()))?;
            Ok((a.read, a.write))
        });

        let opts = crate::remote::AttachOptions {
            identity: &identity,
            label: "zesterm",
            // What the *user* asked for, not what `build_spec` made of it —
            // empty meaning the host's own default shell, exactly as `new_tab`
            // and `split_right` already send it. Forwarding the built command
            // line would send the daemon a line with this process's shell hook
            // already dot-sourced into it, so the daemon would inject a second
            // one and `--no-shell-integration` would quietly stop meaning
            // anything. The daemon is better placed to choose anyway: it is the
            // process that will do the spawning.
            command: self.config.shell.as_deref().unwrap_or_default(),
            cwd: "",
            env: &self.config.shell_env,
            // Restore reattaches sessions that already exist; a created one
            // here has no profile behind it.
            profile: "",
            cols,
            rows,
            scrollback: self.config.scrollback,
            // Restore replaced adoption for the GUI (#23): a launch reopens
            // what this window was showing, or starts fresh — it never again
            // guesses at a session another machine may be driving. `--attach`
            // keeps adopting; see `attach_remote`.
            adopt: false,
            local: true,
            // Loopback: the socket already answered "is this my machine",
            // and there is no advertisement to have been misled by.
            expect_host: None,
            // ...and never consults the trust store, so nothing can pend.
            on_pending: None,
        };
        let session = match restore {
            Some(addr) => {
                let retry_wake = wake_for(proxy, Arc::clone(&addr_cell), Arc::clone(&self.activity));
                crate::remote::RemoteSession::attach_existing(dial, addr, &opts, wake).or_else(
                    |e| {
                        // The remembered session ended while the window was
                        // closed. A fresh shell is the honest launch; the
                        // stale entry is overwritten at the next persist.
                        tracing::warn!(error = %e, "the remembered session is gone; starting fresh");
                        let socket = socket.clone();
                        let fresh: crate::remote::Dialer = Box::new(move || {
                            let a = zest_daemon::find_or_spawn(&socket, DAEMON_START_TIMEOUT)
                                .map_err(|e| crate::remote::RemoteError::Io(e.to_string()))?;
                            Ok((a.read, a.write))
                        });
                        crate::remote::RemoteSession::create_and_attach(fresh, &opts, retry_wake)
                    },
                )
            }
            None => crate::remote::RemoteSession::create_and_attach(dial, &opts, wake),
        };

        match session {
            Ok(session) => {
                let attach_ms = started.elapsed().as_secs_f64() * 1000.0;
                tracing::debug!(
                    connect_ms = format!("{connect_ms:.2}"),
                    attach_ms = format!("{attach_ms:.2}"),
                    spawned,
                    "daemon attached"
                );
                if self.attach_probe {
                    println!("daemon_connect_ms={connect_ms:.2}");
                    println!("attach_keyframe_ms={attach_ms:.2}");
                    println!("daemon_spawned={spawned}");
                    // Killed explicitly, because `process::exit` below runs
                    // no destructors — and killed rather than detached now
                    // that nothing adopts: a probe that leaked one shell per
                    // run would rebuild the very pile #23 started from.
                    session.kill();
                    let budget = if spawned { 150.0 } else { 10.0 };
                    let total = connect_ms + attach_ms;
                    if total > budget {
                        eprintln!(
                            "FAIL: attaching took {total:.2}ms, budget is {budget:.0}ms \
                             ({} path)",
                            if spawned { "cold" } else { "warm" }
                        );
                        std::process::exit(1);
                    }
                    std::process::exit(0);
                }
                *addr_cell.lock() = session.addr();
                // ⌘T dials the same daemon this window attached to; recorded
                // only on success, so a new tab never dials a route that
                // already failed once.
                self.route = Some(HostRoute::LocalSocket(socket.clone()));
                self.client_identity = Some(Arc::clone(&identity));
                Some(Tab::daemon(session, true, (cols, rows)))
            }
            Err(e) => {
                tracing::warn!(error = %e, "could not attach; keeping this session in-process");
                if self.attach_probe {
                    eprintln!("FAIL: could not attach: {e}");
                    std::process::exit(1);
                }
                None
            }
        }
    }

    /// Attach this window to another machine's daemon.
    ///
    /// Unlike the loopback path this **fails loudly instead of falling back**:
    /// a user who asked for a specific machine and silently got a local shell
    /// has a window that looks right and lies. The in-process pty is a fallback
    /// for "my own daemon is broken", not for "the network said no".
    fn attach_remote(
        &mut self,
        addr: &str,
        cols: u16,
        rows: u16,
        proxy: &EventLoopProxy<Wakeup>,
    ) -> Option<Tab> {
        // Stored, unlike the loopback path, and the difference is the whole
        // point: a remote host has no socket permissions to lean on, so it asks
        // a *person*. An ephemeral key means it asks again on every launch, and
        // a prompt that appears every single time is one people learn to click
        // through without reading -- which costs more security than the stored
        // key does.
        //
        // The keychain is answerable here in a way it is not for the daemon.
        // This is a GUI process with a user in front of it; the daemon is
        // detached and blocks on a prompt nobody can see. That asymmetry is why
        // the fix belongs on this side.
        //
        // Falls back to ephemeral rather than refusing: a machine with no
        // credential store should still be able to reach its fleet, at the cost
        // of approving each time. Saying so out loud, because a silent downgrade
        // to "you will be asked forever" is a mystery rather than a trade-off.
        let store = zest_mesh::keystore::OsKeyStore;
        let identity = match zest_mesh::identity::ClientIdentity::load_or_create(&store) {
            Ok(i) => Arc::new(i),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "no credential store for this device's key; using a throwaway \
                     one, so the far host will ask for approval on every launch"
                );
                match zest_mesh::identity::ClientIdentity::generate().map(Arc::new) {
                    Ok(i) => i,
                    Err(e) => {
                        eprintln!("no randomness for a client key: {e}");
                        std::process::exit(1);
                    }
                }
            }
        };

        let started = std::time::Instant::now();
        let dial_addr = addr.to_string();
        let dial: crate::remote::Dialer = Box::new(move || {
            let stream = std::net::TcpStream::connect(&dial_addr)
                .map_err(|e| crate::remote::RemoteError::Io(e.to_string()))?;
            // A terminal's writes are keystrokes: small, latency-bound, and
            // never worth coalescing.
            let _ = stream.set_nodelay(true);
            let read = stream
                .try_clone()
                .map_err(|e| crate::remote::RemoteError::Io(e.to_string()))?;
            Ok((Box::new(read), Box::new(stream)))
        });

        let addr_cell = Arc::new(parking_lot::Mutex::new(crate::tabs::placeholder_addr(
            self.shared.mint_placeholder(),
        )));
        let wake = wake_for(proxy, Arc::clone(&addr_cell), Arc::clone(&self.activity));
        let session = crate::remote::RemoteSession::attach(
            dial,
            &crate::remote::AttachOptions {
                identity: &identity,
                label: "zesterm",
                // Empty means the *host's* default shell. The local command
                // line would ask a Mac to run this machine's PowerShell.
                command: "",
                cwd: "",
                // Empty, and not this window's `shell.env`: that is a
                // *machine's* setting, and the daemon that spawns the shell
                // applies its own (#488). Sending a Mac's entries to a Linux
                // box would be this window's configuration quietly deciding
                // another machine's shells.
                env: &[],
                profile: "",
                cols,
                rows,
                scrollback: self.config.scrollback,
                adopt: !self.new_session,
                local: false,
                // The address was typed by a person, not learned from an
                // advertisement; pinning a HostId here is future work along
                // with the stored identity.
                expect_host: None,
                // This connect runs inline, before the event loop pumps its
                // first frame, so there is no window to show the code in.
                // `--attach` is launched from a terminal, and stderr is
                // where its user is already looking — it is how the M3
                // bring-up compared codes. Redials after the window is up
                // land here too; harmless where no console exists.
                on_pending: Some(Arc::new(|code, expires_in_secs| {
                    eprintln!(
                        "waiting for approval on the host — compare code {code} \
                         (expires in {}m)",
                        expires_in_secs.div_ceil(60)
                    );
                })),
            },
            wake,
        );

        match session {
            Ok(session) => {
                let attach_ms = started.elapsed().as_secs_f64() * 1000.0;
                tracing::info!(addr, attach_ms = format!("{attach_ms:.2}"), "remote attached");
                if self.attach_probe {
                    println!("remote_attach_ms={attach_ms:.2}");
                    drop(session);
                    // No budget: this number includes the network and, on a
                    // first contact, a human reading a six-digit code.
                    std::process::exit(0);
                }
                *addr_cell.lock() = session.addr();
                self.route = Some(HostRoute::Tcp(addr.to_string()));
                self.client_identity = Some(Arc::clone(&identity));
                Some(Tab::daemon(session, false, (cols, rows)))
            }
            Err(e) => {
                eprintln!("could not attach to {addr}: {e}");
                std::process::exit(1);
            }
        }
    }

    /// Write the selection to the X11/Wayland PRIMARY selection.
    ///
    /// PRIMARY *is* the selection, by definition, which is why writing it on
    /// mouse-up does not contradict the deliberate absence of copy-on-select
    /// just below: that argument is about CLIPBOARD, where replacing what
    /// somebody explicitly copied is the surprise. Nothing here touches
    /// CLIPBOARD.
    ///
    /// Failure is a `debug!`, not a `warn!`: PRIMARY needs
    /// `zwlr_data_control_manager_v1` version 2 on Wayland and plenty of
    /// compositors do not offer it, so this is expected to fail on real
    /// machines and must not fill the log on every drag.
    #[cfg(all(unix, not(target_os = "macos")))]
    fn set_primary(&mut self, text: String) {
        use arboard::{LinuxClipboardKind, SetExtLinux};
        let mut clipboard = self.shared.clipboard.borrow_mut();
        let Some(clipboard) = clipboard.as_mut() else { return };
        if let Err(e) = clipboard.set().clipboard(LinuxClipboardKind::Primary).text(text) {
            tracing::debug!(error = %e, "no PRIMARY selection on this session");
        }
    }

    /// PRIMARY's text, for a middle-click paste, falling back to CLIPBOARD.
    ///
    /// The fallback is what keeps the gesture useful where PRIMARY is
    /// unavailable, and it is also what the old code did by accident -- the
    /// difference is that the selection now wins when there is one.
    #[cfg(all(unix, not(target_os = "macos")))]
    fn primary_text(&mut self) -> Option<String> {
        use arboard::{GetExtLinux, LinuxClipboardKind};
        let mut clipboard = self.shared.clipboard.borrow_mut();
        let clipboard = clipboard.as_mut()?;
        match clipboard.get().clipboard(LinuxClipboardKind::Primary).text() {
            Ok(t) if !t.is_empty() => Some(t),
            _ => clipboard.get_text().ok().filter(|t| !t.is_empty()),
        }
    }

    /// Windows and macOS have no PRIMARY; middle-click reads the clipboard.
    #[cfg(not(all(unix, not(target_os = "macos"))))]
    fn primary_text(&mut self) -> Option<String> {
        self.shared.clipboard.borrow_mut().as_mut()?.get_text().ok().filter(|t| !t.is_empty())
    }

    fn set_clipboard(&mut self, text: String) {
        let mut clipboard = self.shared.clipboard.borrow_mut();
        let Some(clipboard) = clipboard.as_mut() else { return };
        if let Err(e) = clipboard.set_text(text) {
            tracing::warn!(error = %e, "copy failed");
        }
    }

    /// The clipboard text a [`TextCommand`] needs, or `None` for the commands
    /// that need none. Read here rather than in `text_field`, which owns no
    /// handle — and read only on an actual paste, because opening the OS
    /// clipboard on every keystroke is a syscall per character.
    fn paste_text(&mut self, cmd: &TextCommand) -> Option<String> {
        matches!(cmd, TextCommand::Paste)
            .then(|| self.shared.clipboard.borrow_mut().as_mut()?.get_text().ok())
            .flatten()
    }

    fn copy_selection(&mut self) {
        // Read through the session first, then hand the owned text on, so the
        // session borrow does not overlap the `&mut self` clipboard access.
        let text = self
            .tabs
            .active_source()
            .and_then(|s| s.terminal().lock().selection_text());
        if let Some(text) = text {
            self.set_clipboard(text);
        }
    }

    /// Copy what a command printed — not its prompt, not the command.
    ///
    /// The *selected* block while there is one, else the most recent block with
    /// output — see [`Self::target_block`]. That fallback is the whole of the
    /// old behaviour, and it stays the behaviour of a session nobody has
    /// clicked in: at a prompt the cursor's block has printed nothing, which is
    /// almost always.
    fn copy_block_output(&mut self) {
        // Same borrow discipline as `copy_selection`: read through the session,
        // drop the lock, then touch the clipboard behind `&mut self`.
        let text = self.tabs.active_source().and_then(|s| {
            let term = s.terminal().lock();
            let block = self.target_block(&term)?;
            block_actions::output_text(&term, &block)
        });
        match text {
            Some(text) => self.set_clipboard(text),
            // Silent would be indistinguishable from a broken shortcut, and the
            // overwhelmingly likely cause is a shell with no integration rather
            // than a bug.
            None => tracing::info!("no command output to copy -- is shell integration loaded?"),
        }
    }

    /// Fold or unfold one block. One path, shared by the chevron and the menu.
    fn toggle_fold(&mut self, id: u32) {
        let Some(tab) = self.tabs.active() else { return };
        let set = self.folded_blocks.entry(tab.focused_addr()).or_default();
        if !set.remove(&id) {
            set.insert(id);
        }
        // Fold state lives outside the cached layout; the header pass rebuilds
        // per frame, so a redraw is all it takes. The grid layer draws the
        // rail, though, so the scene needs rebuilding too.
        self.chrome_dirty = true;
        if let Some(s) = self.tabs.active_source() {
            s.mark_dirty();
        }
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Open a block's ⋯ menu, anchored at `anchor`.
    ///
    /// Selects the block on the way in: the menu is *about* a block, and one
    /// whose subject is not lit is a menu you cannot check before you click.
    fn open_block_menu(&mut self, id: u32, anchor: [f32; 4]) {
        // The overlays are mutually exclusive — two panels floating over one
        // grid is two things claiming the keyboard.
        self.picker = None;
        self.palette_ui = None;
        self.launcher = None;
        self.set_selected_block(Some(id));
        self.block_menu =
            Some(BlockMenuState { block: id, anchor, selected: 0, actions: Vec::new() });
        self.mark_chrome_dirty();
    }


    /// A closure a worker thread can post a wakeup through.
    fn wakeup_sender(&self) -> impl Fn(Wakeup) + Send + 'static {
        let proxy = self.proxy.clone();
        move |w| {
            let _ = proxy.send_event(w);
        }
    }


    /// Act on a block menu row. Every action closes the menu: the user chose.
    fn run_block_menu_action(&mut self, action: crate::block_menu::BlockMenuAction) {
        use crate::block_menu::BlockMenuAction as A;
        let Some(state) = self.block_menu.take() else { return };
        let id = state.block;
        self.mark_chrome_dirty();

        // Everything that reads the block does so under one short lock and
        // hands back plain data, so the clipboard and tab calls below — all of
        // which need `&mut self` — are outside it.
        let Some(session) = self.tabs.active_source() else { return };
        let block = {
            let term = session.terminal().lock();
            term.blocks().get(zest_core::BlockId(id)).cloned()
        };
        let Some(block) = block else { return };

        match action {
            A::None => {}
            A::Fold => self.toggle_fold(id),
            A::CopyOutput => {
                let text = {
                    let term = session.terminal().lock();
                    block_actions::output_text(&term, &block)
                };
                match text {
                    Some(t) => self.set_clipboard(t),
                    None => tracing::info!("block printed nothing to copy"),
                }
            }
            A::CopyCommand => {
                let c = block.command.trim().to_string();
                if !c.is_empty() {
                    self.set_clipboard(c);
                }
            }
            A::CopyBoth => {
                let text = {
                    let term = session.terminal().lock();
                    block_actions::command_and_output(&term, &block)
                };
                if let Some(t) = text {
                    self.set_clipboard(t);
                }
            }
            A::Rerun => {
                let Some(bytes) = block_actions::rerun_bytes(&block) else { return };
                session.write(bytes);
                // Jump to the bottom, as typing would: re-running something and
                // staying scrolled up means watching a command you cannot see.
                session.terminal().lock().scroll_to_bottom();
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            A::RerunInNewTab => {
                let Some(bytes) = block_actions::rerun_bytes(&block) else { return };
                let cwd = (!block.cwd.is_empty()).then(|| block.cwd.clone());
                // A shell, then the command typed into it — deliberately not
                // `open_shell_tab(Some(command), ..)`, which would make the
                // command the session's *shell* and kill the tab the moment it
                // finished. A pty holds type-ahead, so this needs no callback
                // waiting for the prompt.
                self.open_shell_tab(None, None, cwd, Vec::new());
                if let Some(s) = self.tabs.active_source() {
                    s.write(bytes);
                }
            }
            A::SelectText => {
                // Unfold first: a selection you cannot see is a lie.
                if self.active_folds().is_some_and(|f| f.contains(&id)) {
                    self.toggle_fold(id);
                }
                let Some(session) = self.tabs.active_source() else { return };
                let mut term = session.terminal().lock();
                // Straight from line ids — no `visual_abs_pos`/`select::begin`,
                // which exist to turn a *pointer row* into an `AbsPos` through
                // the fold view. This already holds the lines.
                if let Some(sel) = block_actions::output_selection(&term, &block) {
                    term.set_selection(Some(sel));
                }
                drop(term);
                session.mark_dirty();
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
        }
    }

    /// Copy the output of the block a particular viewport row belongs to.
    ///
    /// What the click can express and the keyboard cannot: a specific command
    /// somewhere up the scrollback, rather than the last one.
    fn copy_block_output_at(&mut self, row: usize) {
        let text = self.tabs.active_source().and_then(|s| {
            let term = s.terminal().lock();
            // Through the fold view: a click's row means whatever the
            // renderer drew there, folded rows included.
            let line = self.visual_line_at(&term, row)?;
            let block = term.blocks().block_at(line).cloned()?;
            block_actions::output_text(&term, &block)
        });
        match text {
            Some(text) => self.set_clipboard(text),
            None => tracing::info!(row, "no command output on that row"),
        }
    }

    /// Run a command again — the selected block's, else the last one with
    /// output ([`Self::target_block`]).
    ///
    /// Sent as [`ClientMessage::Input`](zest_proto::ClientMessage) would be —
    /// the command text and a carriage return. There is no "re-run" message,
    /// because the host would have to take the client's word for the command
    /// anyway, and typing it is exactly what re-running means.
    fn rerun_last_command(&mut self) {
        let Some(session) = self.tabs.active_source() else { return };
        let bytes = {
            let term = session.terminal().lock();
            self.target_block(&term).as_ref().and_then(block_actions::rerun_bytes)
        };
        let Some(bytes) = bytes else {
            tracing::info!("no command to re-run -- is shell integration loaded?");
            return;
        };
        session.write(bytes);
        // Jump to the bottom, as typing would. Re-running something and staying
        // scrolled up in history means watching a command you cannot see.
        session.terminal().lock().scroll_to_bottom();
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Middle-click's paste: PRIMARY first, CLIPBOARD second.
    ///
    /// Split from [`Self::paste`] rather than parameterised, because the two
    /// gestures answer different questions -- ⌘V means "what did I copy", a
    /// middle click means "what is selected" -- and a flag would make the call
    /// sites read as the same intent. It is also why this one stays text-only
    /// where [`Self::paste`] does not: PRIMARY *is* the selection, and a
    /// selection is text -- writing a file to disk on a middle click nobody
    /// meant is worse than doing nothing.
    fn paste_primary(&mut self) {
        let Some(text) = self.primary_text() else { return };
        let Some(session) = self.tabs.active_source() else { return };
        let bytes = session.terminal().lock().encode_paste(&text);
        session.write(bytes);
        session.terminal().lock().scroll_to_bottom();
    }

    /// The clipboard's text, or -- when it holds only a picture -- the path of
    /// a PNG written for it (#532).
    ///
    /// Text first, so nothing about pasting text changes: a copy from a web
    /// page carries both text and an image, and the text is what was meant.
    /// The picture branch is reached exactly where this used to log "nothing
    /// to paste" and send no bytes at all. [`crate::paste_image`] has the why
    /// of a path rather than the bytes.
    fn paste(&mut self) {
        let Some(session) = self.tabs.active_source() else { return };
        let Some(text) = self.clipboard_paste_text(&session.origin()) else { return };
        // The terminal owns the encoding: it knows whether the program asked
        // for bracketed paste, and it normalizes line endings.
        let bytes = session.terminal().lock().encode_paste(&text);
        session.write(bytes);
        session.terminal().lock().scroll_to_bottom();
    }

    /// What a paste should send, or `None` when the clipboard has nothing to
    /// offer this session.
    ///
    /// `&self` deliberately: the caller is holding a `&dyn SessionSource`
    /// borrowed out of `self.tabs`, so a `&mut self` helper would not compile.
    /// Nothing here needs to mutate the app -- the clipboard handle sits behind
    /// its own `RefCell`.
    fn clipboard_paste_text(&self, origin: &crate::source::Origin) -> Option<String> {
        let image = {
            let mut clipboard = self.shared.clipboard.borrow_mut();
            let clipboard = clipboard.as_mut()?;
            match clipboard.get_text() {
                Ok(text) if !text.is_empty() => return Some(text),
                // Both arms fall through. A backend holding a picture may
                // answer either an error or an empty string, and only one of
                // the two used to reach the log line below.
                Ok(_) => {}
                Err(e) => tracing::debug!(error = %e, "no text on the clipboard"),
            }

            // Before the picture is read, not after: a path names a file on
            // *this* machine, so a session whose shell runs somewhere else
            // would only decode a screenshot in order to throw it away. That
            // it must refuse at all is #434's rule -- a local path either
            // finds nothing over there or, worse, finds a different file.
            if origin.is_remote() {
                tracing::info!(
                    "this session's shell runs on another machine; a pasted picture \
                     would name a file it cannot open"
                );
                return None;
            }

            // Copied out, and the borrow released, before any filesystem work:
            // `Shared::clipboard` is process-wide and every window in this
            // process reaches it, so a borrow held across a write and an fsync
            // is one that blocks all of them.
            clipboard.get_image().ok()?
        };

        crate::paste_image::text_for_image(&image.bytes, image.width, image.height)
    }

    /// Run one table-resolved action.
    ///
    /// Exhaustive on purpose — no `_` arm: a new [`keymap::Action`] that can
    /// be reached from the table without being handled here is a compile
    /// error, not a dead shortcut.
    fn perform(&mut self, action: keymap::Action) {
        use keymap::Action;
        match action {
            Action::NewTab => self.new_tab(),
            Action::NewWindow => self.requests.new_window = true,
            Action::MoveTabToNewWindow => {
                // Only a session tab has anywhere to go; the app tabs are
                // singletons of the window that holds them.
                if self.tabs.app_tab_active() {
                    return;
                }
                if let Some(tab) = self.tabs.active() {
                    self.requests.tear_off = Some(tab.addr);
                }
            }
            Action::CloseTab => {
                // App tabs first, whichever holds the pane: closing one is
                // closing a tab (§11's rule), and ⌘W is one of the three
                // ways to say so — the chip's × and middle-click are the
                // others, and all three land in `close_tab`.
                if self.settings_tab_active() {
                    self.close_tab(crate::tabs::settings_addr(), false);
                    return;
                }
                if self.profiles_tab_active() {
                    self.close_tab(crate::tabs::profiles_tab_addr(), false);
                    return;
                }
                // A split tab closes its focused pane first; the tab itself
                // goes on the next ⌘W.
                if self.tabs.active_mut().is_some_and(Tab::close_focused_pane) {
                    self.relayout_grid();
                    self.mark_chrome_dirty();
                    return;
                }
                if let Some(tab) = self.tabs.active() {
                    let addr = tab.addr;
                    self.close_tab(addr, false);
                }
            }
            Action::DetachTab => {
                // Only a session can be detached. The app tabs' own ⌘W rule
                // above does not apply: there is no third outcome for a place.
                if self.tabs.app_tab_active() {
                    return;
                }
                if let Some(tab) = self.tabs.active() {
                    let addr = tab.addr;
                    self.detach_tab(addr);
                }
            }
            Action::ToggleFleetPicker => self.toggle_picker(),
            // The activation family leaves any full-pane screen even when
            // the target is already active: "go to my session" must mean
            // *look at it*, and re-activating the current tab is exactly
            // what someone trapped on the fleet screen tries first.
            Action::ActivateTab(n) => {
                self.leave_screen();
                if self.tabs.activate(usize::from(n)) {
                    self.after_activation();
                }
            }
            Action::ActivateLastTab => {
                self.leave_screen();
                let last = self.tabs.len().saturating_sub(1);
                if self.tabs.activate(last) {
                    self.after_activation();
                }
            }
            Action::PrevTab => {
                self.leave_screen();
                if self.tabs.activate_prev() {
                    self.after_activation();
                }
            }
            Action::NextTab => {
                self.leave_screen();
                if self.tabs.activate_next() {
                    self.after_activation();
                }
            }
            Action::Copy => self.copy_selection(),
            Action::Paste => {
                self.paste();
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            Action::CopyBlockOutput => self.copy_block_output(),
            Action::RerunLastCommand => self.rerun_last_command(),
            Action::ScrollPageUp => self.scroll_page(1),
            Action::ScrollPageDown => self.scroll_page(-1),
            Action::TogglePalette => self.toggle_palette(),
            Action::ToggleSettings => self.open_settings_tab(),
            Action::OpenProfiles => self.open_profiles_tab(),
            Action::ToggleTabLayout => self.toggle_tab_layout(),
            Action::SplitRight => self.split_right(),
            Action::OpenFile => self.open_file_prompt(),
            Action::ToggleFind => self.toggle_find(),
            Action::SplitRightOnHost => self.arm_pending(Pending::Split),
            Action::FocusPaneLeft => self.cycle_pane_focus(-1),
            Action::FocusPaneRight => self.cycle_pane_focus(1),
        }
    }

    /// ⌘U / ⌘J: the keyboard moves one pane over, wrapping.
    fn cycle_pane_focus(&mut self, delta: isize) {
        if self.tabs.active_mut().is_some_and(|t| t.cycle_focus(delta)) {
            self.mark_chrome_dirty();
            if let Some(w) = self.window.as_ref() {
                w.request_redraw();
            }
        }
    }


    /// Flip `tabs.position` through the settings write path, so the config
    /// file stays the single source of truth and the watcher's echo diffs to
    /// nothing — exactly the settings overlay's discipline.
    fn toggle_tab_layout(&mut self) {
        use zest_config::settings::TabsPosition;
        let next = match self.config.tabs.position {
            TabsPosition::Top => "left",
            TabsPosition::Left => "top",
        };
        let Some(target) = zest_config::paths::config_file()
            .or_else(|| zest_config::paths::config_dir().map(|d| d.join(zest_config::paths::CONFIG_FILE)))
        else {
            return;
        };
        match zest_config::write_value(&target, "tabs.position", next.into()) {
            Ok(()) => self.reload_config(),
            Err(e) => tracing::error!(error = %e, "could not write tabs.position"),
        }
        self.mark_chrome_dirty();
    }

    /// The full-pane screen's model, when one is open (design screens 7–8).
    fn build_screen_model(
        &self,
        fleet_hosts: &[crate::fleet::FleetHost],
    ) -> Option<crate::chrome::model::ScreenModel> {
        use crate::chrome::model::{
            FleetAccountAction, FleetAccountModel, FleetCard, ScreenModel, ThemeCard,
        };
        use crate::fleet::SessionsState;
        match self.screen {
            AppScreen::Terminal => None,
            AppScreen::Fleet => {
                // The account header: what the state means is decided here,
                // so screens.rs stays declarative-drawing only. An open code
                // entry replaces the affordance — Enter and Esc own it.
                let account = if let Some(edit) = self.enroll_entry.as_ref() {
                    FleetAccountModel {
                        line: "Sign in with a code".into(),
                        action: FleetAccountAction::None,
                        second: FleetAccountAction::None,
                        entry: Some(crate::chrome::model::SettingsValueCell::Editing {
                            buffer: edit.buffer.text().to_string(),
                            caret: edit.buffer.caret(),
                            selection: edit.buffer.selection(),
                            error: edit.error,
                        }),
                        error: None,
                    }
                } else {
                    match &self.account {
                        AccountState::Unknown => FleetAccountModel {
                            line: String::new(),
                            action: FleetAccountAction::None,
                            second: FleetAccountAction::None,
                            entry: None,
                            error: None,
                        },
                        AccountState::SignedOut => FleetAccountModel {
                            line: "not signed in".into(),
                            action: FleetAccountAction::SignIn,
                            second: FleetAccountAction::SignInBrowser,
                            entry: None,
                            error: None,
                        },
                        AccountState::Enrolling => FleetAccountModel {
                            line: "enrolling…".into(),
                            action: FleetAccountAction::None,
                            second: FleetAccountAction::None,
                            entry: None,
                            error: None,
                        },
                        // The anti-phishing half (#226): the fingerprint here
                        // and the one on the approval page are the same eight
                        // hex characters, and the person compares them.
                        AccountState::Linking { fingerprint } => FleetAccountModel {
                            line: format!("approve in your browser — key {fingerprint}"),
                            action: FleetAccountAction::CancelLink,
                            second: FleetAccountAction::None,
                            entry: None,
                            error: None,
                        },
                        AccountState::SignedIn { account } => FleetAccountModel {
                            line: match account {
                                Some(name) => format!("signed in as {name}"),
                                None => "signed in".into(),
                            },
                            action: FleetAccountAction::SignOut,
                            second: FleetAccountAction::None,
                            entry: None,
                            error: None,
                        },
                        // The honest refusals (#371). Each names the person's
                        // actual next move — the generic "not signed in"
                        // pointed all three at re-enrolling, which for a
                        // revoked app can never work (the 409 loop).
                        AccountState::Revoked => FleetAccountModel {
                            line: "this app was revoked — restore it on the account's fleet screen"
                                .into(),
                            action: FleetAccountAction::SignIn,
                            second: FleetAccountAction::SignInBrowser,
                            entry: None,
                            error: None,
                        },
                        AccountState::PendingApproval => FleetAccountModel {
                            line: "waiting for approval on another device".into(),
                            action: FleetAccountAction::None,
                            second: FleetAccountAction::None,
                            entry: None,
                            error: None,
                        },
                        AccountState::StoreUnreadable(message) => FleetAccountModel {
                            line: "the credential store could not be read".into(),
                            action: FleetAccountAction::None,
                            second: FleetAccountAction::None,
                            entry: None,
                            error: Some(message.clone()),
                        },
                        AccountState::Failed(message) => FleetAccountModel {
                            line: "not signed in".into(),
                            action: FleetAccountAction::SignIn,
                            second: FleetAccountAction::SignInBrowser,
                            entry: None,
                            error: Some(message.clone()),
                        },
                    }
                };
                // The enroll button's gate (issue #227): signed in, and the
                // window's daemon really is the loopback one. `route: None`
                // is the in-process fallback — a pty this process owns, no
                // daemon, nothing an account could list — and a Tcp route
                // means the "local" card is a *remote* machine, whose
                // loopback this app cannot reach. Both hide the button.
                let can_enroll_local = matches!(self.account, AccountState::SignedIn { .. })
                    && matches!(self.route, Some(HostRoute::LocalSocket(_)));
                let cards = fleet_hosts
                    .iter()
                    .map(|h| {
                        // `is_online`, not `presence == Online`: the card is
                        // the surface #237 was reported against, and a machine
                        // reachable only through the relay has no discovery
                        // presence to be `Online` in.
                        let online = h.is_online();
                        let mut rows: Vec<(String, String, u8)> = Vec::new();
                        // The `os` row design §7 asks for, filled at last
                        // (#287). It was absent because nothing could answer
                        // it — `Welcome { host, label }` was the whole
                        // description of a machine — and the rule that kept
                        // it absent still holds: a host that has told us
                        // nothing gets no row, rather than a dash pretending
                        // to be a fact.
                        //
                        // `os_version` first, because it carries the kernel's
                        // *name* as well as its release (`Darwin 24.5.0`) —
                        // `os` is `std::env::consts::OS`, which says `macos`
                        // where the design's card says `Darwin`.
                        //
                        // Falling back to `os` rather than dropping the row:
                        // Windows publishes an empty `os_version` today (the
                        // API is there, the dependency to read it is not), and
                        // `windows` is a poorer row than `Windows 10.0.22631`
                        // but a far better one than nothing. Both empty is a
                        // host that has told us nothing, and gets no row.
                        if let Some(offer) = h.offer.as_ref() {
                            let os = if offer.os_version.is_empty() {
                                offer.os.clone()
                            } else {
                                offer.os_version.clone()
                            };
                            if !os.is_empty() {
                                rows.push(("os".into(), os, 0));
                            }
                        }
                        // Only what is actually known: a path row we cannot
                        // fill would be a dash pretending to be a fact.
                        match h.reachability {
                            Some(zest_mesh::Reachability::Loopback) => {
                                rows.push(("path".into(), "loopback".into(), 1));
                            }
                            Some(zest_mesh::Reachability::Lan) => {
                                let v = match h.rtt_ms {
                                    Some(ms) => format!(
                                        "LAN direct · {}",
                                        crate::chrome::layout::format_ms(ms)
                                    ),
                                    None => "LAN direct".into(),
                                };
                                rows.push(("path".into(), v, 1));
                            }
                            Some(zest_mesh::Reachability::Cloud) => {
                                let v = match h.rtt_ms {
                                    Some(ms) => format!(
                                        "tunnel · {}",
                                        crate::chrome::layout::format_ms(ms)
                                    ),
                                    None => "tunnel".into(),
                                };
                                rows.push(("path".into(), v, 2));
                            }
                            None => {}
                        }
                        // Spine vs decoration made visible (WS-G): the row
                        // says the account holds this machine, whether or
                        // not discovery has ever decorated it.
                        if h.enrolled {
                            rows.push(("account".into(), "enrolled".into(), 1));
                        }
                        // A freshly-settled enrolment's own word, drawn until
                        // the account listing's `enrolled` row takes over
                        // (the poke on settle hurries it). Outside the
                        // button's gate below on purpose: success flips the
                        // daemon's `has_account_token` first, which closes
                        // that gate before the listing catches up — and the
                        // feedback must not vanish in the window between the
                        // two. Skipped when the listing's row is already
                        // drawn, or re-enrolling a machine whose row was
                        // live all along (#245) would print `account` twice.
                        if h.local && !h.enrolled {
                            if let LocalEnroll::Enrolled { account } = &self.local_enroll {
                                let value = match account {
                                    Some(a) => format!("enrolled with {a}"),
                                    None => "enrolled".into(),
                                };
                                rows.push(("account".into(), value, 1));
                            }
                        }
                        let mut enroll = None;
                        // `needs_enrollment`, not `!h.enrolled` (#245): the
                        // row is the account table's fact and what the button
                        // grants is a machine token for the *daemon* — an
                        // enrolled row over a tokenless daemon (post-revoke
                        // restore, a wiped machine, --logout) is exactly when
                        // the affordance is needed most.
                        if h.local && can_enroll_local && h.needs_enrollment() {
                            match &self.local_enroll {
                                LocalEnroll::Idle => {
                                    enroll = Some(crate::chrome::model::FleetEnroll {
                                        label: "Enroll this machine".into(),
                                        clickable: true,
                                    });
                                }
                                LocalEnroll::InFlight => {
                                    enroll = Some(crate::chrome::model::FleetEnroll {
                                        label: "enrolling\u{2026}".into(),
                                        clickable: false,
                                    });
                                }
                                // Drawn above, outside this gate.
                                LocalEnroll::Enrolled { .. } => {}
                                LocalEnroll::Failed(message) => {
                                    rows.push(("account".into(), clip_row(message), 2));
                                    enroll = Some(crate::chrome::model::FleetEnroll {
                                        label: "Enroll this machine".into(),
                                        clickable: true,
                                    });
                                }
                            }
                        }
                        rows.push(("key".into(), h.host.short(), 0));
                        // Session rows for every reachable machine, not just
                        // this one (#265 fetches them, #287 draws them).
                        let mut session_rows = Vec::new();
                        let mut hidden = 0;
                        match &h.sessions {
                            SessionsState::Fresh(sessions) => {
                                let n = sessions.len();
                                let label = if n == 1 {
                                    "1 session".into()
                                } else {
                                    format!("{n} sessions")
                                };
                                rows.push(("sessions".into(), label, 0));
                                hidden = n.saturating_sub(FLEET_CARD_SESSIONS);
                                session_rows = sessions
                                    .iter()
                                    .take(FLEET_CARD_SESSIONS)
                                    .map(|info| {
                                        crate::chrome::model::FleetSessionRow {
                                            // As in the ⌘K rows: a listing
                                            // says nothing about what ran.
                                            title: crate::chrome::model::session_label(
                                                "",
                                                &info.title,
                                            ),
                                            // Home-shortened for this machine
                                            // only — another machine's home is
                                            // unknowable from here.
                                            detail: if h.local {
                                                crate::status::shorten_home(&info.cwd)
                                            } else {
                                                info.cwd.clone()
                                            },
                                            attached: info.attached,
                                            here: self.tabs.iter().any(|t| t.addr == info.addr),
                                        }
                                    })
                                    .collect();
                            }
                            // A dial that keeps failing read exactly like a
                            // machine nobody had asked about. Say which.
                            SessionsState::Failed(message) => {
                                rows.push(("sessions".into(), clip_row(message), 2));
                            }
                            // Never asked, or asked and still waiting: no row
                            // at all, because "0 sessions" would be a claim.
                            SessionsState::Unknown | SessionsState::Fetching => {}
                        }
                        FleetCard {
                            name: h.label.clone(),
                            local: h.local,
                            online,
                            pill: matches!(
                                h.reachability,
                                Some(zest_mesh::Reachability::Cloud)
                            )
                            .then(|| "via tunnel".to_string()),
                            // Honest affordance: no route, no click. The
                            // relay dialler (next PR) is what will give an
                            // account-only card one.
                            open: self.best_route(h).is_some(),
                            rows,
                            enroll,
                            sessions: session_rows,
                            sessions_hidden: hidden,
                        }
                    })
                    .collect();
                // Hosted account data: present exactly while signed in.
                // Every row is the account's, so a signed-out screen has no
                // section rather than an empty one pretending to be a fact.
                let devices = matches!(self.account, AccountState::SignedIn { .. }).then(|| {
                    crate::chrome::model::FleetDevicesModel {
                        rows: fleet_device_rows(
                            &self.devices_view,
                            self.shared.remote_identity.borrow().as_ref().map(|i| i.client_id()),
                        ),
                        error: self.devices_error.clone(),
                    }
                });
                Some(ScreenModel::Fleet { account, cards, devices })
            }
            AppScreen::Themes => {
                let active = if crate::themes::get(self.effective_theme()).is_some() {
                    self.effective_theme().to_string()
                } else {
                    zest_theme::builtin::DEFAULT_DARK.to_string()
                };
                let cards = crate::themes::all()
                    .into_iter()
                    .map(|t| {
                        let c = |x: zest_theme::Rgba8| [x.r, x.g, x.b];
                        let default = match t.mode {
                            zest_theme::ThemeMode::Dark => {
                                t.id == zest_theme::builtin::DEFAULT_DARK
                            }
                            zest_theme::ThemeMode::Light => {
                                t.id == zest_theme::builtin::DEFAULT_LIGHT
                            }
                        };
                        let mode = match t.mode {
                            zest_theme::ThemeMode::Dark => "dark",
                            zest_theme::ThemeMode::Light => "light",
                        };
                        // "imported" is the qualifier's third fact: it is how
                        // two cards may honestly share a display name (a
                        // "Nord" variant beside the built-in Nord).
                        let qualifier = if default {
                            format!("{mode} · default")
                        } else if zest_theme::builtin::get(&t.id).is_none() {
                            format!("{mode} · imported")
                        } else {
                            mode.to_string()
                        };
                        ThemeCard {
                            active: t.id == active,
                            id: t.id,
                            name: t.name,
                            qualifier,
                            bg: c(t.ui.bg),
                            fg: c(t.ui.fg),
                            accent: c(t.ui.accent),
                            danger: c(t.ui.danger),
                            green: c(t.ui.green),
                            // Read from the theme, never re-typed — the strip
                            // is builtin.rs's ANSI row in index order.
                            ansi: t.ansi.normal.map_or([[0; 3]; 8], |row| row.map(c)),
                        }
                    })
                    .collect();
                Some(ScreenModel::Themes {
                    cards,
                    import_error: self.theme_import_error.clone(),
                })
            }
        }
    }

    /// The split tab's pane headers, when the active tab has a split.
    /// Lines a pane body can show, for a tab of `panes` panes.
    ///
    /// One short of what fits, so a partially visible last row is never the
    /// one a scroll clamp counts on — the alternative is a file that appears
    /// to have a line left to go and does not move when you ask for it.
    fn editor_body_rows(&self, panes: usize) -> usize {
        let geometry = self.window.as_ref().zip(self.fonts.as_ref());
        let Some((window, fonts)) = geometry else { return 1 };
        let scale = window.scale_factor() as f32;
        let size = window.inner_size();
        let area = self.insets_at(scale).grid_rect(size.width, size.height);
        let frames = crate::chrome::layout::pane_frames(area, scale, panes);
        let Some(frame) = frames.first() else { return 1 };
        let body = crate::chrome::layout::pane_body(*frame, scale, self.config.padding);
        let cell_h = fonts.cell_metrics().cell_h as f32;
        if cell_h <= 0.0 {
            return 1;
        }
        ((body[3] / cell_h).floor() as usize).saturating_sub(1).max(1)
    }

    /// One pane's cell width and body width, for a wheel landing in a file.
    ///
    /// The cell width is the grid's, so a sideways flick moves a file by the
    /// same distance it moves a terminal.
    fn editor_body_span(&self, pane: usize) -> (f32, f32) {
        let geometry = self.window.as_ref().zip(self.fonts.as_ref());
        let Some((window, fonts)) = geometry else { return (8.0, 0.0) };
        let scale = window.scale_factor() as f32;
        let size = window.inner_size();
        let area = self.insets_at(scale).grid_rect(size.width, size.height);
        let n = self.tabs.active().map_or(1, Tab::pane_count);
        let frames = crate::chrome::layout::pane_frames(area, scale, n);
        let cell_w = fonts.cell_metrics().cell_w as f32;
        let Some(frame) = frames.get(pane) else { return (cell_w, 0.0) };
        let body = crate::chrome::layout::pane_body(*frame, scale, self.config.padding);
        (cell_w, body[2])
    }

    /// The visible slice of an open file, ready for the layout pass.
    fn editor_view(
        &self,
        e: &crate::editor::EditorPane,
        rows: usize,
    ) -> crate::chrome::model::EditorView {
        use crate::chrome::model::EditorView;
        use crate::editor::LoadState;

        let notice = match &e.state {
            LoadState::Loading => Some("opening…".to_string()),
            LoadState::Failed(why) => Some(why.clone()),
            LoadState::Ready if e.binary => {
                Some(format!("{} of binary, not shown", crate::status::human_bytes(e.size)))
            }
            LoadState::Ready if e.line_count() == 0 => Some("empty file".to_string()),
            LoadState::Ready => None,
        };
        let first = e.scroll_line;
        let lines = if notice.is_some() {
            Vec::new()
        } else {
            e.lines()
                .iter()
                .skip(first)
                .take(rows)
                .map(|l| crate::editor::expand_tabs(l).into_owned())
                .collect()
        };
        EditorView {
            first_line: first + 1,
            lines,
            total: e.line_count(),
            scroll_x: e.scroll_x,
            readonly: e.readonly,
            truncated: e.truncated,
            notice,
        }
    }


    /// The animation phases right now, from the shared clock. One clock, so
    /// two spinners can never disagree about the time.
    fn anim_phase(&self) -> crate::chrome::model::AnimPhase {
        let ms = self.anim_epoch.elapsed().as_millis() as u64;
        let blink = u64::from(self.config.cursor_blink_interval_ms.max(100));
        let caret_on = !self.config.cursor_blink || (ms / blink).is_multiple_of(2);
        let spin = (ms % 900) as f32 / 900.0;
        // 1.6s ease-in-out between 1.0 and 0.35, as the design writes it.
        let t = (ms % 1600) as f32 / 1600.0;
        let pulse = 0.675 + 0.325 * (t * std::f32::consts::TAU).cos();
        crate::chrome::model::AnimPhase { caret_on, spin, pulse }
    }

    /// The soonest the clock needs to wake the loop, or `None` when nothing
    /// on screen is animating — which is what keeps 0%-idle true: a resting
    /// window schedules nothing and draws nothing.
    /// Whether anything may animate right now.
    ///
    /// One function every animator asks, so `motion.enabled` and the OS
    /// accessibility setting cannot end up meaning different things in
    /// different places. The OS is queried rather than cached at startup, so
    /// toggling "reduce motion" in System Settings takes effect at the next
    /// reload rather than the next launch.
    fn motion_allowed(&self) -> bool {
        self.config.motion_enabled
            && !(self.config.respect_reduce_motion && platform::reduce_motion())
    }

    /// Advance every spring, and say whether any of them still needs frames.
    ///
    /// The single place `dt` is computed, because two animators reading their
    /// own `Instant::now()` drift apart within a second and the drift is
    /// visible where they meet.
    fn step_motion(&mut self) -> bool {
        // Asked every frame, not only when the wheel turns: `motion.enabled`
        // can go false, or the OS can be asked to reduce motion, *while*
        // something is in flight -- and an animation that carried on after the
        // user switched it off would be one more setting that does not apply.
        if !self.motion_allowed() {
            self.scroll_spring.snap_to(0.0);
            self.cursor_trail = None;
            self.last_anim = None;
            return false;
        }
        if !self.config.smooth_scroll {
            self.scroll_spring.snap_to(0.0);
        }
        if !self.config.cursor_trail {
            self.cursor_trail = None;
        }
        let trail_moving = self.cursor_trail.is_some_and(|(x, y)| x.moving() || y.moving());
        if !self.scroll_spring.moving() && !trail_moving {
            // Nothing in flight: drop the clock so the next animation starts
            // from a fresh `dt` rather than integrating however long the
            // terminal happened to sit idle.
            self.last_anim = None;
            return false;
        }
        let now = std::time::Instant::now();
        let dt = self.last_anim.map_or(1.0 / 60.0, |t| now.duration_since(t).as_secs_f32());
        self.last_anim = Some(now);
        let (response, damping) = (self.config.spring_response, self.config.spring_damping);
        let mut moving = self.scroll_spring.step(dt, response, damping);
        if let Some((x, y)) = self.cursor_trail.as_mut() {
            // Both stepped, not short-circuited: `||` would skip the second
            // axis whenever the first was still moving, and a spring that is
            // not stepped never settles.
            let mx = x.step(dt, response, damping);
            let my = y.step(dt, response, damping);
            moving |= mx || my;
        }
        if !moving {
            self.last_anim = None;
        }
        moving
    }

    fn anim_deadline(&self) -> Option<std::time::Duration> {
        let mut next: Option<u64> = None;
        let mut consider = |ms: u64| next = Some(next.map_or(ms, |n: u64| n.min(ms)));
        // A spring in flight wants the next frame; a spring at rest must add
        // nothing at all, which is what keeps the 0%-idle guarantee true. It
        // reports its own rest rather than being asked about a threshold here,
        // so there is one definition of "settled" and not two.
        if self.scroll_spring.moving() {
            consider(8);
        }
        if self.anim_spin || self.anim_spin_tabs {
            consider(80);
        }
        if self.anim_pulse {
            consider(100);
        }
        let caret_active = self.focused
            && self.config.cursor_blink
            && self.screen == AppScreen::Terminal
            && (!self.tabs.is_empty() || self.picker.is_some());
        if caret_active {
            consider(u64::from(self.config.cursor_blink_interval_ms.max(100)));
        }
        // A guess nothing answers expires on a frame (`predicted` ticks), so
        // while one stands the frames must keep coming; the moment none does
        // this adds nothing, which keeps 0%-idle true.
        if self
            .tabs
            .active_source()
            .is_some_and(|s| s.predicting(self.predict_policy()))
        {
            consider(50);
        }
        next.map(std::time::Duration::from_millis)
    }

    /// `cursor.predict_echo`, as the predictor's own three-way policy.
    fn predict_policy(&self) -> zest_proto::Policy {
        match self.settings.cursor.predict_echo {
            zest_config::PredictEcho::Auto => zest_proto::Policy::Auto,
            zest_config::PredictEcho::Always => zest_proto::Policy::Always,
            zest_config::PredictEcho::Off => zest_proto::Policy::Off,
        }
    }

    /// Back to the terminal if a full-pane screen is up; free otherwise.
    fn leave_screen(&mut self) {
        if self.screen != AppScreen::Terminal {
            self.screen = AppScreen::Terminal;
            self.enroll_entry = None;
            self.mark_chrome_dirty();
        }
    }

    /// Open or close a full-pane screen; closing always lands on the grid.
    /// The settings *tab* survives — a screen draws over it and Esc returns,
    /// exactly as over a session's grid.
    fn show_screen(&mut self, screen: AppScreen) {
        self.screen = screen;
        // A fresh look at the gallery starts with a clean import card — an
        // error from last week answers a question nobody is asking.
        if screen == AppScreen::Themes {
            self.theme_import_error = None;
        }
        self.picker = None;
        self.palette_ui = None;
        self.launcher = None;
        self.block_menu = None;
        // First look at the Fleet screen is when the account becomes worth
        // knowing — and the first acceptable moment to touch the keychain
        // for it (never at startup). On a worker, because an OS credential
        // store is allowed to block on a prompt.
        if screen == AppScreen::Fleet {
            if self.account == AccountState::Unknown {
                self.probe_account();
            }
            // The durable listing refreshes when someone actually looks at
            // it; the first show instead *starts* the watcher (same keychain
            // discipline — its fetch reads the stored token). Poking on the
            // same show that started it would queue a second fetch behind the
            // loop's immediate first one — two keychain reads and, signed
            // out, two back-to-back 401s for one glance at the screen.
            let already_watching = self.account_poke.is_some();
            self.start_account_watch();
            if already_watching {
                if let Some(poke) = self.account_poke.as_ref() {
                    poke.poke();
                }
            }
        }
        // A code entry does not survive leaving the screen that shows it —
        // keys must never keep routing to an input nobody can see.
        if screen != AppScreen::Fleet {
            self.enroll_entry = None;
        }
        self.mark_chrome_dirty();
    }


    /// What to call this machine on the devices screen: the fleet's own name
    /// for the local host (the daemon's label, from its signed Welcome) when
    /// one exists, else the environment's — the daemon's `machine_label`
    /// order, minus the uname arm this crate has no reason to grow.
    fn local_machine_label(&self) -> String {
        if let Some(label) = self.shared.fleet.get()
            .and_then(|f| f.snapshot().into_iter().find(|h| h.local).map(|h| h.label))
        {
            return label;
        }
        for var in ["COMPUTERNAME", "HOSTNAME"] {
            if let Ok(name) = std::env::var(var) {
                if !name.is_empty() {
                    return name;
                }
            }
        }
        "unnamed".to_string()
    }

    /// Open (or activate) the Profiles tab — the ⌘⇧, / Manage-profiles /
    /// `--screen profiles` singleton: at most one exists, and reopening it
    /// shows the one that does.
    fn open_profiles_tab(&mut self) {
        // A tab activation leaves any full-pane screen, like every other
        // activation path; the modals close because the tab takes the
        // keyboard they were holding. (`open_settings_tab`'s preamble — the
        // two app tabs open the same way.)
        self.leave_screen();
        self.picker = None;
        self.palette_ui = None;
        self.launcher = None;
        self.block_menu = None;
        if self.profiles_ui.is_none() {
            self.profiles_ui = Some(ProfilesUiState {
                profile: zest_config::profiles::RESERVED_PROFILE.to_string(),
                selected: 0,
                filter: TextField::default(),
                scroll: 0.0,
                scroll_to_selected: true,
                actions: Vec::new(),
                fields: zest_config::profiles::fields(),
                editing: None,
                renaming: None,
                rename_error: None,
                menu: None,
                error: None,
            });
        }
        self.tabs.open_profiles();
        self.mark_chrome_dirty();
    }

    /// Close the Profiles tab — its state lives as long as the tab, exactly
    /// like the Settings tab's (§11's rule, applied to §12).
    fn close_profiles_tab(&mut self) {
        // Closing the tab is leaving the field, so it commits (#272). Unlike
        // every other exit this one does NOT refuse on a buffer that will not
        // parse: there would be nowhere left to show the warn border, and a
        // ⌘W that silently declines to close is worse than dropping a value
        // that could never have been written.
        let _ = self.profiles_commit_edit();
        let was_active = self.tabs.profiles_active();
        self.profiles_ui = None;
        self.tabs.close_profiles();
        self.settled_after_close(was_active);
    }

    /// The Profiles editor holds the keyboard and the grid area.
    fn profiles_tab_active(&self) -> bool {
        self.tabs.profiles_active() && self.profiles_ui.is_some()
    }

    /// Toggle the + launcher menu (clicking the `+`, `--screen launcher`).
    ///
    /// ⌘T deliberately does NOT come here: the design keeps the default
    /// profile one keystroke away, so the chord spawns it directly and only
    /// the button (whose old direct-spawn behaviour this replaces) opens
    /// the menu.
    fn toggle_launcher(&mut self) {
        self.launcher = match self.launcher {
            Some(_) => None,
            None => {
                // One modal at a time — the exclusivity rule every overlay
                // toggle enforces, so the input blocks stay order-free. The
                // settings TAB is not in this set: it is a place, not an
                // overlay (§11), and the menu floats over it like any pane.
                self.picker = None;
                self.palette_ui = None;
                self.block_menu = None;
                Some(LauncherState {
                    // Row 0 is the default row by construction, so opening
                    // and pressing ⏎ runs the default — the menu's header
                    // is a promise, not a hint.
                    selected: 0,
                    anchor: match self.config.tabs.position {
                        zest_config::settings::TabsPosition::Top => {
                            crate::chrome::model::LauncherAnchor::Strip
                        }
                        zest_config::settings::TabsPosition::Left => {
                            crate::chrome::model::LauncherAnchor::Sidebar
                        }
                    },
                    actions: Vec::new(),
                })
            }
        };
        self.mark_chrome_dirty();
    }

    /// Act on a launcher row. Every action closes the menu: the user chose.
    fn run_launcher_action(&mut self, action: crate::launcher::LauncherAction) {
        use crate::launcher::LauncherAction;
        match action {
            // Neither launch arm calls leave_screen(), on purpose: a
            // successful launch runs through open_shell_tab, which ends in
            // after_activation() — and that steps off any full-pane screen,
            // so launching over the fleet/Profiles view lands on the new tab
            // rather than behind the screen (measured, not assumed). A
            // *failed* launch appended nothing, and leaving the screen for
            // it would trade the view the user had for a warn! in a log.
            LauncherAction::Launch(target) => {
                self.launcher = None;
                self.mark_chrome_dirty();
                self.launch_profile_ref(&target);
            }
            LauncherAction::LaunchDefault => {
                self.launcher = None;
                self.mark_chrome_dirty();
                self.new_tab();
            }
            LauncherAction::RunOnHost(target) => {
                // The fleet picker is the "choose the machine" surface the
                // design points ⇧⏎ at; toggle_launcher's exclusivity closed
                // us already, but the order matters: toggle_picker closes
                // every sibling, so the launcher must go first.
                self.launcher = None;
                self.toggle_picker();
                // Carry the highlighted profile into the picker, so its host
                // row launches *that* row somewhere else (#268). Until then
                // ⇧⏎ dropped it and opened a plain shell, which made the
                // menu's second action look like it did nothing much.
                //
                // Only a local one: `Pending::Profile` names a profile in this
                // machine's config, and a published one is already pinned to
                // the machine that published it — "run forge's `nightly` on
                // pi" would mean sending forge's command line somewhere that
                // never described it.
                if let Some(crate::launcher::ProfileRef::Local(name)) = target {
                    if let Some(p) = self.picker.as_mut() {
                        p.pending = Some(Pending::Profile(name));
                    }
                }
            }
            LauncherAction::ManageProfiles => {
                self.launcher = None;
                self.open_profiles_tab();
            }
            LauncherAction::None => {}
        }
    }

    /// Apply a theme from the gallery, through the settings write path —
    /// the file stays the single source of truth, exactly as the settings
    /// overlay's theme row does it.
    fn apply_theme_choice(&mut self, id: &str) {
        let Some(target) = zest_config::paths::config_file()
            .or_else(|| zest_config::paths::config_dir().map(|d| d.join(zest_config::paths::CONFIG_FILE)))
        else {
            return;
        };
        match zest_config::write_value(&target, "appearance.theme", id.into()) {
            Ok(()) => self.reload_config(),
            Err(e) => tracing::error!(error = %e, "could not write appearance.theme"),
        }
        self.mark_chrome_dirty();
    }

    /// Page the scrollback by one screen less a line of overlap — the overlap
    /// is what makes paged reading continuous rather than guessing where the
    /// seam was.
    fn scroll_page(&mut self, dir: isize) {
        let Some(session) = self.tabs.active_source() else { return };
        let rows = session.terminal().lock().grid().rows();
        let lines = dir * (rows.saturating_sub(1).max(1)) as isize;
        session.terminal().lock().scroll_display(lines);
        pull_history_at_top(session);
        session.mark_dirty();
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Composition from an input method: dead keys, and every script that needs
    /// more keys than a keyboard has.
    ///
    /// Only [`Ime::Commit`] reaches the shell. The preedit is drawn over the
    /// cursor and never enters the grid — see `zest_input::ime` for why that
    /// matters more here than in an ordinary text field.
    fn on_ime(&mut self, ime: winit::event::Ime) {
        use winit::event::Ime;

        match ime {
            Ime::Enabled => {
                self.ime.enable();
                // Placed on enable as well as on every preedit: winit's own docs
                // say to start issuing area requests here, and some backends ask
                // for the area before they send any composing text.
                self.place_candidate_window();
            }
            Ime::Disabled => self.ime.disable(),
            Ime::Preedit(text, cursor) => {
                self.ime.set_preedit(text, cursor);
                // Where the candidate list should appear. Sent on every preedit
                // change because the composing text grows, and a candidate
                // window anchored to where the composition *started* ends up
                // covering what is being typed.
                self.place_candidate_window();
            }
            Ime::Commit(text) => {
                self.ime.commit();
                if text.is_empty() {
                    // Nothing composed; nothing to route.
                } else if self.picker.is_none()
                    && self.palette_ui.is_none()
                    && self.settings_tab_active()
                    && self.screen == AppScreen::Terminal
                {
                    // The same gate — and the same precedence — as the
                    // KeyboardInput handler: the Settings tab holds the
                    // keyboard, so composed text must go where keystrokes
                    // go, never to the concealed session's shell. The
                    // picker and palette are checked first because the key
                    // path hands them the keys before the tab.
                    if let Some(ui) = self.settings_ui.as_mut() {
                        // A composed family name belongs to the open
                        // dropdown's search row when there is one — the key
                        // path's ordering, which `commit_text` mirrors.
                        ui.commit_text(&text);
                    }
                    self.mark_chrome_dirty();
                } else if self.picker.is_none()
                    && self.palette_ui.is_none()
                    && self.profiles_tab_active()
                {
                    // The Profiles editor holds the keyboard the same way —
                    // a composed word is a filter or a buffer, never bytes
                    // for a shell nobody can see.
                    if let Some(ui) = self.profiles_ui.as_mut() {
                        ui.commit_text(&text);
                    }
                    self.mark_chrome_dirty();
                } else if let Some(session) = self.tabs.active_source() {
                    // Straight through as UTF-8, exactly as a physical
                    // keyboard would have delivered it. Not `encode_paste`:
                    // this is typing, and bracketing it would make a program
                    // that reads paste-mode treat a composed word as pasted.
                    let policy = self.predict_policy();
                    for c in text.chars() {
                        session.predict(zest_proto::Key::Printable(c), policy);
                    }
                    session.write(text.into_bytes());
                    let mut term = session.terminal().lock();
                    // Same gate as a physical keystroke: the comment above says
                    // this *is* typing, so it has to honour the typing setting
                    // or an input method is the one way to be yanked back.
                    if self.config.scroll_on_keypress {
                        term.scroll_to_bottom();
                    }
                    term.set_selection(None);
                }
            }
        }
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// A key coming back up.
    ///
    /// Silent for every program that has not turned on kitty event types, which
    /// is almost all of them — the encoder returns `None` and nothing is
    /// written. Deliberately does *not* scroll to the bottom or clear the
    /// selection the way a press does: releasing a key is not a second thing
    /// the user did, and a selection that vanished when a finger came off
    /// `Shift` would be its own bug.
    fn on_key_release(&mut self, event: &winit::event::KeyEvent) {
        // A composition owns the keyboard, including the releases.
        if self.ime.composing() || self.picker.is_some() || self.screen != AppScreen::Terminal {
            return;
        }
        let Some(session) = self.tabs.active_source() else { return };
        let modes = session.terminal().lock().modes();
        if let Some(bytes) = key::encode(event, self.modifiers, modes) {
            session.write(bytes);
        }
    }

    /// Tell the platform where the composing text is, so the candidate list
    /// appears under it rather than at the window's corner.
    fn place_candidate_window(&self) {
        let (Some(window), Some(fonts), Some(session)) =
            (self.window.as_ref(), self.fonts.as_ref(), self.tabs.active_source())
        else {
            return;
        };
        let m = fonts.cell_metrics();
        let (row, col) = {
            let term = session.terminal().lock();
            let c = term.grid().cursor;
            (c.row, c.col)
        };
        let insets = self.insets();
        let x = f64::from(insets.left) + f64::from(m.cell_w) * col as f64;
        let y = f64::from(insets.top) + f64::from(m.cell_h) * row as f64;
        // The *area* the composition occupies, not a point: macOS places the
        // candidate window below it, and a zero-width area puts the list over
        // the text it is meant to be helping with.
        let w = f64::from(m.cell_w) * self.ime.width_cells().max(1) as f64;
        window.set_ime_cursor_area(
            winit::dpi::PhysicalPosition::new(x, y),
            winit::dpi::PhysicalSize::new(w, f64::from(m.cell_h)),
        );
    }

    /// Send a mouse event to the program, if it wants one.
    ///
    /// Returns true when the event was consumed, so the caller skips selection.
    fn forward_mouse(
        &self,
        button: MouseButton,
        state: ElementState,
        row: usize,
        col: usize,
    ) -> bool {
        // Shift always means "I want to select", overriding the program.
        if self.modifiers.shift_key() {
            return false;
        }
        let Some(session) = self.tabs.active_source() else { return false };

        let encoded = {
            let term = session.terminal().lock();
            let modes = term.modes();
            if !modes.mouse_enabled() {
                return false;
            }
            let b = match button {
                MouseButton::Left => mouse::MouseButton::Left,
                MouseButton::Middle => mouse::MouseButton::Middle,
                MouseButton::Right => mouse::MouseButton::Right,
                _ => return false,
            };
            let action = match state {
                ElementState::Pressed => mouse::MouseAction::Press,
                ElementState::Released => mouse::MouseAction::Release,
            };
            mouse::encode_mouse(b, action, row, col, self.modifiers, modes)
        };

        match encoded {
            Some(bytes) => {
                session.write(bytes);
                true
            }
            // The program has mouse reporting on but does not want this
            // particular event. It still owns the mouse, so do not select.
            None => true,
        }
    }

    /// Send pointer movement to the program, if it wants it.
    ///
    /// Whether this is a drag or bare motion depends on whether a button is
    /// down, and the two are gated on different modes (1002 vs 1003).
    fn forward_motion(&self, row: usize, col: usize) -> bool {
        if self.modifiers.shift_key() {
            return false;
        }
        let Some(session) = self.tabs.active_source() else { return false };

        let encoded = {
            let modes = session.terminal().lock().modes();
            if !modes.mouse_enabled() {
                return false;
            }
            let action = if self.mouse.is_dragging() {
                mouse::MouseAction::Drag
            } else {
                mouse::MouseAction::Motion
            };
            mouse::encode_mouse(
                mouse::MouseButton::Left,
                action,
                row,
                col,
                self.modifiers,
                modes,
            )
        };

        if let Some(bytes) = encoded {
            session.write(bytes);
        }
        // The program owns the mouse either way; it simply may not have asked
        // for this particular event.
        true
    }

    /// Send a wheel event to the program, if it wants one.
    fn forward_wheel(&self, up: bool, count: usize) -> bool {
        if self.modifiers.shift_key() {
            return false;
        }
        let Some(session) = self.tabs.active_source() else { return false };
        let (row, col) = self.pointer_cell;

        let modes = session.terminal().lock().modes();
        if !modes.mouse_enabled() {
            return false;
        }

        let button = if up { mouse::MouseButton::WheelUp } else { mouse::MouseButton::WheelDown };
        let mut out = Vec::new();
        for _ in 0..count.max(1) {
            if let Some(b) =
                mouse::encode_mouse(button, mouse::MouseAction::Press, row, col, self.modifiers, modes)
            {
                out.extend_from_slice(&b);
            }
        }
        if !out.is_empty() {
            session.write(out);
        }
        true
    }

    /// The chrome's current claim on the window edges.
    ///
    /// Recomputed on demand rather than cached: it is a handful of multiplies,
    /// and a cached copy is one more thing that can disagree with the settings
    /// and the scale factor after a reload or a monitor change.
    fn insets(&self) -> Insets {
        let scale = self.window.as_ref().map_or(1.0, |w| w.scale_factor() as f32);
        self.insets_at(scale)
    }

    /// [`Self::insets`] for callers that know the scale before the window is
    /// stored — the shell-spawn path in `resumed` sizes the grid this way.
    fn insets_at(&self, scale: f32) -> Insets {
        let strip = self.strip_shown().then_some(crate::chrome::insets::StripClaim {
            position: self.config.tabs.position,
            strip_height: self.config.tabs.strip_height,
            sidebar_width: self.config.tabs.sidebar_width,
        });
        Insets::resolved(self.config.padding, scale, strip)
    }

    /// Whether the strip is drawn at all.
    ///
    /// `custom_chrome` forces it, and that is load-bearing rather than a
    /// preference: with `show_single_tab` off and one tab open, a borderless
    /// window would have no titlebar, no caption buttons and nothing to drag —
    /// an undecorated rectangle with no way to move, maximize or close it.
    fn strip_shown(&self) -> bool {
        self.config.chrome.draws_caption()
            || self.config.tabs.show_single_tab
            || self.tabs.len() > 1
    }

    pub(crate) fn mark_chrome_dirty(&mut self) {
        self.chrome_dirty = true;
        self.chrome_layout = None;
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Re-run the chrome layout if anything invalidated it.
    ///
    /// Called from both the input path and the redraw, so a click is always
    /// tested against the same rectangles the frame drew — the "no drift"
    /// property the layout tests pin, extended to runtime.
    fn refresh_chrome(&mut self) {
        if !self.strip_shown()
            && self.picker.is_none()
            && self.palette_ui.is_none()
            && self.dir_picker.is_none()
            && self.launcher.is_none()
            // Load-bearing: with one tab and no custom chrome `strip_shown` is
            // false, so without this the block menu lives in a layout that is
            // thrown away before it is ever built — the menu opens and nothing
            // appears, with nothing to see in any log.
            && self.block_menu.is_none()
            && self.open_file.is_none()
            // The same trap again (#519): the find bar is chrome, and with the
            // strip hidden it would be laid out into a cache thrown away before
            // it is drawn — the bar opens, nothing appears, nothing logs it.
            && self.find.is_none()
            // The same trap as the block menu, one surface along: a file pane
            // *is* chrome, so with the strip hidden its whole body would be
            // laid out and thrown away, and the pane would show nothing with
            // nothing in a log to say why.
            && !self.tabs.active().is_some_and(crate::tabs::Tab::has_editor_pane)
            && !self.tabs.settings_open()
            && self.screen == AppScreen::Terminal
        {
            self.chrome_layout = None;
            return;
        }
        // The layout cache and the frame-pending latch are separate on
        // purpose: the input path refreshes the layout between frames, and
        // must not eat the redraw the change asked for.
        if self.chrome_layout.is_some() {
            return;
        }
        // Built before the font borrow below: row construction reads the
        // fleet and the tabs, never the fonts.
        let anim = self.anim_phase();
        let caret_on = anim.caret_on;
        let early_geometry = self.window.as_ref().map(|w| {
            let scale = w.scale_factor() as f32;
            (scale, w.inner_size())
        });
        let picker_rows = self.picker.is_some().then(|| self.build_picker());
        let picker_model = picker_rows.map(|(rows, actions, hosts_searched)| {
            let state = self.picker.as_mut().expect("is_some gated the build");
            state.actions = actions;
            state.selected = state.selected.min(rows.len().saturating_sub(1));
            // A filter edit can strand the selection on a group label; land
            // it on the nearest row Enter can actually run.
            if matches!(state.actions.get(state.selected), Some(PickerAction::None) | None) {
                if let Some(first) = state
                    .actions
                    .iter()
                    .position(|a| !matches!(a, PickerAction::None))
                {
                    state.selected = first;
                }
            }
            crate::chrome::model::PickerModel {
                rows,
                selected: state.selected,
                filter: state.filter.text().to_string(),
                filter_caret: caret_of(&state.filter),
                scroll: state.scroll,
                ensure_visible: state.scroll_to_selected,
                hosts_searched,
                caret_on,
            }
        });

        // The launcher's rows, rebuilt per pass like the picker's: profiles
        // can change under an open menu via the config watcher, and rows
        // and actions must come from one pass or a click runs the wrong row.
        let launcher_rows = self.launcher.is_some().then(|| {
            let fallback = self.shell_fallback();
            let active_profile = self
                .tabs
                .active()
                .and_then(|t| t.identity.as_ref())
                .map(|i| i.name.clone());
            crate::launcher::build_rows(
                &self.settings,
                &self.fleet_view,
                &fallback,
                active_profile.as_deref(),
                keymap::chord_for(keymap::Action::OpenProfiles),
            )
        });
        let launcher_model = launcher_rows.map(|(rows, actions)| {
            let state = self.launcher.as_mut().expect("is_some gated the build");
            state.actions = actions;
            // A reload can shrink the rows under the selection; land it on
            // the nearest actionable row rather than off the end or on the
            // divider.
            state.selected = state.selected.min(rows.len().saturating_sub(1));
            if matches!(
                state.actions.get(state.selected),
                Some(crate::launcher::LauncherAction::None) | None
            ) {
                state.selected = crate::launcher::step(&state.actions, state.selected, true);
            }
            crate::chrome::model::LauncherModel {
                rows,
                selected: state.selected,
                anchor: state.anchor,
            }
        });

        // Rebuilt every pass, like the launcher's: output can arrive under an
        // open menu and turn "Copy output" from faint to live — or, on the
        // chip menu, a command finishing turns the cd rows live.
        let block_menu_rows = self.block_menu.as_ref().and_then(|m| {
            let tab = self.tabs.active()?;
            let folded = self
                .folded_blocks
                .get(&tab.focused_addr())
                .is_some_and(|f| f.contains(&m.block));
            let term = tab.focused_session()?.terminal();
            let term = term.lock();
            let block = term.blocks().get(zest_core::BlockId(m.block))?.clone();
            Some(crate::block_menu::build_rows(
                &term,
                &block,
                folded,
                &keymap::chord_for(keymap::Action::CopyBlockOutput),
                &keymap::chord_for(keymap::Action::RerunLastCommand),
            ))
        });
        let block_menu_model = block_menu_rows.map(|(rows, actions)| {
            let state = self.block_menu.as_mut().expect("is_some gated the build");
            state.actions = actions;
            // Output arriving can enable a row, and a fold can flip one; land
            // the selection on the nearest live row rather than off the end or
            // on something drawn faint.
            state.selected = state.selected.min(rows.len().saturating_sub(1));
            if !crate::block_menu::is_actionable(&state.actions, state.selected) {
                state.selected = crate::block_menu::first_actionable(&state.actions);
            }
            crate::chrome::model::BlockMenuModel {
                rows,
                selected: state.selected,
                anchor: state.anchor,
            }
        });

        // Before the `&mut self` borrow below, since it reads the tab.
        let open_file_model = self.open_file_model();
        let find_model = self.find_model();
        let dir_picker_model = self.dir_picker.as_mut().map(|state| {
            // Rows and their answers in one pass: `..` first when there is a
            // parent, then the children the filter keeps. The parallel
            // `rows` list is what Enter and a click act on, so the two are
            // built together and cannot drift.
            let filter = state.filter.text().to_lowercase();
            let mut rows = Vec::new();
            let mut answers: Vec<Option<String>> = Vec::new();
            if state.parent.is_some() {
                rows.push("\u{2191}  .. (parent directory)".to_string());
                answers.push(None);
            }
            let base = std::path::Path::new(&state.path);
            for name in &state.dirs {
                if !filter.is_empty() && !name.to_lowercase().contains(&filter) {
                    continue;
                }
                rows.push(name.clone());
                answers.push(Some(base.join(name).to_string_lossy().into_owned()));
            }
            state.selected = state.selected.min(rows.len().saturating_sub(1));
            state.rows = answers;
            crate::chrome::model::DirPickerModel {
                rows,
                has_parent: state.parent.is_some(),
                selected: state.selected,
                filter: state.filter.text().to_string(),
                filter_caret: caret_of(&state.filter),
                scroll: state.scroll,
                ensure_visible: state.scroll_to_selected,
                loading: state.loading,
                error: state.error.clone(),
                truncated: state.truncated,
            }
        });

        let palette_model = self.palette_ui.as_mut().map(|state| {
            let (rows, actions) = keymap::palette(state.filter.text());
            state.actions = actions;
            // A filter edit can strand the selection on a header, a
            // reference row, or past the end; land it on the nearest
            // runnable command instead.
            state.selected = keymap::nearest_runnable(&state.actions, state.selected);
            crate::chrome::model::PaletteModel {
                rows,
                selected: state.selected,
                filter: state.filter.text().to_string(),
                filter_caret: caret_of(&state.filter),
                scroll: state.scroll,
                ensure_visible: state.scroll_to_selected,
            }
        });

        // The Settings tab's screen, built only while it holds the grid area
        // (its state persists while it is a background chip). Inputs gathered
        // before the &mut borrow of the tab state below; the clone is a
        // handful of provenance entries, on an event-driven rebuild.
        // Not built while a full-pane screen covers it: the screen's opaque
        // ground would shadow every region positionally anyway, but
        // `settings_tracks` is keyed by row index, not position — the
        // Profiles screen shares the widget vocabulary, and two panes'
        // tracks under one key would send a slider drag to the wrong file.
        let settings_inputs = (self.settings_tab_active()
            && self.screen == AppScreen::Terminal)
            .then(|| {
            (
                serde_json::to_value(&self.settings).unwrap_or(serde_json::Value::Null),
                self.provenance.clone(),
                self.shared.restart_pending.borrow().clone(),
                self.settings_error.clone(),
                self.unknown_keys.clone(),
                // The rail's visible categories — computed outside the &mut
                // borrow, by the same helper the click handler resolves
                // `SettingsCategory(i)` with, so a click can never land on
                // a different list than was drawn.
                self.visible_categories(),
            )
        });
        // Taken before the &mut borrow below, and taken *once*: opening the
        // menu again on every rebuild would make it impossible to dismiss.
        let start_menu_key = self.start_menu_key.take();
        // Read before the closure: it borrows `self.settings_ui` mutably, and
        // the capability answer is about the surface, not the form.
        let caps = self.capabilities();
        let settings_model = self.settings_ui.as_mut().zip(settings_inputs).map(
            |(ui, (values, provenance, restart_pending, error, unknown_keys, visible_cats))| {
                use crate::settings_ui as sui;
                // The footer counts every category; the rail badges only the
                // visible ones (§11: the filter hides empty categories).
                let (_, total) =
                    sui::modified_counts(&ui.fields, &values, &sui::categories(&ui.fields));
                let (counts, _) = sui::modified_counts(&ui.fields, &values, &visible_cats);
                let visible: Vec<(String, usize)> =
                    visible_cats.into_iter().zip(counts).collect();
                if !visible.iter().any(|(g, _)| *g == ui.category) {
                    if let Some((first, _)) = visible.first() {
                        ui.category = first.clone();
                        ui.selected = 0;
                        ui.scroll = 0.0;
                    }
                }
                let selected_category = visible
                    .iter()
                    .position(|(g, _)| *g == ui.category)
                    .unwrap_or(0);

                let (rows, actions, empty) = if ui.category == sui::UNKNOWN_CATEGORY {
                    let (mut rows, mut actions) = sui::build_unknown_rows(
                        &unknown_keys,
                        &provenance,
                        ui.filter.text(),
                        &zest_config::schema::keys(),
                    );
                    let empty = rows.is_empty().then(|| {
                        if unknown_keys.is_empty() {
                            "Every key in your files is a setting this build knows.".to_string()
                        } else {
                            format!("nothing matches \u{201c}{}\u{201d}", ui.filter.text())
                        }
                    });
                    if !rows.is_empty() {
                        // The §11 warn banner: a key from a newer version is
                        // indistinguishable from a typo, so these warn
                        // rather than fail — and the rest of the file
                        // applied normally.
                        let n = rows.len();
                        let text = format!(
                            "{n} key{} in your config {} not settings. Kept rather than \
                             discarded, and warned about rather than failed on: a key from \
                             a newer version is indistinguishable from a typo. The rest of \
                             the file applied normally.",
                            if n == 1 { "" } else { "s" },
                            if n == 1 { "is" } else { "are" },
                        );
                        rows.insert(0, crate::chrome::model::SettingsRowModel::Notice { text });
                        actions.insert(0, sui::RowAction::None);
                    }
                    (rows, actions, empty)
                } else {
                    let (rows, actions) = sui::build_category_rows(
                        &ui.fields,
                        &values,
                        &provenance,
                        &ui.category,
                        ui.filter.text(),
                        ui.editing.as_ref(),
                        &restart_pending,
                        error.as_deref(),
                        &ui.installed,
                        caps,
                    );
                    let empty = rows
                        .is_empty()
                        .then(|| format!("nothing matches \u{201c}{}\u{201d}", ui.filter.text()));
                    (rows, actions, empty)
                };
                ui.actions = actions;
                // A filter edit can strand the selection on a banner or past
                // the end; land it on the nearest real row instead.
                ui.selected = sui::nearest_field(&ui.actions, ui.selected);

                // `--screen settings-menu`, now that the rows exist. Opened
                // through the same state Enter arms, so the flag can never
                // show something a user could not have reached.
                if let Some(key) = start_menu_key {
                    if let Some(row) = ui.actions.iter().position(|a| {
                        matches!(a, sui::RowAction::Field(i)
                            if ui.fields.get(*i).is_some_and(|f| f.key == key))
                    }) {
                        ui.selected = row;
                        let roster: Vec<String> = crate::themes::ids();
                        let current = zest_config::ui::value_at(&values, &key)
                            .and_then(serde_json::Value::as_str);
                        ui.menu = Some(MenuState::roster(row, roster, current));
                    }
                }

                let menu = ui.menu.as_ref().and_then(|menu| {
                    let field_idx = match ui.actions.get(menu.row) {
                        Some(sui::RowAction::Field(i)) => *i,
                        _ => return None,
                    };
                    let field = ui.fields.get(field_idx)?;
                    let value = zest_config::ui::value_at(&values, &field.key);
                    // The font list's value is an array; its first entry is
                    // the face the atlas actually shapes with, so that is the
                    // one the ✓ belongs on.
                    let current = match value {
                        Some(serde_json::Value::Array(a)) => {
                            a.first().and_then(serde_json::Value::as_str)
                        }
                        other => other.and_then(serde_json::Value::as_str),
                    };
                    let footer = (field.widget == zest_config::ui::Widget::ThemePicker)
                        .then(|| BROWSE_THEMES.to_string());
                    menu_model(menu, &ui.actions, &ui.fields, current, footer)
                });

                let config_path = zest_config::paths::config_file()
                    .or_else(|| {
                        zest_config::paths::config_dir()
                            .map(|d| d.join(zest_config::paths::CONFIG_FILE))
                    })
                    .map(|p| crate::status::shorten_home(&p.display().to_string()))
                    .unwrap_or_default();

                crate::chrome::model::SettingsScreenModel {
                    categories: visible
                        .into_iter()
                        .map(|(label, modified)| {
                            crate::chrome::model::SettingsCategoryModel { label, modified }
                        })
                        .collect(),
                    selected_category,
                    heading: ui.category.clone(),
                    prefix: if ui.category == sui::UNKNOWN_CATEGORY {
                        "\u{2014}".to_string()
                    } else {
                        sui::category_prefix(&ui.fields, &ui.category)
                    },
                    lede: sui::category_lede(&ui.category).to_string(),
                    rows,
                    empty,
                    selected: ui.selected,
                    filter: ui.filter.text().to_string(),
                    filter_caret: caret_of(&ui.filter),
                    scroll: ui.scroll,
                    ensure_visible: ui.scroll_to_selected,
                    modified_total: total,
                    config_path,
                    menu,
                }
            },
        );

        // The Profiles editor's screen (§12), built only while its pane
        // holds the grid area — the Settings tab's exact discipline: inputs
        // gathered before the &mut borrow of the tab state.
        let profiles_inputs = self.profiles_tab_active().then(|| {
            let hosts: Vec<(String, bool, bool)> = self.shared.fleet.get()
                .map(|f| {
                    f.snapshot()
                        .into_iter()
                        .map(|h| {
                            let online = h.is_online();
                            (h.label, online, h.local)
                        })
                        .collect()
                })
                .unwrap_or_default();
            let local_host = hosts
                .iter()
                .find(|(_, _, local)| *local)
                .map_or_else(|| "this machine".to_string(), |(label, ..)| label.clone());
            (
                serde_json::to_value(&self.settings).unwrap_or(serde_json::Value::Null),
                self.settings.clone(),
                self.shell_fallback(),
                local_host,
                hosts.into_iter().map(|(label, online, _)| (label, online)).collect::<Vec<_>>(),
                self.effective_theme().to_string(),
            )
        });
        let profiles_model = self.profiles_ui.as_mut().zip(profiles_inputs).map(
            |(ui, (window_values, settings, fallback_command, local_host, hosts, window_theme))| {
                use crate::profiles_ui as pui;
                use crate::settings_ui as sui;
                // A profile deleted or renamed under the editor falls back
                // to Defaults rather than editing a ghost.
                let names = pui::rail_names(&settings);
                if !names.contains(&ui.profile) {
                    ui.profile = zest_config::profiles::RESERVED_PROFILE.to_string();
                    ui.selected = 0;
                    ui.scroll = 0.0;
                    // The buffer was typed for a profile that no longer
                    // exists, and `editing` is keyed by field index alone —
                    // left alive, the next Enter would write it into
                    // `[profiles.defaults]`, the parent every other profile
                    // inherits from. Losing the keystrokes beats writing them
                    // somewhere nobody asked for (#272).
                    ui.editing = None;
                    ui.renaming = None;
                    ui.rename_error = None;
                }
                let selected_rail = names.iter().position(|n| *n == ui.profile).unwrap_or(0);
                let is_defaults = selected_rail == 0;

                let root = crate::launcher::profiles_root(&settings);
                let resolved = zest_config::profiles::resolve_profile(&root, &ui.profile);
                let schemes = pui::scheme_swatches();
                let ctx = pui::ProfileRowContext {
                    window_values: &window_values,
                    window_theme: &window_theme,
                    fallback_command: &fallback_command,
                    local_host: &local_host,
                    hosts: &hosts,
                    schemes: &schemes,
                    is_defaults,
                };
                let (rows, chips, actions) = pui::build_profile_rows(
                    &ui.fields,
                    &resolved,
                    &ctx,
                    ui.filter.text(),
                    ui.editing.as_ref(),
                    ui.error.as_deref(),
                );
                ui.actions = actions;
                // A filter edit can strand the selection on a section rule
                // or past the end; land it on the nearest real row.
                ui.selected = sui::nearest_field(&ui.actions, ui.selected);

                // The open dropdown, resolved same-pass against the row —
                // window.backdrop's variants, and §12's theme and font
                // rosters, through the Settings tab's own builder.
                let overrides = pui::overrides_json(&resolved);
                let menu = ui.menu.as_ref().and_then(|menu| {
                    let field_idx = match ui.actions.get(menu.row) {
                        Some(sui::RowAction::Field(i)) => *i,
                        _ => return None,
                    };
                    let field = ui.fields.get(field_idx)?;
                    let value = pui::effective_value(field, &resolved, &overrides, &ctx);
                    let current = match &value {
                        serde_json::Value::Array(a) => {
                            a.first().and_then(serde_json::Value::as_str)
                        }
                        other => other.as_str(),
                    };
                    // The ✓ is matched against the *drawn* option, and the host
                    // field's on-disk spelling is not one: "no pin" is an empty
                    // string where the menu shows "(this machine)", and a pin
                    // written `Forge` is the row spelled `forge`. Without this
                    // fold the row every profile lands on is the one row that
                    // never shows a tick.
                    let folded;
                    let current = if field.widget == zest_config::ui::Widget::HostPicker {
                        folded = host_menu_selection(&menu.roster, current);
                        Some(folded.as_str())
                    } else {
                        current
                    };
                    let footer = (field.widget == zest_config::ui::Widget::ThemePicker)
                        .then(|| BROWSE_THEMES.to_string());
                    menu_model(menu, &ui.actions, &ui.fields, current, footer)
                });

                let display_name =
                    if is_defaults { "Defaults".to_string() } else { ui.profile.clone() };
                // Static, per the §12 caption's spirit: the real per-host
                // uname needs the control plane and belongs to the
                // cross-host launch item, not to a preview.
                let os_line =
                    format!("{} {}", std::env::consts::OS, std::env::consts::ARCH);
                let preview =
                    pui::build_preview(&display_name, &resolved, &window_theme, &os_line);
                let count = pui::override_count(&ui.fields, &resolved);
                let empty = rows
                    .is_empty()
                    .then(|| format!("nothing matches \u{201c}{}\u{201d}", ui.filter.text()));

                let renaming = ui.renaming.as_ref().map(|buffer| {
                    crate::chrome::model::ProfileNameEdit {
                        buffer: buffer.text().to_string(),
                        caret: buffer.caret(),
                        selection: buffer.selection(),
                        error: ui.rename_error.clone(),
                    }
                });
                Box::new(crate::chrome::model::ProfilesScreenModel {
                    renaming,
                    rail: pui::build_rail(&settings, &fallback_command, &local_host),
                    selected_rail,
                    name: display_name,
                    command: resolved
                        .meta
                        .command
                        .clone()
                        .unwrap_or_else(|| fallback_command.clone()),
                    host_chip: resolved.meta.host.clone(),
                    icon: resolved.meta.icon.clone(),
                    accent: pui::accent_of(&resolved.meta),
                    can_delete: !is_defaults,
                    preview,
                    rows,
                    chips,
                    selected: ui.selected,
                    filter: ui.filter.text().to_string(),
                    filter_caret: caret_of(&ui.filter),
                    scroll: ui.scroll,
                    ensure_visible: ui.scroll_to_selected,
                    empty,
                    footer_sentence: pui::footer_sentence(is_defaults, count),
                    table_name: format!("[profiles.{}]", ui.profile),
                    menu,
                })
            },
        );

        // Built before the font borrow below: these read tabs, fleet and the
        // filesystem, never the fonts.
        let fleet_hosts = self.shared.fleet.get().map(|f| f.snapshot()).unwrap_or_default();
        // The enrolled machine's account machinery, started on the daemon's
        // word instead of waiting for someone to open the Fleet screen —
        // see `should_start_account_watch`. Cheap per rebuild: a bool and a
        // scan of a handful of rows, and `account_poke` closes the gate the
        // moment the watch is up.
        if self.should_start_account_watch(&fleet_hosts) {
            if self.account == AccountState::Unknown {
                self.probe_account();
            }
            self.start_account_watch();
        }
        // Retained beside the model it feeds: the fleet screen's hit map
        // carries card indices, and they must resolve against the snapshot
        // the cards were built from, not a fresher one.
        self.fleet_view = fleet_hosts.clone();
        self.devices_view = self.shared.fleet.get().map(|f| f.devices()).unwrap_or_default();
        // Same retention rule for the theme gallery: card index i must mean
        // the same theme at click time that it meant at draw time.
        self.themes_view = crate::themes::ids();
        let screen_model = profiles_model
            .map(crate::chrome::model::ScreenModel::Profiles)
            .or_else(|| self.build_screen_model(&fleet_hosts));
        let panes = self.build_panes_model(&fleet_hosts);
        let grid_area = early_geometry.map_or([0.0; 4], |(scale, size)| {
            self.insets_at(scale).grid_rect(size.width, size.height)
        });

        // Before the font borrow below, like everything else the model reads.
        let notice = self.pairing_notice();
        let approval = self.approval_model();

        let Some(window) = self.window.as_ref() else { return };
        let Some(fonts) = self.fonts.as_mut() else { return };

        let scale = window.scale_factor() as f32;
        let size = window.inner_size();
        let cm = fonts.cell_metrics();
        let metrics = ChromeMetrics {
            width: size.width as f32,
            height: size.height as f32,
            scale,
            strip_height: self.config.tabs.strip_height as f32,
            sidebar_width: self.config.tabs.sidebar_width as f32,
            line_height: cm.cell_h as f32,
            baseline: cm.baseline as f32,
            font_px: fonts.shaping_px(),
            cell_w: cm.cell_w as f32,
            padding: self.config.padding,
        };
        // In fullscreen the traffic lights auto-hide, so the strip reclaims
        // their reserve; everywhere else the answer comes from AppKit fresh,
        // because the inset is not a constant.
        //
        // Fullscreen also takes the caption buttons and the resize edges: the
        // OS owns the frame there, and drawing our own close button over a
        // fullscreen window would be offering to do something the window is
        // not currently able to do.
        let fullscreen = window.fullscreen().is_some();
        let controls = WindowControls {
            native_leading: (!fullscreen)
                .then(|| platform::native_control_inset(window))
                .flatten()
                .map(|(x, y)| [x as f32 * scale, y as f32 * scale]),
            drawn_caption: self.config.chrome.draws_caption() && !fullscreen,
            maximized: window.is_maximized(),
            // A maximized window that resized from its edge would un-maximize
            // under the pointer, which is not what the drag meant.
            resizable_edges: self.config.chrome.draws_caption()
                && !fullscreen
                && !window.is_maximized(),
        };

        let local_label = fleet_hosts
            .iter()
            .find(|h| h.local)
            .map_or_else(|| "local".to_string(), |h| h.label.clone());

        // Host accent slots: the local machine is always slot 0; remote hosts
        // take the next slots in first-seen strip order, so a host keeps its
        // colour for the life of the window.
        let mut remote_slots = HostSlots::new();

        let tab_models: Vec<TabModel> = self
            .tabs
            .iter()
            .map(|tab| {
                // A brief lock per tab per chrome rebuild — rebuilds are
                // event-driven, so this is microseconds, not a frame cost.
                let (title, cwd, running, progress) = {
                    let term = tab.source().terminal();
                    let term = term.lock();
                    let title = crate::chrome::model::terminal_label(&term);
                    // A remote terminal's cwd never crosses the wire directly;
                    // its blocks do, and each carries the cwd it ran in.
                    let cwd = if term.cwd().is_empty() {
                        term.blocks().last().map(|b| b.cwd.clone()).unwrap_or_default()
                    } else {
                        term.cwd().to_string()
                    };
                    let running = term.blocks().last().is_some_and(|b| b.is_running());
                    (title, cwd, running, term.progress())
                };
                let origin = match tab.source().origin() {
                    Origin::Daemon { host, local: false } => {
                        // The id is the tab's own address's — all-zero while a
                        // launch is still connecting, which is exactly what
                        // the variant's placeholder fallback is for (#304).
                        TabOrigin::Remote { host: tab.addr.host, label: host }
                    }
                    _ => TabOrigin::Local,
                };
                let (host_label, accent, cwd) = match &origin {
                    TabOrigin::Remote { label, .. } => {
                        let slot = remote_slots.slot(tab.addr, label);
                        (label.clone(), slot + 1, cwd)
                    }
                    TabOrigin::Local => {
                        (local_label.clone(), 0, crate::status::shorten_home(&cwd))
                    }
                };
                let age = self
                    .activity
                    .lock()
                    .get(&tab.addr)
                    .map(|t| crate::status::age_label(t.elapsed()))
                    .unwrap_or_default();
                // How this tab's host is reached — the fact the chip's glyph
                // tile inks when it degrades (the status bar's old job). A
                // dropped daemon link outranks whatever path the host
                // normally takes.
                let link = if self.link_down {
                    crate::chrome::model::LinkKind::Reconnecting
                } else if matches!(origin, TabOrigin::Local) {
                    crate::chrome::model::LinkKind::Loopback
                } else {
                    // The same lookup `presence` uses. These were two, matching
                    // differently, so with duplicate labels a tab could report
                    // one machine's presence beside another's route (#297).
                    let host = fleet_host_of(&origin, &fleet_hosts);
                    match host.and_then(|h| h.reachability) {
                        Some(zest_mesh::Reachability::Cloud) => {
                            crate::chrome::model::LinkKind::Tunnel
                        }
                        Some(zest_mesh::Reachability::Loopback) => {
                            crate::chrome::model::LinkKind::Loopback
                        }
                        _ => crate::chrome::model::LinkKind::Lan,
                    }
                };
                TabModel {
                    addr: tab.addr,
                    kind: crate::chrome::model::TabKind::Session,
                    title: if tab.dead { format!("{title} · ended") } else { title },
                    host: host_label,
                    cwd,
                    // The fleet's word about this tab's machine (#297). Three
                    // of `TabPresence`'s four variants had never been produced
                    // for a tab, so a session on a machine whose port had
                    // stopped answering read exactly like one on a healthy
                    // machine — the chip's "· unreachable" was drawn by code
                    // nothing could reach.
                    presence: tab_presence(&origin, &fleet_hosts),
                    origin,
                    accent,
                    tab_accent: crate::chrome::model::tab_accent(tab.identity.as_ref(), accent),
                    running,
                    progress,
                    attention: self.attention.get(&tab.addr).copied(),
                    age,
                    // Dead tabs borrow the connecting style (faint text): not
                    // live, not interactive, still present. A launching tab
                    // wears it for real (issue #175).
                    connecting: tab.dead || tab.connecting,
                    link,
                    opacity: tab.identity.as_ref().and_then(|i| i.opacity),
                }
            })
            .collect();

        // App tabs after the session tabs, in §1's order: sessions, then
        // Profiles, then Settings, then the `+`. One list, so the strip, the
        // sidebar rows and the hit map all agree what exists — including in
        // the vertical position, which Profiles used to be gated out of
        // entirely: ⌘⇧, opened a pane the sidebar could neither show nor
        // close (#494).
        let mut tab_models = tab_models;
        if self.tabs.profiles_open() {
            tab_models.push(TabModel {
                addr: crate::tabs::profiles_tab_addr(),
                kind: crate::chrome::model::TabKind::Profiles,
                title: "Profiles".into(),
                // Empty, like Settings': an app tab is a place, not a shell
                // on a host, and the vertical header draws its host pill off
                // exactly this field. The local label sat here harmlessly
                // while Profiles was horizontal-only — the horizontal strip
                // has no header — so making the tab appear in both positions
                // is what turned it into a visible "Profiles · local".
                host: String::new(),
                cwd: String::new(),
                origin: TabOrigin::Local,
                presence: TabPresence::Online,
                accent: 0,
                // Accent index 0 is the theme's own accent: an app tab is a
                // place, not a shell on a host.
                tab_accent: crate::chrome::model::AccentChoice::Profile(0),
                running: false,
                progress: zest_core::Progress::None,
                attention: None,
                age: String::new(),
                connecting: false,
                link: crate::chrome::model::LinkKind::Loopback,
                // An app tab is a place, not a shell on a host: no pane, so
                // nothing to match.
                opacity: None,
            });
        }
        if self.tabs.settings_open() {
            tab_models.push(TabModel {
                addr: crate::tabs::settings_addr(),
                kind: crate::chrome::model::TabKind::Settings,
                title: "Settings".to_string(),
                host: String::new(),
                cwd: String::new(),
                origin: TabOrigin::Local,
                presence: TabPresence::Online,
                accent: 0,
                tab_accent: crate::chrome::model::tab_accent(None, 0),
                running: false,
                progress: zest_core::Progress::None,
                attention: None,
                age: String::new(),
                connecting: false,
                link: crate::chrome::model::LinkKind::Loopback,
                // An app tab is a place, not a shell on a host: no pane, so
                // nothing to match.
                opacity: None,
            });
        }

        // The sidebar's host grouping, built from the same tab models the
        // strip draws — one pass, one truth. App tabs are places with no
        // host; the vertical layout pins them above the footer instead.
        let sidebar = (self.config.tabs.position == zest_config::settings::TabsPosition::Left)
            .then(|| {
                let mut groups: Vec<crate::chrome::model::HostGroup> = Vec::new();
                for (i, tm) in tab_models.iter().enumerate() {
                    if tm.kind != crate::chrome::model::TabKind::Session {
                        continue;
                    }
                    if let Some(g) = groups.iter_mut().find(|g| g.label == tm.host) {
                        g.tabs.push(i);
                        continue;
                    }
                    let fleet = fleet_hosts.iter().find(|h| h.label == tm.host);
                    let sub = match fleet.and_then(|h| h.reachability) {
                        Some(zest_mesh::Reachability::Loopback) => "loopback".to_string(),
                        Some(zest_mesh::Reachability::Lan) => match fleet.and_then(|h| h.rtt_ms) {
                            Some(ms) => format!("LAN {}", crate::chrome::layout::format_ms(ms)),
                            None => "LAN".to_string(),
                        },
                        Some(zest_mesh::Reachability::Cloud) => match fleet.and_then(|h| h.rtt_ms) {
                            Some(ms) => format!("tunnel {}", crate::chrome::layout::format_ms(ms)),
                            None => "tunnel".to_string(),
                        },
                        None => String::new(),
                    };
                    groups.push(crate::chrome::model::HostGroup {
                        label: tm.host.clone(),
                        accent: tm.accent,
                        sub,
                        online: fleet.is_none_or(crate::fleet::FleetHost::is_online),
                        tabs: vec![i],
                    });
                }
                // The footer says "N hosts online · M asleep", and until #237 a
                // relay-reachable machine was counted in M — the same wrong
                // answer the card gave, in a second place.
                let online = fleet_hosts.iter().filter(|h| h.is_online()).count().max(1);
                let asleep = fleet_hosts.len().saturating_sub(online);
                crate::chrome::model::SidebarModel {
                    groups,
                    hosts_online: online,
                    hosts_asleep: asleep,
                }
            });

        // Ungated from `TabsPosition::Left` (#385). `running` has been computed
        // for every tab all along and read at exactly one site — the sidebar's
        // dot — so the horizontal strip showed nothing at all while a command
        // ran. The clock is what the chip's ring and the row's dot both turn
        // on, and neither position has a claim on it.
        self.anim_pulse = tab_models.iter().any(|t| t.running);
        // Only what actually *turns*. A determinate arc is a static picture
        // that changes when the number does, so keeping the 80ms timer alive
        // for it would spend a frame every 80ms redrawing an identical ring —
        // and 0%-idle is a property this app has tests for, not a hope.
        self.anim_spin_tabs = tab_models.iter().any(|t| {
            matches!(t.progress, zest_core::Progress::Indeterminate)
                || (t.running && !t.progress.is_busy())
        });

        // Which chip is lit — exactly one (invariant 9), and the strip is
        // what says so. This used to be derived here instead, because
        // `display_active` knew only about Settings; both now walk the same
        // drawn order (sessions, Profiles, Settings) so the chrome and the
        // ⌘⇧] cycle cannot disagree about which tab is which.
        let active = self.tabs.display_active();

        let model = ChromeModel {
            tabs: tab_models,
            active,
            position: self.config.tabs.position,
            strip_scroll: self.strip_scroll,
            ensure_active_visible: self.strip_ensure_visible,
            hover: self.chrome_hover,
            controls,
            focused: self.focused,
            sidebar,
            screen: screen_model,
            panes,
            grid_area,
            anim,
            palette_chord: keymap::chord_for(keymap::Action::ToggleFleetPicker),
            settings_chord: keymap::chord_for(keymap::Action::ToggleSettings),
            profiles_chord: keymap::chord_for(keymap::Action::OpenProfiles),
            picker: picker_model,
            // The picker wins: it opens *over* the settings tab's content.
            palette: palette_model,
            dir_picker: dir_picker_model,
            open_file: open_file_model,
            find: find_model,
            settings: settings_model,
            launcher: launcher_model,
            block_menu: block_menu_model,
            // The pairing line wins a collision: it is about something that
            // happened to this machine and stays until it is dealt with, where
            // a drop hint is about a gesture in flight and comes back the next
            // time a file crosses the window.
            notice: notice.or_else(|| self.drop_hint.clone()),
            approval,
            confirm_close: self.confirm_close.clone(),
        };

        let colors = self.chrome_colors;
        let mut measure = |s: &str, px: f32, bold: bool, tracking: f32| {
            zest_render_wgpu::measure_ui_run(fonts, s, zest_font::Style::new(bold, false), px, tracking)
        };
        let laid = crate::chrome::layout::layout(&model, &colors, &metrics, &mut measure);
        self.strip_scroll = laid.strip_scroll;
        // One layout consumed the request; the wheel is free again — the
        // same discipline as the overlays' scroll_to_selected below.
        self.strip_ensure_visible = false;
        if let Some(state) = self.picker.as_mut() {
            state.scroll = laid.picker_scroll;
            // One layout consumed the request; the wheel is free again.
            state.scroll_to_selected = false;
        }
        if let Some(state) = self.palette_ui.as_mut() {
            state.scroll = laid.palette_scroll;
            // One layout consumed the request; the wheel is free again.
            state.scroll_to_selected = false;
        }
        if let Some(state) = self.dir_picker.as_mut() {
            state.scroll = laid.dir_picker_scroll;
            state.scroll_to_selected = false;
        }
        // Written back only when the pane actually laid out — a covered tab
        // reports 0.0, and writing that would reset a scroll the user set.
        if self.tabs.settings_active() && self.screen == AppScreen::Terminal {
            if let Some(state) = self.settings_ui.as_mut() {
                state.scroll = laid.settings_scroll;
                // One layout consumed the request; the wheel is free again.
                state.scroll_to_selected = false;
            }
        }
        if self.profiles_tab_active() {
            if let Some(state) = self.profiles_ui.as_mut() {
                state.scroll = laid.profiles_scroll;
                // One layout consumed the request; the wheel is free again.
                state.scroll_to_selected = false;
            }
        }
        self.chrome_layout = Some(laid);
    }

    /// What the pointer is over in the chrome, using the current layout.
    fn chrome_hit(&mut self, x: f64, y: f64) -> Option<HitRegion> {
        self.refresh_chrome();
        // Cached chrome first — its overlays and scrims must outrank the
        // per-frame block headers, whose regions only exist inside the grid
        // area anyway.
        self.chrome_layout
            .as_ref()
            .and_then(|l| l.hit.hit(x as f32, y as f32))
            .or_else(|| self.chip_hits.hit(x as f32, y as f32))
            .or_else(|| self.block_hits.hit(x as f32, y as f32))
    }

    /// A pointer action that landed in the chrome.
    fn on_chrome_click(
        &mut self,
        region: HitRegion,
        button: MouseButton,
        state: ElementState,
    ) {
        if state != ElementState::Pressed {
            return;
        }
        // An open dropdown menu closes on any settings click that is not one
        // of its own rows — choosing elsewhere means "never mind".
        if self.settings_ui.as_ref().is_some_and(|ui| ui.menu.is_some())
            && !matches!(region, HitRegion::SettingsMenuRow(_))
        {
            if let Some(ui) = self.settings_ui.as_mut() {
                ui.menu = None;
            }
            self.mark_chrome_dirty();
        }
        if self.profiles_ui.as_ref().is_some_and(|ui| ui.menu.is_some())
            && !matches!(region, HitRegion::SettingsMenuRow(_))
        {
            if let Some(ui) = self.profiles_ui.as_mut() {
                ui.menu = None;
            }
            self.mark_chrome_dirty();
        }
        // A click anywhere else in either editor is leaving the open field, so
        // it commits first (#272, #275) — and stays put when the buffer cannot
        // be written, which is what stops a stray click destroying it.
        //
        // One guard here rather than one per arm: the per-arm version is what
        // let `Enter` become the only exit that wrote in the first place, and
        // an arm added later inherits this instead of having to remember it.
        // Menu regions are excluded on purpose — a dropdown is modal and owns
        // its own keys, so a click inside it is not leaving anything.
        let in_menu = matches!(
            region,
            HitRegion::SettingsMenuRow(_)
                | HitRegion::SettingsMenuSearch
                | HitRegion::SettingsMenuPanel
                | HitRegion::SettingsMenuFooter
        );
        if !in_menu {
            if self.settings_tab_active() && !self.settings_commit_edit() {
                return;
            }
            // The name entry is its own control: clicking it is not leaving
            // it, and committing here would close what the click is opening.
            let on_name = region == HitRegion::ProfilesName;
            if self.profiles_tab_active() && !on_name && !self.profiles_commit_edit() {
                return;
            }
        }
        match (region, button) {
            (HitRegion::ApprovalApprove, MouseButton::Left) => self.decide_approval(true),
            (HitRegion::ApprovalDeny, MouseButton::Left) => self.decide_approval(false),
            (HitRegion::ConfirmClose, MouseButton::Left) => {
                self.answer_confirm_close(Some(true));
            }
            (HitRegion::ConfirmDetach, MouseButton::Left) => {
                self.answer_confirm_close(Some(false));
            }
            (HitRegion::ConfirmCancel, MouseButton::Left) => {
                self.answer_confirm_close(None);
            }
            // The panel and its scrim swallow, and deliberately do not
            // dismiss: one of the three answers destroys a running command,
            // and "clicked it away" must not be able to reach any of them.
            (HitRegion::ConfirmPanel, _) => {}
            // The panel (and its full-window scrim) swallows everything
            // else: a security prompt neither dismisses on a stray click
            // nor lets one fall through to the grid beneath it.
            (HitRegion::ApprovalPanel, _) => {}
            (HitRegion::Tab(addr), MouseButton::Left) => {
                // The Profiles chip is an app tab, not a session: clicking
                // it shows its pane. Through the open path (idempotent) so
                // the editor state is guaranteed to exist behind the screen.
                if addr == crate::tabs::profiles_tab_addr() {
                    self.open_profiles_tab();
                    return;
                }
                // Even when it is already the active one: clicking a session
                // means "show me this", which no screen may overrule.
                self.leave_screen();
                if self.tabs.activate_addr(addr) {
                    self.after_activation();
                }
            }
            (HitRegion::TabClose(addr), MouseButton::Left)
            | (HitRegion::Tab(addr), MouseButton::Middle) => {
                if addr == crate::tabs::profiles_tab_addr() {
                    // No chip × exists (app tabs carry none), but middle
                    // click closes every other tab and must not skip this
                    // one — closing it is closing a tab.
                    self.close_profiles_tab();
                    return;
                }
                self.close_tab(addr, false);
            }
            (HitRegion::NewTab, MouseButton::Left) => {
                // The + opens the launcher menu (design §1) — there is no
                // separate default-only half; ⌘T still spawns the default
                // directly, so it stays one keystroke away.
                self.toggle_launcher();
            }
            (HitRegion::LauncherRow(i), MouseButton::Left) => {
                if let Some(l) = self.launcher.as_mut() {
                    l.selected = i;
                }
                let action = self.launcher.as_ref().and_then(|l| l.actions.get(i).cloned());
                if let Some(action) = action {
                    self.run_launcher_action(action);
                } else {
                    self.mark_chrome_dirty();
                }
            }
            // A click on the panel beside a row chose nothing; swallowing it
            // is what makes a near-miss not a dismissal.
            (HitRegion::LauncherPanel, _) => {}
            (HitRegion::LauncherScrim, MouseButton::Left) => {
                self.launcher = None;
                self.mark_chrome_dirty();
            }
            (HitRegion::FindPrev, MouseButton::Left) => self.step_find(-1),
            (HitRegion::FindNext, MouseButton::Left) => self.step_find(1),
            (HitRegion::FindClose, MouseButton::Left) => self.close_find(),
            (HitRegion::FindCase, MouseButton::Left) => {
                // Smart case reads the query, so forcing it here would be a
                // second source of truth for the same fact. Typing a capital
                // is the toggle; the chip reports it.
                self.run_find();
                self.mark_chrome_dirty();
            }
            // Swallowed: a click beside the entry must not reach the grid and
            // move the selection out from under the search.
            (HitRegion::FindPanel, _) => {}
            (HitRegion::PalettePill, MouseButton::Left)
            | (HitRegion::SidebarSearch, MouseButton::Left) => {
                self.perform(keymap::Action::ToggleFleetPicker);
            }
            (HitRegion::FleetFooter, MouseButton::Left) => {
                // A toggle: the button that opened the fleet view is the most
                // discoverable way back out of it.
                self.show_screen(if self.screen == AppScreen::Fleet {
                    AppScreen::Terminal
                } else {
                    AppScreen::Fleet
                });
            }
            // The screen's ground swallows; its cards claim their own.
            (HitRegion::ScreenPanel, _) => {}
            (HitRegion::Pane(i), MouseButton::Left)
            // Clicking into a file moves the keyboard there too, so a wheel
            // and a keystroke agree about which pane you are in.
            | (HitRegion::EditorBody(i), MouseButton::Left) => {
                if self.tabs.active_mut().is_some_and(|t| t.focus_pane(i)) {
                    self.mark_chrome_dirty();
                }
            }
            (HitRegion::ThemeCard(i), MouseButton::Left) => {
                // The retained snapshot, not a fresh `themes::ids()`: the
                // roster can change between the frame that drew the cards
                // and the click (an import, a config reload), and index i
                // must keep meaning the card the user aimed at.
                let id = self.themes_view.get(i).cloned();
                if let Some(id) = id {
                    self.apply_theme_choice(&id);
                }
            }
            (HitRegion::ThemeImport, MouseButton::Left) => {
                // The card's promise: whatever scheme file the clipboard
                // holds becomes a theme. Parse failures land on the card
                // itself — a click with feedback nowhere reads as dead UI,
                // which is exactly what this card spent its life as.
                let outcome = match self.shared.clipboard.borrow_mut().as_mut() {
                    // No OS clipboard connection at all is a different fact
                    // from an empty one — the user cannot fix it by copying.
                    None => Err("the clipboard is unavailable in this session".to_string()),
                    Some(clipboard) => match clipboard.get_text() {
                        Ok(text) if !text.trim().is_empty() => {
                            crate::themes::import_pasted(&text)
                        }
                        // Empty and non-text (an image, a file) both land
                        // here — arboard reports each as "not available",
                        // and the remedy is the same either way.
                        _ => Err("the clipboard has no text — copy a scheme file first"
                            .to_string()),
                    },
                };
                match outcome {
                    Ok(theme) => {
                        self.theme_import_error = None;
                        // Applying is the success feedback: the new card
                        // appears, ringed active.
                        self.apply_theme_choice(&theme.id);
                        // Directly too: a re-import of the *active* theme
                        // changes no config value, so the reload above
                        // classifies it as a no-op and would repaint nothing.
                        self.apply_theme();
                    }
                    Err(e) => {
                        self.theme_import_error = Some(e);
                        self.mark_chrome_dirty();
                    }
                }
            }
            (HitRegion::FleetCard(i), MouseButton::Left) => {
                // The card's promise is the picker row's: a fresh shell on
                // that machine. Routed against the retained snapshot the
                // card indices were built from, and through the exact
                // PickerAction::Create path — back to the grid, remote
                // creates pinned to the id the roster named.
                let target = self.fleet_view.get(i).map(|h| (h.host, self.best_route(h)));
                if let Some((host, Some(route))) = target {
                    let expect = (!route.is_local()).then_some(host);
                    self.screen = AppScreen::Terminal;
                    self.spawn_tab_worker_pinned(route, None, expect, true, None);
                    self.mark_chrome_dirty();
                }
                // No route: the card drew without a hit region, so this arm
                // is only reachable by a click racing a snapshot change —
                // ignoring it is the honest answer.
            }
            (HitRegion::FleetSession(i, j), MouseButton::Left) => {
                // Attach to what is already running there (#287) — the ⌘K
                // picker's `Attach` arm, reached from the screen that shows
                // you the fleet. Resolved against the retained snapshot the
                // card indices were built from, exactly as the card above is.
                //
                // The listing is re-read rather than trusted from the drawn
                // row, because the card is redrawn on every fleet change and
                // a click can land a frame behind one: an index into a stale
                // list would attach to a neighbouring session.
                let target = self.fleet_view.get(i).and_then(|h| {
                    let crate::fleet::SessionsState::Fresh(sessions) = &h.sessions else { return None };
                    let info = sessions.get(j)?;
                    Some((info.addr, self.best_route(h)))
                });
                let Some((addr, route)) = target else { return };
                // Already open here: activate that tab rather than opening a
                // second view of one session — the picker's rule, and the one
                // that keeps a fleet card from quietly duplicating tabs.
                // `after_activation` steps off a full-pane screen by itself.
                let acted = if self.tabs.activate_addr(addr) {
                    self.after_activation();
                    true
                } else if let Some(route) = route {
                    self.screen = AppScreen::Terminal;
                    self.spawn_tab_worker(route, Some(addr));
                    true
                } else {
                    false
                };
                // Leaving the fleet screen is part of *acting*, never a
                // consolation. A host can lose its route between the layout
                // pass that drew this row and the click that lands on it, and
                // dropping the user back to the terminal with nothing opened
                // would take away the view they had and give nothing for it —
                // the card arm above already refuses on the same grounds.
                if acted {
                    self.mark_chrome_dirty();
                }
            }
            (HitRegion::FleetEnrollLocal, MouseButton::Left) => {
                self.enroll_local_daemon();
            }
            (HitRegion::FleetApproveDevice(i), MouseButton::Left) => {
                // Approve or vouch — which verb is the row's state, decided
                // where the row was built (`fleet_device_rows`), against the
                // same snapshot this index resolves into.
                self.spawn_approve(i);
            }
            (HitRegion::FleetLinkStart, MouseButton::Left) => {
                self.spawn_link();
            }
            (HitRegion::FleetLinkCancel, MouseButton::Left) => {
                self.cancel_link();
            }
            (HitRegion::FleetSignIn, MouseButton::Left) => {
                // A fresh, empty entry: the keyboard owns it from here
                // (Enter enrols, Esc drops it). field_idx/append are the
                // settings tab's concerns and idle here.
                self.enroll_entry = Some(crate::settings_ui::EditBuffer {
                    field_idx: 0,
                    buffer: TextField::default(),
                    error: false,
                    list: crate::settings_ui::ListEdit::Value,
                });
                self.mark_chrome_dirty();
            }
            (HitRegion::FleetSignOut, MouseButton::Left) => {
                self.spawn_sign_out();
            }
            (HitRegion::BlockFold(id), MouseButton::Left) => {
                // Folding a block is acting on it, so it becomes the target
                // the chords and the menu mean.
                self.set_selected_block(Some(id));
                self.toggle_fold(id);
            }
            // The rail and the band are one target with two shapes: a click on
            // either selects the block.
            (HitRegion::BlockRail(id) | HitRegion::BlockHeader(id), MouseButton::Left) => {
                self.set_selected_block(Some(id));
            }
            (HitRegion::BlockMenu(id), MouseButton::Left) => {
                // Off the ⋯ the block pass drew, so the panel hangs from the
                // affordance that opened it. The pointer is the fallback for
                // the frame after a scroll moved the header.
                let anchor = self
                    .block_menu_anchor
                    .filter(|(b, _)| *b == id)
                    .map(|(_, r)| r)
                    .unwrap_or([self.pointer_pos.0 as f32, self.pointer_pos.1 as f32, 0.0, 0.0]);
                self.open_block_menu(id, anchor);
            }
            // Right-click on a block's chrome is the menu's other door.
            (
                HitRegion::BlockRail(id)
                | HitRegion::BlockHeader(id)
                | HitRegion::BlockFold(id)
                | HitRegion::BlockMenu(id),
                MouseButton::Right,
            ) => {
                let at = [self.pointer_pos.0 as f32, self.pointer_pos.1 as f32, 0.0, 0.0];
                self.open_block_menu(id, at);
            }
            // A chip click acts on what it shows, re-read from the view this
            // frame drew, never from the region — one computation. Most kinds
            // copy their value (silently, like the block menu's copy rows);
            // the two that can do better, do: the cwd chip opens its
            // recent-directories menu, and the exit chip selects its failed
            // block *and scrolls it into view* — there is nothing to copy
            // about a failure, there is somewhere to look.
            (HitRegion::PromptChip(kind), MouseButton::Left) => {
                use crate::chrome::prompt_chips::ChipKind;
                let chip = self
                    .prompt_chips_view
                    .as_ref()
                    .and_then(|v| v.chips.iter().find(|c| c.kind == kind))
                    .cloned();
                if let Some(chip) = chip {
                    match kind {
                        ChipKind::Cwd => {
                            self.open_dir_picker(chip.value);
                        }
                        ChipKind::Exit => {
                            if let Ok(id) = chip.value.parse::<u32>() {
                                self.set_selected_block(Some(id));
                                if let Some(session) = self.tabs.active_source() {
                                    let mut term = session.terminal().lock();
                                    if let Some(line) = term
                                        .blocks()
                                        .get(zest_core::BlockId(id))
                                        .map(|b| b.prompt_line)
                                    {
                                        term.scroll_to_line(line);
                                    }
                                }
                                if let Some(w) = self.window.as_ref() {
                                    w.request_redraw();
                                }
                            }
                        }
                        _ => self.set_clipboard(chip.value),
                    }
                }
            }
            // Anything else on a chip swallows, like the block chrome below:
            // the pill paints over prompt rows, and that text must not be
            // selectable through it.
            (HitRegion::PromptChip(_), _) => {}
            // Anything else on the block's chrome swallows: the band paints
            // over the prompt rows, and that text must not be selectable
            // through it — which is the reason it has a hit region at all.
            (
                HitRegion::BlockHeader(_) | HitRegion::BlockRail(_) | HitRegion::BlockMenu(_),
                _,
            ) => {}
            (HitRegion::BlockMenuRow(i), MouseButton::Left) => {
                let action = self.block_menu.as_ref().and_then(|m| m.actions.get(i).copied());
                if let Some(action) = action {
                    self.run_block_menu_action(action);
                }
            }
            // A near-miss beside a row chose nothing; swallowing is what makes
            // it not a dismissal.
            (HitRegion::BlockMenuPanel, _) => {}
            // Any button, not just Left: a right-click away from an open menu
            // should dismiss it, not open a second one.
            (HitRegion::BlockMenuScrim, _) => {
                self.block_menu = None;
                self.mark_chrome_dirty();
            }
            (HitRegion::PickerRow(i), MouseButton::Left) => {
                if let Some(p) = self.picker.as_mut() {
                    p.selected = i;
                }
                let action = self.picker.as_ref().and_then(|p| p.actions.get(i).cloned());
                if let Some(action) = action {
                    self.run_picker_action(action, false);
                } else {
                    self.mark_chrome_dirty();
                }
            }
            // A click on the panel beside a row chose nothing; swallowing it
            // is what makes a near-miss not a dismissal.
            (HitRegion::PickerPanel, _) => {}
            (HitRegion::PickerScrim, MouseButton::Left) => {
                self.picker = None;
                self.mark_chrome_dirty();
            }
            (HitRegion::PaletteRow(i), MouseButton::Left) => {
                if let Some(p) = self.palette_ui.as_mut() {
                    p.selected = i;
                }
                self.run_palette_selection();
            }
            (HitRegion::PaletteScrim, MouseButton::Left) => {
                self.palette_ui = None;
                self.mark_chrome_dirty();
            }
            (HitRegion::DirPickerRow(i), MouseButton::Left) => {
                if let Some(p) = self.dir_picker.as_mut() {
                    p.selected = i;
                }
                self.dir_picker_activate(i);
            }
            // Outside the panel dismisses; the panel itself swallows, so a
            // click that just misses the entry does not throw away a
            // half-typed path.
            (HitRegion::OpenFileScrim, MouseButton::Left) => {
                self.open_file = None;
                self.mark_chrome_dirty();
            }
            (HitRegion::OpenFilePanel, _) => {}
            (HitRegion::DirPickerScrim, MouseButton::Left) => {
                self.dir_picker = None;
                self.mark_chrome_dirty();
            }
            // A missed click inside the panel must not fall through to the
            // scrim and dismiss what the user is reading.
            (HitRegion::DirPickerPanel | HitRegion::DirPickerRow(_) | HitRegion::DirPickerScrim, _) => {}
            // The Settings* widget regions are the shared §11 vocabulary:
            // while the Profiles screen is up they were drawn by it (the
            // Settings tab's model is not even built then), so they route
            // to the profiles state; otherwise to the settings tab as ever.
            (HitRegion::SettingsRow(i), MouseButton::Left) => {
                if self.profiles_tab_active() {
                    if let Some(ui) = self.profiles_ui.as_mut() {
                        ui.selected = i;
                    }
                } else if let Some(ui) = self.settings_ui.as_mut() {
                    ui.selected = i;
                }
                self.mark_chrome_dirty();
            }
            (HitRegion::SettingsToggle(i), MouseButton::Left) => {
                // Select first, then flip through the same path the keyboard
                // uses — one code path per change, however it arrives.
                if self.profiles_tab_active() {
                    if let Some(ui) = self.profiles_ui.as_mut() {
                        ui.selected = i;
                    }
                    self.profiles_adjust(1);
                    return;
                }
                if let Some(ui) = self.settings_ui.as_mut() {
                    ui.selected = i;
                }
                self.adjust_selected_setting(1);
            }
            (HitRegion::SettingsSlider(i), MouseButton::Left) => {
                if self.profiles_tab_active() {
                    if let Some(ui) = self.profiles_ui.as_mut() {
                        ui.selected = i;
                    }
                } else if let Some(ui) = self.settings_ui.as_mut() {
                    ui.selected = i;
                }
                self.slider_drag = Some(i);
                self.apply_slider_at(i, self.pointer_pos.0 as f32);
            }
            (HitRegion::SettingsCategory(i), MouseButton::Left) => {
                self.select_settings_category(i);
            }
            (HitRegion::SettingsBrowse(row), MouseButton::Left) => {
                // Recorded, not opened -- see `App::pending_pick`. The row is
                // resolved now rather than later because the list is rebuilt
                // every frame and the index would not survive the dialog.
                let to_profiles = self.profiles_tab_active();
                let field = if to_profiles {
                    self.profiles_field_of_row(row)
                } else {
                    self.settings_field_of_row(row)
                };
                if let Some(field) = field {
                    self.pending_pick = Some(PickRequest { field, to_profiles });
                }
            }

            (HitRegion::SettingsReset(i), MouseButton::Left) => {
                // THE DOT RESETS (§11/§12): delete the key from the file,
                // then reload through the cascade — the file stays the
                // single source of truth, exactly like every other edit.
                // The profiles dot deletes from `[profiles.<name>]`, never
                // the root.
                if self.profiles_tab_active() {
                    self.profiles_reset_row(i);
                    return;
                }
                self.reset_setting_row(i);
            }
            (HitRegion::SettingsEditToml, MouseButton::Left) => {
                self.open_config_externally();
            }
            // Typing already goes to the filter; the pill's click only says
            // "yes, this is where the characters go".
            (HitRegion::SettingsFilter, MouseButton::Left) => {}
            (HitRegion::SettingsSegment(row, opt), MouseButton::Left) => {
                if self.profiles_tab_active() {
                    if let Some(ui) = self.profiles_ui.as_mut() {
                        ui.selected = row;
                    }
                    self.profiles_apply_variant(row, opt);
                    return;
                }
                if let Some(ui) = self.settings_ui.as_mut() {
                    ui.selected = row;
                }
                self.apply_variant(row, opt);
            }
            (HitRegion::SettingsStep(row, up), MouseButton::Left) => {
                // Select first, then step through the keyboard's path — one
                // code path per change, however it arrives.
                if self.profiles_tab_active() {
                    if let Some(ui) = self.profiles_ui.as_mut() {
                        ui.selected = row;
                    }
                    self.profiles_adjust(if up { 1 } else { -1 });
                    return;
                }
                if let Some(ui) = self.settings_ui.as_mut() {
                    ui.selected = row;
                }
                self.adjust_selected_setting(if up { 1 } else { -1 });
            }
            (HitRegion::SettingsSelect(row), MouseButton::Left) => {
                // Select first, then act through the keyboard's path (Enter)
                // — one dispatch per widget, however the request arrives.
                // Arming `ui.menu` directly here left the theme pill dead:
                // ThemePicker's options are a roster, not `field.variants`,
                // so the same-pass menu resolution discarded the menu and
                // the click opened nothing. Enter already knows the picker
                // is that widget's dropdown.
                if self.profiles_tab_active() {
                    if let Some(ui) = self.profiles_ui.as_mut() {
                        ui.selected = row;
                    }
                    self.profiles_activate_selected();
                    return;
                }
                if let Some(ui) = self.settings_ui.as_mut() {
                    ui.selected = row;
                }
                self.activate_selected_setting();
            }
            (HitRegion::SettingsMenuRow(opt), MouseButton::Left) => {
                if self.profiles_tab_active() {
                    self.profiles_apply_menu_choice(opt);
                    return;
                }
                self.apply_menu_choice(opt);
            }
            // A click inside the menu keeps it: the search box is where the
            // keys already go, so focusing it is swallowing the click, and a
            // near-miss on the panel's padding must not dismiss what it was
            // aiming at. Without these both fall through to the pane.
            (HitRegion::SettingsMenuSearch | HitRegion::SettingsMenuPanel, MouseButton::Left) => {}
            (HitRegion::SettingsMenuFooter, MouseButton::Left) => {
                // "Browse all themes…": the gallery shows the swatches a
                // 288px menu cannot.
                if let Some(ui) = self.settings_ui.as_mut() {
                    ui.menu = None;
                }
                if let Some(ui) = self.profiles_ui.as_mut() {
                    ui.menu = None;
                }
                self.show_screen(AppScreen::Themes);
            }
            (HitRegion::SettingsListRemove(row, item), MouseButton::Left) => {
                self.remove_list_item(row, item);
            }
            (HitRegion::SettingsListAdd(row), MouseButton::Left) => {
                self.begin_list_add(row);
            }
            (HitRegion::SettingsListEdit(row, item), MouseButton::Left) => {
                self.begin_list_edit(row, item);
            }
            (HitRegion::SettingsListItem(row, item), MouseButton::Left) => {
                // Drag-to-reorder begins here; crossing another item applies
                // the move (order IS the setting, §11). Release ends it.
                if let Some(ui) = self.settings_ui.as_mut() {
                    ui.selected = row;
                    ui.list_drag = Some((row, item));
                }
                self.mark_chrome_dirty();
            }
            (HitRegion::ProfilesRailRow(i), MouseButton::Left) => {
                // Selecting in the rail EDITS; only the launcher launches —
                // two different verbs, two different places (§12).
                self.profiles_select_rail(i);
            }
            (HitRegion::ProfilesNew, MouseButton::Left) => {
                self.profiles_new();
            }
            (HitRegion::ProfilesName, MouseButton::Left) => {
                self.profiles_begin_rename();
            }
            (HitRegion::ProfilesDuplicate, MouseButton::Left) => {
                self.profiles_duplicate();
            }
            (HitRegion::ProfilesDelete, MouseButton::Left) => {
                self.profiles_delete();
            }
            (HitRegion::ProfilesChoice(row, opt), MouseButton::Left) => {
                self.profiles_choice(row, opt);
            }
            // PalettePanel and SettingsPanel deliberately have no arm: the
            // panels exist in the hit map to swallow clicks, not to act.
            (HitRegion::Drag, MouseButton::Left) => {
                let now = std::time::Instant::now();
                let double = self
                    .last_drag_click
                    .is_some_and(|t| now.duration_since(t) < std::time::Duration::from_millis(400));
                self.last_drag_click = Some(now);
                if let Some(w) = self.window.as_ref() {
                    if double {
                        // Double-click on empty chrome is zoom, matching what
                        // every native macOS titlebar does.
                        w.set_maximized(!w.is_maximized());
                    } else if let Err(e) = w.drag_window() {
                        tracing::debug!(error = %e, "window drag unavailable");
                    }
                }
            }
            (HitRegion::CaptionButton(which), MouseButton::Left) => {
                if let Some(w) = self.window.as_ref().map(Arc::clone) {
                    match which {
                        CaptionButton::Minimize => w.set_minimized(true),
                        CaptionButton::Maximize => w.set_maximized(!w.is_maximized()),
                        // Through the same path the window manager's own close
                        // takes. Anything else and the drawn button quietly
                        // skips `persist_tabs`, so every launch would forget
                        // the tab set — a bug that only appears next time.
                        CaptionButton::Close => self.request_close(),
                    }
                }
            }
            (HitRegion::Resize(edge), MouseButton::Left) => {
                if let Some(w) = self.window.as_ref() {
                    if let Err(e) = w.drag_resize_window(edge.into()) {
                        tracing::debug!(error = %e, "window resize unavailable");
                    }
                }
            }
            _ => {}
        }
    }

    /// Ask the process to close this window, exactly as `CloseRequested`
    /// does. The process remembers the set first; dropping the window is the
    /// detach of every tab, and the last window closing is what exits.
    pub(crate) fn request_close(&mut self) {
        self.requests.close = true;
    }

    /// What this window is showing, for the file the next launch reopens
    /// from (#23's adopt bug, retired). `None` when restore is off.
    pub(crate) fn saved_window(&self) -> Option<SavedWindow> {
        if !self.config.tabs.restore {
            return None;
        }
        // Which tabs, and which of them leads the restore, is the strip's
        // call (`persistable` keeps the filter and the active-index remap
        // together, under test); this side only adds what needs a terminal
        // lock to read.
        let (active, tabs) = self.tabs.persistable();
        let tabs = tabs
            .into_iter()
            .map(|tab| crate::tabs_state::SavedTab {
                addr: tab.addr,
                local: tab.local,
                dial_hint: tab.dial_hint.clone(),
                // The same string the chip showed: this is the name a
                // restored tab wears until its keyframe lands, so anything
                // else visibly renames every tab for a second.
                title: crate::chrome::model::terminal_label(&tab.source().terminal().lock()),
            })
            .collect();
        Some(SavedWindow { active, tabs, geometry: self.current_geometry() })
    }

    /// Toggle the fleet picker (⌘K, and the picker rows' Escape hatch).
    fn toggle_picker(&mut self) {
        let opening = self.picker.is_none();
        self.picker = match self.picker {
            Some(_) => None,
            None => {
                // One modal at a time: opening any overlay closes the
                // others, which is what keeps the modal input blocks
                // order-independent. The settings *tab* is not an overlay
                // and stays put underneath.
                self.palette_ui = None;
                self.launcher = None;
                self.block_menu = None;
                Some(PickerState {
                    selected: 0,
                    filter: TextField::default(),
                    scroll: 0.0,
                    scroll_to_selected: false,
                    actions: Vec::new(),
                    pending: None,
                })
            }
        };
        // The Blocks group is the fleet's answer, so ask on the way in: an
        // empty query is "the most recent", which is what an opening
        // palette shows.
        if opening {
            if let Some(fleet) = self.shared.fleet.get() {
                fleet.search_blocks("");
            }
        }
        self.mark_chrome_dirty();
    }

    /// Toggle the command palette (⌘/, ⌘⇧P).
    fn toggle_palette(&mut self) {
        self.palette_ui = match self.palette_ui {
            Some(_) => None,
            None => {
                self.picker = None;
                self.launcher = None;
                self.block_menu = None;
                Some(PaletteState {
                    selected: 0,
                    filter: TextField::default(),
                    scroll: 0.0,
                    scroll_to_selected: true,
                    actions: Vec::new(),
                })
            }
        };
        self.mark_chrome_dirty();
    }

    /// Run the palette's selected command: close first, then perform —
    /// the command may itself open an overlay (settings, the picker).
    fn run_palette_selection(&mut self) {
        let action =
            self.palette_ui.as_ref().and_then(|p| p.actions.get(p.selected).copied()).flatten();
        let Some(action) = action else { return };
        self.palette_ui = None;
        self.mark_chrome_dirty();
        self.perform(action);
    }

    /// Open the Settings tab, or activate the one that exists (⌘, — §11:
    /// "if it is already open it activates that tab rather than opening a
    /// second"). Never a toggle: closing it is closing a tab.
    fn open_settings_tab(&mut self) {
        // A tab activation leaves any full-pane screen, like every other
        // activation path; the modals close because the tab takes the
        // keyboard they were holding.
        self.leave_screen();
        self.picker = None;
        self.palette_ui = None;
        self.launcher = None;
        self.block_menu = None;
        if self.settings_ui.is_none() {
            // The scan is a real cost, paid once at open — the font rows'
            // fallback tags read the cached roster from then on.
            let installed =
                self.fonts.as_mut().map(Fonts::installed_families).unwrap_or_default();
            self.settings_ui = Some(SettingsUiState {
                selected: 0,
                category: crate::settings_ui::GROUP_ORDER[0].to_string(),
                filter: TextField::default(),
                scroll: 0.0,
                scroll_to_selected: true,
                actions: Vec::new(),
                fields: zest_config::ui::fields(),
                editing: None,
                installed,
                menu: None,
                list_drag: None,
            });
        }
        self.tabs.open_settings();
        self.mark_chrome_dirty();
    }

    /// Close the Settings tab — its state lives as long as the tab (§11),
    /// so closing drops it; the keyboard returns to the session underneath.
    fn close_settings_tab(&mut self) {
        // Closing the tab is leaving the field, so it commits (#275). Unlike
        // every other exit it does NOT refuse on a buffer that will not parse:
        // there would be nowhere left to show the warn border, and a ⌘W that
        // silently declines to close is worse than dropping a value that could
        // never have been written. The profiles tab settled this the same way.
        let _ = self.settings_commit_edit();
        let was_active = self.tabs.settings_active();
        self.settings_ui = None;
        self.tabs.close_settings();
        self.settled_after_close(was_active);
    }

    /// The Settings tab holds the keyboard and the grid area.
    fn settings_tab_active(&self) -> bool {
        self.tabs.settings_active() && self.settings_ui.is_some()
    }

    /// Open the long-list picker on the selected settings row.
    ///
    /// Returns false when the row has nothing to pick from, so the caller can
    /// fall back to whatever it would otherwise have done.
    /// Open the dropdown on the selected row over a *roster* the client
    /// brings — themes, installed families. `false` when there is nothing to
    /// choose from, so the caller can fall back to cycling rather than
    /// swallowing the keypress.
    ///
    /// The roster is captured here, once, on the keypress that opens the
    /// menu: enumerating installed families is a real system scan, and the
    /// model is rebuilt on every dirty frame.
    fn open_roster_menu(&mut self, append: bool) -> bool {
        let Some(idx) = self.selected_settings_field() else { return false };
        let Some(field) = self.settings_ui.as_ref().and_then(|ui| ui.fields.get(idx)) else {
            return false;
        };
        let roster = match field.widget {
            zest_config::ui::Widget::FontList => {
                self.fonts.as_mut().map(Fonts::installed_families).unwrap_or_default()
            }
            zest_config::ui::Widget::ThemePicker => crate::themes::ids(),
            _ => return false,
        };
        if roster.is_empty() {
            return false;
        }
        // Start on the value already set, so opening the menu and pressing
        // Enter is a no-op rather than a surprise. An *append* starts at the
        // top instead: there is no current value for a face being added.
        let current = (!append)
            .then(|| self.settings_value_of(idx))
            .flatten()
            .and_then(|v| match &v {
                serde_json::Value::Array(a) => {
                    a.first().and_then(|f| f.as_str().map(str::to_string))
                }
                serde_json::Value::String(s) => Some(s.clone()),
                _ => None,
            });
        let row = self.settings_ui.as_ref().map(|ui| ui.selected).unwrap_or(0);
        if let Some(ui) = self.settings_ui.as_mut() {
            let mut menu = MenuState::roster(row, roster, current.as_deref());
            menu.append = append;
            ui.menu = Some(menu);
        }
        self.mark_chrome_dirty();
        true
    }

    /// Write the dropdown's chosen option to the field it hangs off.
    ///
    /// `opt` indexes the *visible* options, which a live search has already
    /// narrowed — the same-pass contract the model documents.
    fn apply_menu_choice(&mut self, opt: usize) {
        self.mark_chrome_dirty();
        // Resolved *before* the menu is taken. Enter on a search that matched
        // nothing used to close the dropdown and apply nothing, which reads
        // as the menu breaking rather than as the filter being wrong — and
        // leaves the person to reopen it and retype. A choice that cannot
        // resolve leaves the menu exactly as it was, filter included.
        let Some((field_idx, chosen)) = self.settings_ui.as_ref().and_then(|ui| {
            let menu = ui.menu.as_ref()?;
            let field_idx = match ui.actions.get(menu.row)? {
                crate::settings_ui::RowAction::Field(i) => *i,
                crate::settings_ui::RowAction::None => return None,
            };
            // A schema select still writes its variant; only a roster menu
            // goes through the filtered list.
            if menu.roster.is_empty() {
                ui.fields.get(field_idx)?.variants.get(opt)?;
                return Some((field_idx, None));
            }
            Some((field_idx, Some(menu.matching().get(opt)?.clone())))
        }) else {
            return;
        };
        let append = self
            .settings_ui
            .as_ref()
            .and_then(|ui| ui.menu.as_ref())
            .is_some_and(|m| m.append);
        if let Some(ui) = self.settings_ui.as_mut() {
            ui.menu = None;
        }
        let Some(chosen) = chosen else {
            self.apply_variant_at(field_idx, opt);
            return;
        };
        let Some(widget) =
            self.settings_ui.as_ref().and_then(|ui| ui.fields.get(field_idx)).map(|f| f.widget)
        else {
            return;
        };
        let value = if widget == zest_config::ui::Widget::FontList {
            if append {
                // The add row grows the stack (§11: the dashed row opens
                // this menu); choosing a face already present is a no-op,
                // not a duplicate — the Curlz MT lesson, again.
                let mut arr = self
                    .settings_value_of(field_idx)
                    .and_then(|v| v.as_array().cloned())
                    .unwrap_or_default();
                if arr.iter().any(|v| v.as_str() == Some(chosen.as_str())) {
                    return;
                }
                arr.push(serde_json::Value::String(chosen));
                serde_json::Value::Array(arr)
            } else {
                // The chosen face, and only it -- see `settings_ui::adjust`
                // for why a tail is neither needed nor harmless.
                serde_json::Value::Array(vec![serde_json::Value::String(chosen)])
            }
        } else {
            serde_json::Value::String(chosen)
        };
        self.apply_edit(field_idx, value);
    }

    /// The selected settings row's field index, when it is a real field.
    fn selected_settings_field(&self) -> Option<usize> {
        let ui = self.settings_ui.as_ref()?;
        match ui.actions.get(ui.selected) {
            Some(crate::settings_ui::RowAction::Field(i)) => Some(*i),
            _ => None,
        }
    }


    /// What dropping a file right now would do.
    fn drop_target_now(&self) -> DropTarget {
        drop_target(
            self.settings_tab_active(),
            self.profiles_tab_active(),
            self.profiles_ui.as_ref().map(|ui| ui.profile.as_str()),
        )
    }

    /// A file is over the window: say what dropping it would do, if anything.
    fn on_file_hovered(&mut self, path: &std::path::Path) {
        let target = self.drop_target_now();
        // Logged because the alternative diagnosis is guesswork: if a drag does
        // nothing, this line says whether the event arrived at all (a platform
        // question) or arrived and was not claimed (ours).
        tracing::debug!(path = %path.display(), ?target, "file hovered");
        // The sniff reads a bounded header (`background::HEADER`), and this
        // runs once per file per drag
        // *entering* the window -- winit emits nothing at all for `DragOver`,
        // so it cannot run per pointer move even if it wanted to.
        let hint = match target {
            _ if !crate::background::looks_like_an_image(path) => None,
            DropTarget::Unclaimed => None,
            DropTarget::Window => {
                Some("Drop to use this picture as the window's background".to_string())
            }
            DropTarget::Profile(name) => {
                Some(format!("Drop to use this picture as {name}'s background"))
            }
        };
        if hint != self.drop_hint {
            self.drop_hint = hint;
            self.mark_chrome_dirty();
        }
    }

    fn clear_drop_hint(&mut self) {
        if self.drop_hint.take().is_some() {
            self.mark_chrome_dirty();
        }
    }

    /// A file was dropped on the window.
    ///
    /// Fires once per file, so a multi-file drop ends with the last picture in
    /// it. Deterministic and needs no notion of where a batch begins -- there
    /// is no event for that, and inferring one from `HoveredFile` would make
    /// every drop depend on a hover having been delivered first, which is a
    /// platform assumption whose failure mode is "drag and drop does nothing".
    fn on_file_dropped(&mut self, path: &std::path::Path) {
        // First, and whatever the outcome: the drag it described has ended, and
        // a hint left standing reads as the drop still being available.
        self.clear_drop_hint();

        let target = self.drop_target_now();
        let profile = match target {
            // Silent, deliberately -- see `drop_target`. A terminal has no
            // meaning for this gesture yet, and explaining that on every stray
            // drag would be worse than saying nothing.
            DropTarget::Unclaimed => return,
            DropTarget::Window => None,
            DropTarget::Profile(name) => Some(name),
        };

        if !crate::background::looks_like_an_image(path) {
            self.drop_report(
                profile.is_some(),
                format!("{} is not a picture this build can read", path.display()),
            );
            return;
        }
        let Some(text) = path.to_str() else {
            // `write_value` takes a `&str`, and a path that is not UTF-8 cannot
            // be spelled in TOML at all. Refusing beats writing a mangled one.
            self.drop_report(profile.is_some(), "that path is not valid UTF-8".to_string());
            return;
        };
        let Some(file) = Self::config_target() else {
            self.drop_report(profile.is_some(), "no config directory on this system".to_string());
            return;
        };

        // The absolute path the OS handed us, not a copy into the config
        // directory: a drop says which picture to use, and quietly duplicating
        // someone's file somewhere they did not choose is a second decision
        // they did not make.
        let value = zest_config::toml_edit::Value::from(text);
        const KEY: &str = "window.background_image";
        let wrote = match &profile {
            Some(name) => zest_config::write_profile_value(&file, name, KEY, value),
            None => zest_config::write_value(&file, KEY, value),
        };
        match wrote {
            Ok(()) => {
                // No `restart_pending` bookkeeping: this key is
                // `Invalidation::Free` by the table in `zest-config`, and the
                // reload below is the whole of applying it.
                self.settings_error = None;
                if let Some(ui) = self.profiles_ui.as_mut() {
                    ui.error = None;
                }
                tracing::info!(path = %path.display(), profile = ?profile, "background set by drop");
                self.reload_config();
            }
            Err(e) => {
                tracing::error!(error = %e, "could not write the dropped background");
                self.drop_report(profile.is_some(), format!("could not save {KEY}: {e}"));
            }
        }
        self.mark_chrome_dirty();
    }

    /// Report a refused drop on whichever editor claimed it.
    ///
    /// The two screens keep their banners in different places, and a message
    /// written to the wrong one is invisible on the screen the person is
    /// looking at.
    fn drop_report(&mut self, to_profiles: bool, message: String) {
        if to_profiles {
            self.profiles_report(message);
        } else {
            self.settings_report(message);
        }
    }


    // ----- The Profiles editor's input path (§12) --------------------------
    //
    // The Settings tab's shape, scoped to `[profiles.<name>]`: every edit is
    // write_profile_value → reload_config → rows rebuilt from the resolved
    // file — one state path; the editor never holds a value the file does
    // not. The reload re-resolves open tabs' identities (#162), which is
    // what makes a scheme edit restyle a running tab live.


    /// The picker's rows and their actions, from the fleet snapshot.
    ///
    /// One pass builds both lists, which is what keeps a drawn row and its
    /// meaning aligned by construction — the hit map's discipline, applied
    /// to indices.
    fn build_picker(
        &self,
    ) -> (Vec<crate::chrome::model::PickerRow>, Vec<PickerAction>, crate::chrome::model::HostsSearched)
    {
        use crate::chrome::model::PickerRow;
        use crate::fleet::SessionsState;

        let mut rows = Vec::new();
        let mut actions = Vec::new();
        let filter = self
            .picker
            .as_ref()
            .map(|p| p.filter.text().to_lowercase())
            .unwrap_or_default();
        let matches = |text: &str| filter.is_empty() || text.to_lowercase().contains(&filter);

        let fleet_hosts = self.shared.fleet.get().map(|f| f.snapshot()).unwrap_or_default();

        // Blocks first — the palette is primarily a history of what ran
        // anywhere in the fleet (design screen 6). The rows are every
        // reachable host's answer to the filter (`FleetModel::search_blocks`,
        // asked on every keystroke), merged newest first. Until a host has
        // answered, the blocks this window's own replicas hold for it stand
        // in, and yield the moment it speaks — `zest_fleet::merge_matches`
        // owns that rule. An in-process shell has no daemon to answer for it
        // and is seed for good.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let query = self.picker.as_ref().map(|p| p.filter.text().to_string()).unwrap_or_default();
        let view = self.shared.fleet.get().map(|f| f.block_matches());
        let answers: Vec<(zest_proto::HostId, Vec<zest_proto::BlockMatch>)> = match &view {
            Some(v) if v.query == query => v.answered.clone(),
            _ => Vec::new(),
        };
        let needle = zest_proto::search::Needle::new(&query);
        let mut seed = Vec::new();
        let mut in_process = false;
        for tab in self.tabs.iter() {
            if crate::tabs::is_placeholder_host(tab.addr.host) {
                in_process = true;
            }
            let term = tab.source().terminal();
            let term = term.lock();
            seed.extend(
                term.blocks()
                    .blocks()
                    .iter()
                    .filter(|b| b.is_command())
                    .filter(|b| needle.matches(&b.command))
                    .map(|b| {
                        zest_proto::BlockMatch::from_block(
                            tab.addr.host,
                            Some(tab.addr.session),
                            "",
                            b,
                        )
                    }),
            );
        }
        let hosts_searched = hosts_searched(view.as_ref(), &query, in_process);
        // A glance on an empty filter, so Sessions and Hosts stay above the
        // fold; a real list once something is typed (the panel scrolls).
        let cap = if query.is_empty() { PALETTE_BLOCKS_IDLE } else { PALETTE_BLOCKS_FILTERED };
        let history = zest_fleet::merge_matches(&answers, &seed, cap);
        // By id, never by label (#304): the window's own machine says
        // nothing, as its tabs never did.
        let label_of = |host: zest_proto::HostId| -> String {
            if crate::tabs::is_placeholder_host(host) {
                return String::new();
            }
            match fleet_hosts.iter().find(|h| h.host == host) {
                Some(h) if h.local => String::new(),
                Some(h) => h.label.clone(),
                None => host.short(),
            }
        };
        if !history.is_empty() {
            rows.push(PickerRow::Group { title: "Blocks".into() });
            actions.push(PickerAction::None);
            for m in &history {
                let command = m.command.trim().to_string();
                let ago = match (m.state, m.ended_ms) {
                    (zest_proto::BlockState::Running, _) => "running".to_string(),
                    (_, Some(e)) => crate::status::age_words(std::time::Duration::from_millis(
                        now_ms.saturating_sub(e),
                    )),
                    (_, None) => String::new(),
                };
                let outcome = match m.state {
                    zest_proto::BlockState::Finished { exit_code: Some(c) } => format!("exit {c}"),
                    zest_proto::BlockState::Finished { exit_code: None } => "done".to_string(),
                    _ => String::new(),
                };
                let host = label_of(m.host);
                let provenance = [host.as_str(), ago.as_str(), outcome.as_str()]
                    .iter()
                    .filter(|p| !p.is_empty())
                    .copied()
                    .collect::<Vec<_>>()
                    .join(" \u{b7} ");
                let ok = !matches!(m.state, zest_proto::BlockState::Finished { exit_code: Some(c) } if c != 0);
                // A command the store cut (ADR-020) is history to read, not
                // a thing to re-run: the row shows the cut, and Enter does
                // nothing on it rather than typing the first four kilobytes
                // of a pasted script.
                if m.command_truncated {
                    rows.push(PickerRow::Block { command: format!("{command}\u{2026}"), provenance, ok });
                    actions.push(PickerAction::None);
                } else {
                    rows.push(PickerRow::Block { command: command.clone(), provenance, ok });
                    actions.push(PickerAction::RunBlock { command });
                }
            }
        }

        // Sessions and hosts, from the fleet.
        let mut session_rows = Vec::new();
        let mut host_rows = Vec::new();
        for host in &fleet_hosts {
            // The picker prints this as a word beside a dot; `presence_of`
            // is the rule, shared with the tab strip since #297.
            let presence = presence_of(host);
            // How this host is dialled — the same rule the fleet cards read.
            // This was `host.address.map(HostRoute::Tcp)` until #250, which is
            // why an enrolled machine off this LAN had a card that opened a
            // shell and a ⌘K row that did nothing.
            let route = self.best_route(host);

            if let SessionsState::Fresh(sessions) = &host.sessions {
                for info in sessions {
                    // No command: a listing carries the title and nothing
                    // about what ran (see `SessionInfo`), so these rows keep
                    // the one fallback rather than inventing a second.
                    let title = crate::chrome::model::session_label("", &info.title);
                    if !(matches(&host.label) || matches(&title) || matches(&info.cwd)) {
                        continue;
                    }
                    let attached_here = self.tabs.iter().any(|t| t.addr == info.addr);
                    let action = if attached_here {
                        PickerAction::Activate(info.addr)
                    } else if let Some(route) = route.clone() {
                        PickerAction::Attach { addr: info.addr, route }
                    } else {
                        PickerAction::None
                    };
                    session_rows.push((
                        PickerRow::Session {
                            title,
                            // Home-shortened for this machine only — another
                            // machine's home is unknowable from here.
                            detail: if host.local {
                                crate::status::shorten_home(&info.cwd)
                            } else {
                                info.cwd.clone()
                            },
                            host: host.label.clone(),
                            attached: info.attached,
                            attached_here,
                        },
                        action,
                    ));
                }
            }

            if matches(&host.label) {
                let detail = match host.reachability {
                    Some(zest_mesh::Reachability::Loopback) => "loopback".to_string(),
                    Some(zest_mesh::Reachability::Lan) => match host.rtt_ms {
                        Some(ms) => {
                            format!("LAN \u{b7} {}", crate::chrome::layout::format_ms(ms))
                        }
                        None => "LAN".to_string(),
                    },
                    Some(zest_mesh::Reachability::Cloud) => match host.rtt_ms {
                        Some(ms) => {
                            format!("tunnel \u{b7} {}", crate::chrome::layout::format_ms(ms))
                        }
                        None => "tunnel".to_string(),
                    },
                    None => String::new(),
                };
                // No create action on a host that cannot be dialled: offering
                // an action that must fail is worse than saying so. The route
                // alone decides, exactly as `FleetCard::open` does — the extra
                // `presence != Unreachable` guard this used to carry became
                // wrong the moment routes could be relay ones (#250): a
                // machine whose advertised port refuses is precisely the one
                // the tunnel is the answer for, and suppressing its row made
                // the picker refuse what the fleet card offered.
                let action = match route {
                    Some(route) => PickerAction::Create { host: host.host, route },
                    None => PickerAction::None,
                };
                host_rows.push((
                    PickerRow::Host { label: host.label.clone(), presence, detail },
                    action,
                ));
            }
        }
        if !session_rows.is_empty() {
            rows.push(PickerRow::Group { title: "Sessions".into() });
            actions.push(PickerAction::None);
            for (row, action) in session_rows {
                rows.push(row);
                actions.push(action);
            }
        }
        if !host_rows.is_empty() {
            rows.push(PickerRow::Group { title: "Hosts".into() });
            actions.push(PickerAction::None);
            for (row, action) in host_rows {
                rows.push(row);
                actions.push(action);
            }
        }

        // Actions last: the keymap's visible commands, through the same
        // dispatch their chords use.
        let action_cap = if filter.is_empty() { 4 } else { 8 };
        let mut action_rows = Vec::new();
        // The two full-pane screens, searchable by name; chords may come
        // later, and the palette contract already allows a chordless row.
        for (name, screen) in
            [("Fleet", AppScreen::Fleet), ("Themes", AppScreen::Themes)]
        {
            if matches(name) {
                action_rows.push((
                    PickerRow::Action { name: name.to_string(), chord: String::new() },
                    PickerAction::ShowScreen(screen),
                ));
            }
        }
        for b in keymap::BINDINGS.iter().filter(|b| b.show && matches(b.name)) {
            if action_rows.len() >= action_cap {
                break;
            }
            action_rows.push((
                PickerRow::Action {
                    name: b.name.to_string(),
                    chord: keymap::chord_label(b),
                },
                PickerAction::Perform(b.action),
            ));
        }
        if !action_rows.is_empty() {
            rows.push(PickerRow::Group { title: "Actions".into() });
            actions.push(PickerAction::None);
            for (row, action) in action_rows {
                rows.push(row);
                actions.push(action);
            }
        }

        if rows.is_empty() {
            rows.push(PickerRow::Nothing);
            actions.push(PickerAction::None);
        }

        (rows, actions, hosts_searched)
    }

    /// Act on a picker row.
    ///
    /// **Every action closes the picker, with one deliberate exception.** The
    /// rule is "the user chose, so get out of the way" — and ⇧⏎ on a block is
    /// the one gesture where choosing the row is only *half* the answer: it
    /// says what to run and leaves open where. That arm re-opens the picker
    /// holding the command (see [`Pending`]), so the second half is the next
    /// row picked rather than a second overlay.
    fn run_picker_action(&mut self, action: PickerAction, shift: bool) {
        // Anything pending rides exactly one picker choice: a host or session
        // row carries it, anything else abandons it (and dismissing the picker
        // abandons it structurally — it lives on the picker's own state).
        // Before the take, deliberately. A group header is `None`, and
        // clicking one is not choosing anything — consuming the pending there
        // would cancel a "choose a machine" flow with no row picked and no
        // feedback, leaving the user to press ⇧⏎ again without knowing why.
        // (Pre-existing: `pending_profile` was dropped the same way.)
        if matches!(action, PickerAction::None) {
            return;
        }
        let pending = self.picker.as_mut().and_then(|p| p.pending.take());
        match action {
            // Unreachable — the guard above returned — and kept for
            // exhaustiveness rather than swept under a `_`, which would stop
            // the compiler naming a future variant nobody handled.
            PickerAction::None => return,
            // ⇧⏎ on a block re-opens the picker to choose a machine (§6's
            // footer: `⇧⏎ run on host…`). It used to re-run the command
            // *where it came from*, which is useful and is not what the
            // footer promises — the point of the gesture is to take
            // something you already ran and run it somewhere else.
            PickerAction::RunBlock { command } if shift => {
                self.arm_pending(Pending::Command(command));
            }
            PickerAction::RunBlock { command } => {
                self.picker = None;
                self.screen = AppScreen::Terminal;
                if let Some(session) = self.tabs.active_source() {
                    session.write(with_return(&command));
                }
            }
            PickerAction::Perform(action) => {
                self.picker = None;
                self.mark_chrome_dirty();
                self.perform(action);
                return;
            }
            PickerAction::ShowScreen(screen) => {
                self.show_screen(screen);
                return;
            }
            PickerAction::Activate(addr) => {
                self.picker = None;
                if self.tabs.activate_addr(addr) {
                    self.after_activation();
                }
                // Already open here, so there is nothing to wait for: the
                // session exists and this window holds it.
                //
                // By address, not by `active_source()`. `activate_addr` can
                // fail — a picker row a frame behind a close — and the active
                // tab is then some *other* session, which would take the
                // command. That is the rule this whole change is built on:
                // the write is keyed to the session the user chose.
                if let Some(Pending::Command(command)) = &pending {
                    let bytes = with_return(command);
                    match self.tabs.find_mut(addr) {
                        Some(tab) => tab.source().write(bytes),
                        None => tracing::warn!(
                            %addr,
                            "the session closed before its command could run"
                        ),
                    }
                }
            }
            PickerAction::Attach { addr, route } => {
                self.picker = None;
                self.screen = AppScreen::Terminal;
                if matches!(pending, Some(Pending::Split)) {
                    let expect = (!route.is_local()).then_some(addr.host);
                    let label = self.host_label_for(addr.host, &route);
                    self.spawn_pane_worker(route, Some(addr), expect, label);
                    self.mark_chrome_dirty();
                    return;
                }
                // A session that already exists on the chosen machine is the
                // cheaper half of "run on host…": attach, then write.
                let run = match &pending {
                    Some(Pending::Command(c)) => Some(c.clone()),
                    _ => None,
                };
                // `spawn_tab_worker`'s pin, computed here because this arm no
                // longer goes through it. **Not optional**: the address came
                // from an advertisement or an account listing, which are
                // claims, and `expect_host` is what checks the machine that
                // answered is the one claimed — the reason the host signs
                // first, so a client can hang up before revealing anything.
                // Dropping it would let a stale or poisoned route attach a
                // session to the wrong machine.
                let expect = (!route.is_local()).then_some(addr.host);
                self.spawn_tab_worker_pinned(route, Some(addr), expect, true, run);
            }
            PickerAction::Create { host, route } => {
                self.picker = None;
                self.screen = AppScreen::Terminal;
                if matches!(pending, Some(Pending::Split)) {
                    let expect = (!route.is_local()).then_some(host);
                    let label = self.host_label_for(host, &route);
                    self.spawn_pane_worker(route, None, expect, label);
                    self.mark_chrome_dirty();
                    return;
                }
                // The ask_host flow (design §12): the picker was opened to
                // choose this profile's host, and this row is the choice.
                if let Some(Pending::Profile(name)) = &pending {
                    let name = name.clone();
                    let meta = crate::launcher::profile_meta(&self.settings, &name);
                    let fleet = self.shared.fleet.get().map(|f| f.snapshot()).unwrap_or_default();
                    // The picked host's display name, for the provenance line
                    // — the profile itself pinned none (that is what ask_host
                    // means).
                    let label = fleet
                        .iter()
                        .find(|h| h.host == host)
                        .map(|h| h.label.clone())
                        .unwrap_or_default();
                    // Every route the picker can build now carries a launch,
                    // relay included (#250). Before that this arm fell through
                    // to a plain shell for `Relay`, so an ask_host profile
                    // pointed at a machine off this LAN silently lost its
                    // command and its appearance.
                    let target = crate::launch::resolve_picked_host(host, route, &fleet);
                    self.launch_profile_at(&name, &meta, target, label);
                    return;
                }
                // Pin remote creates to the host the roster named: the
                // address came from an advertisement, which is a claim.
                let expect = (!route.is_local()).then_some(host);
                // A command from a block, if one is riding this choice (§6):
                // armed on the tab the worker builds, because the session it
                // needs does not exist yet.
                let run = match &pending {
                    Some(Pending::Command(c)) => Some(c.clone()),
                    _ => None,
                };
                self.spawn_tab_worker_pinned(route, None, expect, true, run);
            }
        }
        self.mark_chrome_dirty();
    }

    /// The fleet's display name for `host`, falling back to the address being
    /// dialled — which still says *where* a pane is headed.
    fn host_label_for(&self, host: zest_proto::HostId, route: &HostRoute) -> String {
        self.shared.fleet.get()
            .map(|f| f.snapshot())
            .and_then(|hosts| hosts.iter().find(|e| e.host == host).map(|e| e.label.clone()))
            .unwrap_or_else(|| match route {
                HostRoute::Tcp(a) => a.clone(),
                HostRoute::LocalSocket(_) => "local".to_string(),
                HostRoute::Relay { .. } => "the host".to_string(),
            })
    }

    /// Re-open the picker holding something for the next machine to carry.
    ///
    /// Toggling rather than assuming: ⇧⏎ arrives while the picker is *open*,
    /// and `toggle_picker` would close it. The state is rebuilt so the filter
    /// starts clean — you have chosen the command, and what is left to say is
    /// which machine.
    fn arm_pending(&mut self, pending: Pending) {
        self.picker = None;
        self.toggle_picker();
        if let Some(p) = self.picker.as_mut() {
            p.pending = Some(pending);
        }
        self.mark_chrome_dirty();
    }

    /// The best way to open a session on this host right now, or `None`.
    ///
    /// The window's three facts, handed to the one rule in [`crate::route`].
    /// Every caller goes through here — the fleet cards, the ⌘K picker's host
    /// and session rows, and a profile launch — because they each derived it
    /// separately before #250 and only this one had learned about the relay.
    fn best_route(&self, host: &crate::fleet::FleetHost) -> Option<HostRoute> {
        best_route(
            host,
            self.route.as_ref(),
            self.relay_origin().as_deref(),
            matches!(self.account, AccountState::SignedIn { .. }),
        )
    }

    /// The account's relay origin, when there is one to reach through.
    fn relay_origin(&self) -> Option<String> {
        self.shared.fleet.get().and_then(|f| f.relay_origin())
    }

    /// The stored identity for hosts that are not this machine, loaded on
    /// first need — the keychain stays off the startup path, and off every
    /// path for people who never leave loopback. Falls back to a throwaway
    /// key with a loud log, same trade as `--attach`.
    fn remote_identity(&mut self) -> Option<Arc<zest_mesh::identity::ClientIdentity>> {
        let mut cached = self.shared.remote_identity.borrow_mut();
        if cached.is_none() {
            let store = zest_mesh::keystore::OsKeyStore;
            match zest_mesh::identity::ClientIdentity::load_or_create(&store) {
                Ok(i) => *cached = Some(Arc::new(i)),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "no credential store; using a throwaway key, the far host \
                         will ask for approval every time"
                    );
                    *cached = zest_mesh::identity::ClientIdentity::generate().ok().map(Arc::new);
                }
            }
        }
        cached.clone()
    }

    /// The stored identity, or a refusal — never a throwaway.
    ///
    /// [`App::remote_identity`]'s fallback is right for dialling (the far
    /// host re-approves) and wrong for enrolling: a device row bound to a key
    /// that evaporates on restart is a row nobody can ever use again, minted
    /// with a one-shot code. So this always asks the store — the cache may be
    /// holding that very fallback — and refreshes the cache only on success,
    /// which also heals a cached throwaway once the store works again.
    fn durable_identity(&mut self) -> Result<Arc<zest_mesh::identity::ClientIdentity>, String> {
        match zest_mesh::identity::ClientIdentity::load_or_create(&zest_mesh::keystore::OsKeyStore)
        {
            Ok(i) => {
                let identity = Arc::new(i);
                *self.shared.remote_identity.borrow_mut() = Some(Arc::clone(&identity));
                Ok(identity)
            }
            Err(e) => Err(format!("no credential store to keep the device key in ({e})")),
        }
    }

    fn spawn_tab_worker(&mut self, route: HostRoute, attach: Option<zest_proto::SessionAddr>) {
        let expect = attach.and_then(|a| (!route.is_local()).then_some(a.host));
        self.spawn_tab_worker_pinned(route, attach, expect, true, None);
    }


    /// Open a session on `route` off the event loop and park the finished
    /// tab for `Wakeup::TabsChanged`.
    ///
    /// A worker because a dead host costs a connect timeout, and seconds of
    /// frozen UI is the one price a picker must never charge.
    fn spawn_tab_worker_pinned(
        &mut self,
        route: HostRoute,
        attach: Option<zest_proto::SessionAddr>,
        expect_host: Option<zest_proto::HostId>,
        focus: bool,
        // `run`: a command to execute once the session exists (§6's
        // `⇧⏎ run on host…`). Armed on the tab rather than written here,
        // because here there is no session yet — the dial is on a worker and
        // this call returns long before it lands.
        run: Option<String>,
    ) {
        let identity = if route.is_local() {
            self.client_identity.clone()
        } else {
            self.remote_identity()
        };
        let Some(identity) = identity else {
            tracing::warn!("no identity to dial with; cannot open the tab");
            return;
        };

        let (cols, rows) = self.current_dims();
        let scrollback = self.config.scrollback;
        let local = route.is_local();
        let command =
            if local { self.config.shell.clone().unwrap_or_default() } else { String::new() };
        // Owned before the worker takes it, beside `command`, and empty for a
        // remote host for `command`'s own reason: `shell.env` is a machine's
        // setting and the far daemon applies its own (#488).
        let env = if local { self.config.shell_env.clone() } else { Vec::new() };

        let cell = Arc::new(parking_lot::Mutex::new(crate::tabs::placeholder_addr(
            self.shared.mint_placeholder(),
        )));
        let wake = wake_for(&self.proxy, Arc::clone(&cell), Arc::clone(&self.activity));
        let pending = Arc::clone(&self.pending_tabs);
        let proxy = self.proxy.clone();
        let palette = self.palette.clone();

        // A first contact holds the connect while a person over there
        // approves; the matching code must reach this window's chrome, not a
        // log line (#190). Named like the fleet names the host, falling back
        // to the address being dialled — which still says *where* the
        // approval is owed.
        let pending_host = expect_host
            .and_then(|h| {
                self.shared.fleet.get()
                    .map(|f| f.snapshot())
                    .and_then(|hosts| hosts.iter().find(|e| e.host == h).map(|e| e.label.clone()))
            })
            .or_else(|| match &route {
                HostRoute::Tcp(a) => Some(a.clone()),
                // The pairing notice falls back to "the host": a relay
                // origin is where the pipe is, not who is at its far end.
                HostRoute::LocalSocket(_) | HostRoute::Relay { .. } => None,
            });
        let on_pending = (!local).then(|| {
            self.pairing_notifier(pending_host.unwrap_or_else(|| "the host".into()))
        });
        let pairing = Arc::clone(&self.pairing);

        let spawned = std::thread::Builder::new().name("zest-tab-open".into()).spawn(move || {
            let opts = crate::remote::AttachOptions {
                identity: &identity,
                label: "zesterm",
                command: &command,
                cwd: "",
                env: &env,
                profile: "",
                cols,
                rows,
                scrollback,
                adopt: false,
                local,
                expect_host,
                on_pending,
            };
            let result = match attach {
                Some(addr) => {
                    crate::remote::RemoteSession::attach_existing(route.dialer(), addr, &opts, wake)
                }
                None => crate::remote::RemoteSession::create_and_attach(route.dialer(), &opts, wake),
            };
            // Either way the wait is over — the code must not outlive it.
            clear_pairing(&pairing, &proxy);
            match result {
                Ok(session) => {
                    *cell.lock() = session.addr();
                    session.terminal().lock().set_palette(palette);
                    // No dial hint for a relayed tab: restore-by-address has
                    // no address for one, and rebuilding its route from the
                    // account is later work.
                    let hint = route.dial_hint();
                    pending.lock().push((
                        Tab::daemon(session, local, (cols, rows))
                            .with_dial_hint(hint)
                            .with_pending_input(run.as_deref().map(with_return)),
                        focus,
                    ));
                    let _ = proxy.send_event(Wakeup::TabsChanged);
                }
                // The picker is already closed; a failure is a log line for
                // now and a placeholder tab once restore lands.
                Err(e) => tracing::warn!(error = %e, "could not open the picked session"),
            }
        });
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "could not start the tab worker");
        }
    }

    /// What to run in a fresh local shell, per the settings — or per `command`
    /// when a profile names one, which must be decided *here*: integration is
    /// injected against the command line this returns, so a caller that
    /// overwrites it afterwards is a caller that hooked the wrong shell (a
    /// profile-launched WSL tab got a zsh's `ZDOTDIR`, and a pwsh profile had
    /// its appended `-Command` thrown away).
    ///
    /// Returns the spec **and** the variables shell integration injected: the
    /// in-process spawn layers a profile's environment after this returns, and
    /// it needs the same collision warning `apply_shell_settings` gets. It used
    /// to recompute the list from `spec.env.len()` at that point, which is a
    /// mark taken after the thing it measures — always empty, so a profile
    /// setting `ZDOTDIR` lost its command blocks in silence (#496).
    fn build_spec(&self, command: Option<&str>) -> (CommandSpec, Vec<String>) {
        let mut spec = CommandSpec::default_shell();
        if let Some(command) = command {
            spec.command_line = command.to_string();
        } else if let Some(shell) = &self.config.shell {
            spec.command_line = shell.clone();
        }
        // The in-process path gets the same hook as the daemon's, or
        // `--no-daemon` would silently be a terminal without command blocks.
        //
        // Before the settings, because this appends environment of its own --
        // zsh is hooked entirely through `ZDOTDIR` -- and the user's entries
        // have to be applied last to actually win. Whatever it added is the
        // tail, which is how `apply_shell_settings` knows which collisions are
        // worth warning about.
        let mut injected = Vec::new();
        if let Some(dir) = zest_config::paths::config_dir() {
            injected = spec.enable_shell_integration(&dir.join("shell-integration"));
        }
        // The daemon's spawn makes the same read (#426); an in-process
        // session diverging from a daemon-backed one over which prompt it
        // got would be the two paths quietly disagreeing about the product.
        if self.settings.prompt.compact_ps1 {
            spec.env.push(("ZESTERM_COMPACT_PS1".into(), "1".into()));
        }
        apply_shell_settings(&mut spec, &self.config, &injected);
        (spec, injected)
    }

    /// The grid size a tab should be told right now.
    fn current_dims(&self) -> (u16, u16) {
        match (self.fonts.as_ref(), self.gpu.as_ref()) {
            (Some(fonts), Some(gpu)) => {
                self.insets().grid_dims(fonts.cell_metrics(), gpu.config.width, gpu.config.height)
            }
            _ => (80, 24),
        }
    }

    /// Open a new default-shell tab on the current tab's host (⌘T; the +
    /// button used to do this directly and now opens the launcher instead —
    /// the chord is how the default stays one keystroke away).
    fn new_tab(&mut self) {
        self.open_shell_tab(None, None, None, Vec::new());
    }

    /// The command a command-less launch resolves to — the launcher rows'
    /// caption and the profiles editor's unset `command` row. On a remote
    /// route the far host runs its own default shell, and captioning that
    /// with this machine's `shell.command` would name a command that will
    /// not run.
    fn shell_fallback(&self) -> String {
        match self.route {
            Some(HostRoute::Tcp(_) | HostRoute::Relay { .. }) => {
                "the host's default shell".to_string()
            }
            _ => self
                .config
                .shell
                .clone()
                .unwrap_or_else(|| CommandSpec::default_shell().command_line),
        }
    }

    /// Launch a named profile (a launcher row, or its digit) on the host its
    /// `host` key pins — the window's route when it pins none (issue #175,
    /// design §12's launch semantics).
    /// Launch whichever kind of row the launcher offered (#268).
    fn launch_profile_ref(&mut self, target: &crate::launcher::ProfileRef) {
        match target {
            crate::launcher::ProfileRef::Local(name) => self.launch_profile(name),
            crate::launcher::ProfileRef::Remote { host, name } => {
                self.launch_published(*host, name);
            }
        }
    }

    /// Launch a profile a *remote* machine published (#262).
    ///
    /// Runs what that machine said, on that machine — never re-resolved
    /// against this one's config. Re-resolving the name locally would apply
    /// our `profiles.defaults` to their profile, so a `nightly` on the build
    /// box would silently inherit this laptop's command; and if we have no
    /// profile by that name at all, the cascade answers empty-over-Defaults
    /// rather than failing, which is worse — a row that launches something
    /// plausible and wrong.
    fn launch_published(&mut self, host: zest_proto::HostId, name: &str) {
        let fleet = self.shared.fleet.get().map(|f| f.snapshot()).unwrap_or_default();
        let Some(entry) = fleet.iter().find(|h| h.host == host) else {
            // `host_id` rather than `host`: the machine is gone, so there is no
            // label to give — and a field that means an id here and a label
            // three lines down is a filter that silently matches neither.
            tracing::warn!(host_id = %host.short(), "that machine has left the fleet");
            return;
        };
        let Some(profile) = entry
            .offer
            .as_ref()
            .and_then(|o| o.profiles.iter().find(|p| p.name == name))
        else {
            // The far config changed under an open menu. Nothing to launch and
            // nothing to guess at.
            tracing::warn!(host = %entry.label, profile = %name, "that machine no longer offers it");
            return;
        };

        let identity = crate::tabs::ProfileIdentity::from_published(profile);
        let label = entry.label.clone();
        let target = crate::launch::resolve_picked_host(
            host,
            match self.best_route(entry) {
                Some(route) => route,
                None => {
                    // §12: a launch at a host we cannot reach is still a
                    // launch — a connecting tab that settles failed, saying
                    // so — never a silent refusal.
                    self.spawn_connecting_tab(
                        ProfileLaunch {
                            name: name.to_string(),
                            identity,
                            command: profile.command.clone(),
                            cwd: profile.starting_directory.clone(),
                            env: Vec::new(),
                        },
                        label.clone(),
                        crate::launch::HostTarget::Unroutable {
                            error: format!("no way to reach host '{label}' right now"),
                        },
                    );
                    return;
                }
            },
            &fleet,
        );
        let command = profile.command.clone();
        let cwd = profile.starting_directory.clone();
        match target {
            crate::launch::HostTarget::Local => {
                self.open_shell_tab(
                    (!command.is_empty()).then_some(command),
                    Some(identity),
                    (!cwd.is_empty()).then_some(cwd),
                    // A published profile belongs to the machine that
                    // published it, and `HostProfile` deliberately carries no
                    // environment: a host must not hand its profiles'
                    // environments to every paired device. Resolving the name
                    // against *this* machine's config instead would be worse
                    // than nothing -- a local profile can share a name with a
                    // remote one and mean something else entirely. So nothing
                    // travels but the name, and the host applies its own
                    // profile's env when the launch names it (#487 phase 3,
                    // #559) -- `open_shell_tab` sends `identity.name`.
                    Vec::new(),
                );
            }
            target => self.spawn_connecting_tab(
                ProfileLaunch { name: name.to_string(), identity, command, cwd, env: Vec::new() },
                label,
                target,
            ),
        }
    }

    fn launch_profile(&mut self, name: &str) {
        let meta = crate::launcher::profile_meta(&self.settings, name);
        if meta.ask_host {
            // Host-agnostic profile: the fleet picker chooses the machine,
            // and its Create action carries this launch there.
            if self.picker.is_none() {
                self.toggle_picker();
            }
            if let Some(p) = self.picker.as_mut() {
                p.pending = Some(Pending::Profile(name.to_string()));
            }
            return;
        }
        let fleet = self.shared.fleet.get().map(|f| f.snapshot()).unwrap_or_default();
        let target = crate::launch::resolve_host(
            meta.host.as_deref(),
            &fleet,
            self.route.as_ref(),
            self.relay_origin().as_deref(),
            matches!(self.account, AccountState::SignedIn { .. }),
        );
        let host_label = meta.host.clone().unwrap_or_default();
        self.launch_profile_at(name, &meta, target, host_label);
    }

    /// The launch itself, host already decided. Local targets run inline on
    /// the window's proven route (sub-millisecond, exactly like ⌘T); remote
    /// and unroutable ones go up immediately as a connecting tab and settle
    /// off a worker — a cold host must cost a placeholder, never a frozen
    /// event loop or a silent `warn!`.
    fn launch_profile_at(
        &mut self,
        name: &str,
        meta: &zest_config::profiles::ProfileMeta,
        target: crate::launch::HostTarget,
        host_label: String,
    ) {
        let identity = crate::tabs::ProfileIdentity::resolve(&self.settings, name);
        let cwd = meta.starting_directory.clone();
        match target {
            crate::launch::HostTarget::Local => {
                self.open_shell_tab(
                    meta.command.clone(),
                    Some(identity),
                    cwd,
                    meta.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                );
            }
            target => {
                let command = crate::launch::launch_command(
                    meta.command.clone(),
                    false,
                    self.config.shell.as_deref(),
                );
                // This machine's own profile, launched elsewhere: its entries
                // travel unexpanded, and the far host resolves them -- the
                // remote half of #487's "on every spawn path", which sent
                // nothing until #559. `shell.env` stays home; that is the far
                // machine's setting to apply.
                self.spawn_connecting_tab(
                    ProfileLaunch {
                        name: name.to_string(),
                        identity,
                        command,
                        cwd: cwd.unwrap_or_default(),
                        env: meta.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                    },
                    host_label,
                    target,
                );
            }
        }
    }

    /// Push a connecting tab for a remote (or unroutable) launch and let a
    /// worker dial with bounded retries (issue #175).
    ///
    /// The tab appears NOW: placeholder address, the chrome's connecting
    /// treatment, and a provenance line in the pane — "New session ·
    /// profile on host · command" in the scheme's dim colour (§12). It
    /// settles live when an attach succeeds, or into the dead-tab treatment
    /// carrying the error after [`crate::launch::MAX_DIALS`] failures.
    ///
    /// `launch.name` goes on the wire as the launch's profile, so the far
    /// host resolves `env`'s placeholders against its own directories and
    /// applies its own profile of that name beneath (#559).
    fn spawn_connecting_tab(
        &mut self,
        launch: ProfileLaunch,
        host_label: String,
        target: crate::launch::HostTarget,
    ) {
        let ProfileLaunch { name, identity, command, cwd, env } = launch;
        let Some(client) = self.remote_identity() else {
            tracing::warn!("no identity to dial with; cannot launch the profile");
            return;
        };

        let (cols, rows) = self.current_dims();
        let seed = self.palette_for(Some(&identity));
        // The same rule the launcher row used, from the same function: these
        // read differently for one launch until they shared one — the row said
        // `zsh -l` and the tab it opened said "the host's default shell".
        //
        // By id, never by label: `target` carries the machine this will
        // actually dial, and two machines may share a display name — the same
        // reason the launcher's group keys carry a `HostId`. An unroutable
        // target has no id to match, and no far shell to know either.
        let far_shell = match &target {
            crate::launch::HostTarget::Remote { host, .. } => self.shared.fleet.get()
                .map(|f| f.snapshot())
                .unwrap_or_default()
                .iter()
                .find(|h| h.host == *host)
                .and_then(|h| h.offer.as_ref())
                .map(|o| o.default_shell.clone())
                .unwrap_or_default(),
            _ => String::new(),
        };
        let shown = crate::launcher::shown_command(&command, &far_shell);
        let provenance = format!("New session \u{b7} {name} on {host_label} \u{b7} {shown}");
        let pending = crate::tabs::PendingSession::new(
            cols,
            rows,
            seed.clone(),
            &name,
            &provenance,
            &host_label,
        );
        let placeholder = crate::tabs::placeholder_addr(self.shared.mint_placeholder());
        let hint = match &target {
            crate::launch::HostTarget::Remote { route, .. } => route.dial_hint(),
            _ => None,
        };
        self.tabs.push(
            Tab::connecting(placeholder, pending, (cols, rows))
                .with_identity(Some(identity))
                .with_dial_hint(hint),
        );
        self.after_activation();
        self.relayout_grid();

        let cell = Arc::new(parking_lot::Mutex::new(placeholder));
        let proxy = self.proxy.clone();
        let activity = Arc::clone(&self.activity);
        let outcomes = Arc::clone(&self.pending_launches);
        let scrollback = self.config.scrollback;
        // First contact with this host holds the dial while a person over
        // there approves — the matching code goes up as the chrome's notice,
        // beside this launch's connecting tab (#190).
        let on_pending = Some(self.pairing_notifier(host_label.clone()));
        let pairing = Arc::clone(&self.pairing);
        let profile = name;
        let spawned = std::thread::Builder::new().name("zest-profile-launch".into()).spawn(
            move || {
                let mut failures = 0u32;
                let outcome = loop {
                    let attempt = match &target {
                        crate::launch::HostTarget::Remote { host, route } => {
                            let wake = wake_for(&proxy, Arc::clone(&cell), Arc::clone(&activity));
                            crate::remote::RemoteSession::create_and_attach(
                                route.dialer(),
                                &crate::remote::AttachOptions {
                                    identity: &client,
                                    label: "zesterm",
                                    command: &command,
                                    cwd: &cwd,
                                    // Remote by construction (`local: false`
                                    // below), so this machine's `shell.env`
                                    // stays home; the far daemon applies its
                                    // own (#488). Only the profile's entries
                                    // travel, unexpanded, beside its name.
                                    env: &env,
                                    profile: &profile,
                                    cols,
                                    rows,
                                    scrollback,
                                    adopt: false,
                                    local: false,
                                    // The address came from an advertisement,
                                    // which is a claim; pin the identity it
                                    // claimed, like the picker's creates do.
                                    expect_host: Some(*host),
                                    on_pending: on_pending.clone(),
                                },
                                wake,
                            )
                            .map_err(|e| e.to_string())
                        }
                        crate::launch::HostTarget::Unroutable { error } => Err(error.clone()),
                        // Local never comes here; launch_profile_at ran it
                        // inline. Treated as a failure rather than a panic —
                        // the never-crash rule.
                        crate::launch::HostTarget::Local => {
                            Err("local launches do not dial".to_string())
                        }
                    };
                    match attempt {
                        Ok(session) => {
                            *cell.lock() = session.addr();
                            session.terminal().lock().set_palette(seed.clone());
                            break Ok(session);
                        }
                        // Nothing to dial means nothing to retry: an unknown
                        // label or an empty address set cannot come back in
                        // two seconds, and the backoff would only delay the
                        // honest failure the tab exists to show.
                        Err(e)
                            if !matches!(
                                &target,
                                crate::launch::HostTarget::Remote { .. }
                            ) =>
                        {
                            break Err(e);
                        }
                        Err(e) => {
                            failures += 1;
                            match crate::launch::verdict_after(failures) {
                                crate::launch::DialVerdict::RetryAfter(pause) => {
                                    std::thread::sleep(pause);
                                }
                                crate::launch::DialVerdict::GiveUp => break Err(e),
                            }
                        }
                    }
                };
                // The dial settled, live or failed — the code must not
                // outlive the wait it was informing.
                clear_pairing(&pairing, &proxy);
                outcomes.lock().push((placeholder, outcome));
                let _ = proxy.send_event(Wakeup::TabsChanged);
            },
        );
        if let Err(e) = spawned {
            tracing::warn!(error = %e, "could not start the launch worker");
            if let Some(tab) = self.tabs.find_mut(placeholder) {
                tab.resolve_failed("no thread for the launch worker");
            }
        }
    }

    /// Open a tab running `command` (the configured shell when `None`),
    /// wearing `identity` — the profile appearance seam from #162, so a
    /// profile-launched tab gets its scheme on its very first frame.
    ///
    /// One daemon per window today, so "the current tab's host" is the
    /// window's route; the fleet model makes it genuinely per-tab. Runs
    /// inline: creating on an already-proven route is sub-millisecond on
    /// loopback and a few on the LAN — the picker's cold dials are the ones
    /// that must not block, and they arrive with the fleet model.
    /// `env` is the profile's own, unexpanded: `${profile_dir}` names a
    /// directory on the machine that runs the shell, so the host resolves it.
    /// The profile's *name* travels beside it, taken from `identity` — which
    /// already carries it, and is the only thing here that knows whether there
    /// is a profile at all.
    fn open_shell_tab(
        &mut self,
        command: Option<String>,
        identity: Option<crate::tabs::ProfileIdentity>,
        cwd: Option<String>,
        env: Vec<(String, String)>,
    ) {
        let profile = identity.as_ref().map(|i| i.name.clone()).unwrap_or_default();
        let (cols, rows) = self.current_dims();
        // Seeded before the first byte arrives, so the grid never flashes
        // the window's palette under a profile's scheme.
        let seed = self.palette_for(identity.as_ref());

        match (&self.route, &self.client_identity) {
            (Some(route), Some(client)) => {
                let cell = Arc::new(parking_lot::Mutex::new(crate::tabs::placeholder_addr(
                    self.shared.mint_placeholder(),
                )));
                let wake = wake_for(&self.proxy, Arc::clone(&cell), Arc::clone(&self.activity));
                // Empty means the host's default shell — for a remote host,
                // its shell, never this machine's command line. A profile's
                // command travels as written: it is what the profile means,
                // whichever machine runs it. (`launch_command` is the tested
                // statement of this rule.)
                let command = crate::launch::launch_command(
                    command.clone(),
                    route.is_local(),
                    self.config.shell.as_deref(),
                );
                let launch_env: Vec<(String, String)> = if route.is_local() {
                    self.config.shell_env.iter().cloned().chain(env.iter().cloned()).collect()
                } else {
                    // A remote host applies its own `shell.env`; only the
                    // profile's entries are this launch's to carry.
                    env.clone()
                };
                let session = crate::remote::RemoteSession::create_and_attach(
                    route.dialer(),
                    &crate::remote::AttachOptions {
                        identity: client,
                        label: "zesterm",
                        command: &command,
                        cwd: cwd.as_deref().unwrap_or_default(),
                        // The daemon-backed twin of what `apply_shell_settings`
                        // does for the in-process path. Without it the two
                        // disagree about the user's own configuration, and the
                        // one that wins is the one nobody takes (#488).
                        //
                        // Not redundant with the daemon reading the same file:
                        // this is the *resolved* value, so it carries layers
                        // the daemon cannot see -- `--profile` above all, which
                        // the daemon loads with `Options::default()` and
                        // therefore never applies. Where they do agree the
                        // entry simply arrives twice with the same value, and
                        // last-wins makes that a no-op.
                        //
                        // The profile's own entries go after, so the more
                        // specific of the two wins: a profile names the
                        // identity this tab is *for*, and losing to a
                        // machine-wide default would make it the identity of
                        // whoever configured the box.
                        env: &launch_env,
                        profile: &profile,
                        cols,
                        rows,
                        scrollback: self.config.scrollback,
                        adopt: false,
                        local: route.is_local(),
                        expect_host: None,
                        // Inline on the event loop over the window's already
                        // proven route: a pend here could not paint anyway.
                        on_pending: None,
                    },
                    wake,
                );
                match session {
                    Ok(session) => {
                        *cell.lock() = session.addr();
                        self.seed_terminal(&mut session.terminal().lock(), seed);
                        let local = route.is_local();
                        // A create should never collide, but the daemon owns
                        // session ids — adopt guards every path the same way
                        // (#188); a refused duplicate detaches on drop.
                        let tab = Tab::daemon(session, local, (cols, rows))
                            .with_identity(identity)
                            // What actually crossed the wire, not just the
                            // profile's half: a split reuses this verbatim,
                            // and re-deriving the machine's half at split time
                            // would give a pane a different environment from
                            // its tab the moment `shell.env` changed.
                            .with_launch_env(launch_env.clone());
                        if let Some(dup) = self.tabs.adopt(tab, true) {
                            tracing::info!(addr = %dup.addr, "session already open; activating its tab");
                            drop(dup);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "could not open a new tab");
                        return;
                    }
                }
            }
            // No daemon (--no-daemon, or it was unreachable): another
            // in-process pty. Degraded but honest — the tab works, it just
            // cannot outlive the window.
            _ => {
                let addr = crate::tabs::placeholder_addr(self.shared.mint_placeholder());
                let cell = Arc::new(parking_lot::Mutex::new(addr));
                let (mut spec, injected) = self.build_spec(command.as_deref());
                // The profile's starting_directory, resolved by the machine
                // that spawns — here, this one (§12: the daemon path sends
                // it over the wire instead).
                if let Some(dir) = cwd.as_deref().filter(|d| !d.is_empty()) {
                    spec.cwd = Some(dir.into());
                }
                // The profile's own environment, after `build_spec` has
                // applied `shell.env`, so the more specific wins. Expanded
                // here because here *is* the host: with no daemon there is
                // nobody else to resolve `${profile_dir}` against, and the
                // answer has to be the same one the daemon would give or the
                // two paths would put a profile's files in two places.
                // Taken after `build_spec` has applied `shell.env` and the
                // integration hook, so `${env:…}` reads what the child will
                // really have -- and before this profile's own entries, which
                // is what makes "never a sibling" structural.
                let ctx = zest_config::profiles::ExpandContext {
                    profile: profile.clone(),
                    config_dir: zest_config::paths::config_dir(),
                    home: crate::launch::home_dir(),
                    env: spec.effective_env(),
                };
                spec.layer_env(
                    env.iter().map(|(k, v)| (k.clone(), zest_config::profiles::expand(v, &ctx))),
                    &injected,
                );
                match Session::spawn(
                    &spec,
                    PtySize::new(cols, rows),
                    self.config.scrollback,
                    wake_for(&self.proxy, cell, Arc::clone(&self.activity)),
                ) {
                    Ok(session) => {
                        self.seed_terminal(&mut session.terminal().lock(), seed);
                        self.tabs.push(
                            Tab::in_process(session, addr, (cols, rows))
                                .with_identity(identity)
                                // The in-process twin of the daemon branch:
                                // the spec's own entries, so a split of this
                                // tab reuses exactly what this shell got.
                                .with_launch_env(spec.env.to_vec()),
                        );
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "could not spawn a new in-process tab");
                        return;
                    }
                }
            }
        }

        self.after_activation();
        self.relayout_grid();
        self.requests.persist = true;
    }

    /// Close one tab: local sessions die, remote ones are only let go of.
    ///
    /// `already_exited` marks the child as gone (a `TabExited` wakeup), where
    /// there is nothing left to kill. The last tab closing closes the window.
    fn close_tab(&mut self, addr: zest_proto::SessionAddr, already_exited: bool) {
        // An app tab has no session to kill or detach; closing it is
        // dropping its state and returning the keyboard (§11, and §12 which
        // takes that rule whole). Both sentinels answer here so that the
        // chip's ×, middle-click and ⌘W cannot disagree about what a close
        // means — which is how Profiles ended up closable three different
        // ways and drawable in only one position (#494).
        if addr == crate::tabs::settings_addr() {
            self.close_settings_tab();
            return;
        }
        if addr == crate::tabs::profiles_tab_addr() {
            self.close_profiles_tab();
            return;
        }
        // Decided *before* the tab leaves the strip: a confirm that has
        // already taken it out has nothing left to cancel back to.
        match self.close_decision(addr, already_exited) {
            CloseDecision::Close => self.finish_close_tab(addr, already_exited, false),
            CloseDecision::Detach => self.finish_close_tab(addr, already_exited, true),
            CloseDecision::Ask(question) => {
                // The one-overlay rule (`toggle_picker` and friends): a
                // question drawn over an open menu is a question about a tab
                // whose chip you can no longer see.
                self.picker = None;
                self.palette_ui = None;
                self.launcher = None;
                self.block_menu = None;
                self.confirm_close = Some(question);
                self.mark_chrome_dirty();
            }
        }
    }

    /// What closing `addr` should actually do, per the settings and what the
    /// tab is doing right now.
    fn close_decision(
        &self,
        addr: zest_proto::SessionAddr,
        already_exited: bool,
    ) -> CloseDecision {
        let Some(tab) = self.tabs.iter().find(|t| t.addr == addr) else {
            return CloseDecision::Close;
        };
        let facts = TabFacts {
            already_exited,
            dead: tab.dead,
            local: tab.local,
            connecting: tab.connecting,
            busy: Self::tab_is_busy(tab),
            can_detach: !matches!(tab.source().origin(), Origin::InProcess),
        };
        match close_policy(
            self.config.tabs.close_action,
            self.config.tabs.confirm_close_when_busy,
            facts,
        ) {
            ClosePolicy::Close => CloseDecision::Close,
            ClosePolicy::Detach => CloseDecision::Detach,
            ClosePolicy::Ask => CloseDecision::Ask(self.close_question(tab)),
        }
    }

    /// Whether this tab is doing something a close would destroy.
    ///
    /// **`alt_screen` is in this test on purpose.** `BlockIndex` records no
    /// markers at all while the alternate screen is up
    /// (`zest_core`'s `block_line`), so a tab running `vim` or `htop` reports
    /// no running block — and a full-screen editor with unsaved work is
    /// exactly where a mistaken ⌘W costs the most.
    ///
    /// The honest limit, stated rather than papered over: a shell with no
    /// integration (bash, fish, `cmd.exe`) mints no blocks either, so a
    /// command running under one of those is invisible here. `alt_screen`
    /// still covers its TUIs.
    fn tab_is_busy(tab: &Tab) -> bool {
        let term = tab.source().terminal();
        let term = term.lock();
        term.blocks().last().is_some_and(zest_core::Block::is_running)
            || term.modes().contains(zest_core::Modes::ALT_SCREEN)
    }

    /// The question to put on screen for this tab, with what it would end
    /// *named* — "something is running" is the sentence this modal exists to
    /// avoid, because the whole reason to stop someone is that they may have
    /// forgotten what.
    fn close_question(&self, tab: &Tab) -> crate::chrome::model::ConfirmCloseModel {
        let (title, what) = {
            let term = tab.source().terminal();
            let term = term.lock();
            let title = crate::chrome::model::terminal_label(&term);
            let running = term
                .blocks()
                .last()
                .filter(|b| b.is_running())
                .map(|b| b.command.trim().to_string())
                .filter(|c| !c.is_empty());
            let what = what_is_running(
                running.as_deref(),
                term.modes().contains(zest_core::Modes::ALT_SCREEN),
            );
            (title, what)
        };
        let can_detach = !matches!(tab.source().origin(), Origin::InProcess);
        crate::chrome::model::ConfirmCloseModel {
            addr: tab.addr,
            title: format!("Close \u{201c}{title}\u{201d}?"),
            body: format!("{what} is still running."),
            hint: if can_detach {
                DETACH_HINT.to_string()
            } else {
                NO_DAEMON_HINT.to_string()
            },
            choices: if can_detach {
                crate::chrome::model::ConfirmChoices::DetachOrClose
            } else {
                crate::chrome::model::ConfirmChoices::CloseOnly
            },
        }
    }

    /// What ⌘B has to say when there is no daemon to leave the session with.
    ///
    /// A panel rather than a log line: the action promised not to end the
    /// shell, so doing nothing *silently* is indistinguishable from a broken
    /// keybinding. It states the refusal and offers one button, which is
    /// deliberately not "Close and stop it" — answering "that cannot be
    /// detached" with a destructive default is how a gesture that promised
    /// not to end a shell ends one.
    fn refuse_detach(&mut self, tab_addr: zest_proto::SessionAddr, title: String) {
        self.picker = None;
        self.palette_ui = None;
        self.launcher = None;
        self.block_menu = None;
        self.confirm_close = Some(crate::chrome::model::ConfirmCloseModel {
            addr: tab_addr,
            title: format!("Cannot detach \u{201c}{title}\u{201d}"),
            body: String::new(),
            hint: NO_DAEMON_HINT.to_string(),
            choices: crate::chrome::model::ConfirmChoices::Acknowledge,
        });
        self.mark_chrome_dirty();
    }

    /// Answer the confirm. `Some(true)` closes, `Some(false)` detaches,
    /// `None` cancels; the modal empties either way.
    fn answer_confirm_close(&mut self, close: Option<bool>) {
        let Some(q) = self.confirm_close.take() else { return };
        self.mark_chrome_dirty();
        match close {
            // The tab may have gone while the question was up (its shell
            // exited, or another path closed it); `finish_close_tab` no-ops on
            // an address the strip no longer holds.
            Some(true) => self.finish_close_tab(q.addr, false, false),
            Some(false) => self.finish_close_tab(q.addr, false, true),
            None => {}
        }
    }

    /// Stop watching a tab's session and leave it running (⌘B, #381).
    ///
    /// The whole mechanism is `Drop`: `RemoteSession`'s destructor sends
    /// `Detach` and joins its writer, which is also what closing the window
    /// has always done to every tab.
    fn detach_tab(&mut self, addr: zest_proto::SessionAddr) {
        if addr == crate::tabs::settings_addr() || addr == crate::tabs::profiles_tab_addr() {
            // An app tab has no session; closing it is the only thing that
            // means anything, and ⌘W already does that.
            self.close_tab(addr, false);
            return;
        }
        // An in-process pty has no daemon to leave it with — dropping it
        // hangs the shell up either way. Say so rather than doing the one
        // thing this action promises not to.
        let in_process = self.tabs.iter().find(|t| t.addr == addr).map(|t| {
            (
                matches!(t.source().origin(), Origin::InProcess),
                crate::chrome::model::terminal_label(&t.source().terminal().lock()),
            )
        });
        if let Some((true, title)) = in_process {
            self.refuse_detach(addr, title);
            return;
        }
        self.finish_close_tab(addr, false, true);
    }

    /// Take the tab out of the strip and let go of its session — killing it
    /// only when it is this machine's, alive, and `detach` was not asked for.
    fn finish_close_tab(
        &mut self,
        addr: zest_proto::SessionAddr,
        already_exited: bool,
        detach: bool,
    ) {
        let was_active = self.tabs.is_active(addr);
        let Some(tab) = self.tabs.close(addr) else { return };
        // The map is keyed by address and a closed tab will never be looked
        // at, so nothing else would ever take this entry out.
        self.attention.remove(&addr);
        if already_exited || tab.dead || !tab.local || detach {
            // Dropping detaches (the destructor sends it); a remote session
            // keeps running on its host, which is the point of the fleet.
            drop(tab);
        } else {
            // A local tab closing means "this shell is done" — the opposite
            // default of a remote one, and what every ordinary terminal does.
            tab.kill();
        }

        self.requests.persist = true;
        if self.tabs.is_empty() {
            // The last tab closing closes the window — the old single-session
            // behavior — and the process decides whether that was the last
            // window.
            self.request_close();
            return;
        }
        if was_active {
            self.after_activation();
        } else {
            // Closing a background chip is not an activation: the pane the
            // user is looking at never changed, so after_activation() — whose
            // ensure-visible flag would snap a wheel-scrolled strip back to
            // the active chip mid-close-spree — must not run. The strip still
            // changed shape, so chrome repaints.
            self.mark_chrome_dirty();
        }
        self.relayout_grid();
    }

    /// A session asked to be noticed.
    fn note_attention(
        &mut self,
        addr: zest_proto::SessionAddr,
        cause: zest_proto::AttentionCause,
    ) {
        let enabled = match cause {
            zest_proto::AttentionCause::Bell => self.config.tabs.attention_bell,
            zest_proto::AttentionCause::Notify => self.config.tabs.attention_notify,
        };
        if !enabled || !attention_is_news(self.tabs.is_active(addr), self.focused) {
            return;
        }
        // Last cause wins. Attention is not a log: a shell that rings and then
        // notifies has one thing to show you, and the dot is the same dot.
        self.attention.insert(addr, cause);
        // Its own invalidation, because the dot is chrome. A bare redraw
        // repaints the grid against a cached chrome layout and the dot would
        // never appear — the argument `PairingChanged` already makes.
        self.mark_chrome_dirty();
    }

    /// Forget a tab's unseen signal, because it has now been seen.
    fn clear_attention(&mut self, addr: zest_proto::SessionAddr) {
        if self.attention.remove(&addr).is_some() {
            self.mark_chrome_dirty();
        }
    }

    /// What a closed app tab leaves behind: an activation only if it was the
    /// one being looked at.
    ///
    /// `finish_close_tab` has taken this decision for session tabs all along
    /// (`was_active`, captured before the tab leaves the strip) and writes
    /// down why: closing a *background* chip changed no pane, so
    /// `after_activation`'s ensure-visible flag would snap a wheel-scrolled
    /// strip back to the active chip mid-close-spree and pull the next target
    /// out from under the pointer. The app tabs could not reach that case
    /// while neither drew a × — the only close was ⌘W, which only ever fires
    /// on the active one — so both closes called `after_activation`
    /// unconditionally. Giving them a × is what made the background close
    /// possible, and this is the one copy of the rule they now share.
    fn settled_after_close(&mut self, was_active: bool) {
        if was_active {
            self.after_activation();
        } else {
            self.mark_chrome_dirty();
        }
    }

    /// Housekeeping after the active tab changed.
    fn after_activation(&mut self) {
        // Choosing a session is choosing to look at it: any full-pane screen
        // steps aside. Without this, clicking a sidebar row under the fleet
        // view activated the session *invisibly* — and the only way out of
        // the screen was knowing about Esc.
        self.screen = AppScreen::Terminal;
        // A menu about a block in the tab you just left is stale by
        // definition, and its anchor now names a row of somebody else's grid.
        self.block_menu = None;
        // …and the chip must be in view: activation from the keyboard or the
        // picker can land on a tab the strip has scrolled past.
        self.strip_ensure_visible = true;
        // A drag cannot span a tab switch, and half a selection drag leaking
        // into another tab's grid would.
        self.mouse.release();
        // Looking at it is what "seen" means.
        if let Some(addr) = self.tabs.active().map(|t| t.addr) {
            self.clear_attention(addr);
        }
        let dims = self.current_dims();
        // A split tab's panes are sized by their columns, never the whole
        // grid; `resize_split_panes` knows the columns and skips the panes
        // that already fit.
        let split = self.tabs.active().is_some_and(Tab::is_split);
        if split {
            self.resize_split_panes();
        }
        if let Some(tab) = self.tabs.active_mut() {
            // Background tabs are resized lazily: this is the moment a stale
            // one catches up (RemoteSession::resize also requests the
            // keyframe that makes the new shape true).
            if !split && tab.sized != dims {
                tab.source().resize(dims.0, dims.1);
                tab.sized = dims;
            }
            if let Some(session) = tab.focused_session() {
                session.mark_dirty();
            }
        }
        self.mark_chrome_dirty();
        self.requests.persist = true;
    }

    /// Recompute the grid after the strip's extent may have changed —
    /// opening a second tab or closing back to one moves the grid edge when
    /// `show_single_tab` is off.
    fn relayout_grid(&mut self) {
        if let Some(w) = self.window.as_ref() {
            let size = w.inner_size();
            self.resize_surface(size.width, size.height);
        }
    }

    /// The folded set for the active session, when it has one.
    fn active_folds(&self) -> Option<&std::collections::BTreeSet<u32>> {
        let tab = self.tabs.active()?;
        self.folded_blocks.get(&tab.focused_addr()).filter(|s| !s.is_empty())
    }

    /// The active pane's selected block id, if it has one.
    fn selected_block(&self) -> Option<u32> {
        let tab = self.tabs.active()?;
        self.selected_block.get(&tab.focused_addr()).copied()
    }

    /// Select a block in the active pane, or clear the selection.
    fn set_selected_block(&mut self, id: Option<u32>) {
        let Some(tab) = self.tabs.active() else { return };
        let addr = tab.focused_addr();
        let before = self.selected_block.get(&addr).copied();
        if before == id {
            return;
        }
        match id {
            Some(id) => {
                self.selected_block.insert(addr, id);
            }
            None => {
                self.selected_block.remove(&addr);
            }
        }
        // The rail and wash are grid-layer instances, so the *scene* has to be
        // rebuilt, not just the chrome — marking only the chrome dirty leaves a
        // click on a rail looking like it did nothing.
        if let Some(s) = self.tabs.active_source() {
            s.mark_dirty();
        }
        self.mark_chrome_dirty();
    }

    /// Drop a selection whose block has left the scrollback.
    ///
    /// Eviction, `erase_screen` and a re-anchor all drop blocks outright, so
    /// the id is the only thing that can go stale — and a menu still open on a
    /// block that no longer exists is worse than a stale highlight, so it goes
    /// too. Called once per redraw, which is the only place that already holds
    /// `&mut self` and a lock on the terminal.
    fn prune_selected_block(&mut self) {
        let Some(tab) = self.tabs.active() else { return };
        let addr = tab.focused_addr();
        let Some(&id) = self.selected_block.get(&addr) else { return };
        let Some(session) = tab.focused_session() else { return };
        let alive = {
            let term = session.terminal();
            let term = term.lock();
            term.blocks().get(zest_core::BlockId(id)).is_some()
        };
        if !alive {
            self.selected_block.remove(&addr);
            self.block_menu = None;
            self.mark_chrome_dirty();
        }
    }

    /// The block a block action targets.
    ///
    /// The selected one while there is one, else the most recent block *with
    /// output*. That fallback is the whole of the old behaviour and stays the
    /// behaviour of a session nobody has clicked in — at a prompt the cursor's
    /// own block has printed nothing, so "the block I am in" would copy air.
    fn target_block(&self, term: &zest_core::Terminal) -> Option<zest_core::Block> {
        self.selected_block()
            .and_then(|id| term.blocks().get(zest_core::BlockId(id)).cloned())
            .or_else(|| block_actions::last_with_output(term))
    }

    /// The rail-and-wash bands for one pane's blocks, in absolute line ids.
    ///
    /// Line ids rather than rows because these are consumed by the grid layer,
    /// which resolves rows through the fold map anyway: a folded block's output
    /// lines are simply never asked about, a scroll moves the bands with the
    /// text, and a block whose header has scrolled off the top still rails the
    /// rows that remain. All four fall out; none is a case here.
    ///
    /// Skips a block still at its prompt, matching the header pass — that is
    /// where the user is typing, and decorating it would draw a finished-looking
    /// frame around a half-typed command.
    ///
    /// A free function rather than a method: the call sites sit inside the
    /// redraw's mutable borrow of the GPU, and it needs nothing from `App`
    /// beyond the colours.
    fn block_bands(
        c: &ChromeColors,
        term: &zest_core::Terminal,
        selected: Option<u32>,
        dead: bool,
    ) -> Vec<zest_render_wgpu::BlockBand> {
        if term.in_alt_screen() {
            return Vec::new();
        }
        // A running block ends at the *cursor*, not at the bottom of the
        // viewport. `block_actions`' own `last_line` uses the bottom row, and
        // that is right for copying — `selection_text` trims the blanks — but
        // a rail cannot trim: taking the bottom row drew one running command's
        // rail down the whole empty half of the window.
        //
        // Active space either way, so the rail does not end wherever the user
        // happened to have scrolled to.
        let grid = term.grid();
        let last_line = grid
            .active_line_id_at(term.cursor().row.min(grid.rows().saturating_sub(1)))
            .unwrap_or(0);
        let mut bands: Vec<zest_render_wgpu::BlockBand> = term
            .blocks()
            .blocks()
            .iter()
            .filter_map(|b| {
                let out_line = b.output_line?;
                let running = b.is_running() && !dead;
                let rail = if b.is_running() && dead {
                    c.text_faint
                } else if running {
                    c.warn
                } else if b.failed() {
                    c.danger
                } else if b.end_line.is_some_and(|e| e < out_line) {
                    c.text_faint
                } else {
                    c.success
                };
                let is_selected = selected == Some(b.id.0);
                let to = b.end_line.map_or(last_line + 1, |e| e + 1);
                let header_to = header_span(b.prompt_line, b.output_line, to);
                Some(zest_render_wgpu::BlockBand {
                    from: b.prompt_line,
                    // Inclusive `end_line`, so `+ 1` converts to the half-open
                    // range the row test wants — the same conversion, and for
                    // the same reason, as `block_actions::fold_range`.
                    //
                    // A running block ends at the newest line the grid holds,
                    // not at infinity: an open-ended range railed every blank
                    // row below the output too, so one running command drew a
                    // rail to the bottom of the window and the block looked
                    // like it owned the rest of the session.
                    to,
                    header_to,
                    rail: if is_selected { c.accent } else { rail },
                    // The block's own edges, now that the header is not a
                    // surface: a breath of the state colour under the output,
                    // 10% of the accent when this is the selected block. See
                    // `state_wash` for why the first of those is solved per
                    // theme rather than being the design's flat 4%.
                    //
                    // `None` for a block that printed nothing: its rows are all
                    // header, and the rail alone says it ran.
                    wash: (header_to < to).then(|| {
                        if is_selected {
                            crate::chrome::layout::washed(c.accent, SELECTED_WASH)
                        } else {
                            state_wash(rail, c)
                        }
                    }),
                })
            })
            .collect::<Vec<_>>();
        // The renderer binary-searches these, which is only correct while they
        // are ascending and disjoint. That holds because `blocks()` is
        // chronological and a block's rows cannot overlap its neighbour's —
        // but it is a *precondition* of the search rather than something the
        // search can check, so a stale wire upsert or a bad re-anchor must not
        // be able to break it silently. Dropping the offender keeps the rails
        // honest; the next upsert corrects it.
        bands.dedup_by(|b, prev| {
            let bad = b.from < prev.to;
            if bad {
                tracing::debug!(from = b.from, prev_to = prev.to, "overlapping block bands");
            }
            bad
        });
        bands
    }

    /// A visual (clicked) row to the line it shows, through the fold view
    /// when one is active. What keeps selection, ⌘⇧-click and the renderer
    /// reading the same row list.
    fn visual_line_at(&self, term: &zest_core::Terminal, row: usize) -> Option<u64> {
        match self.active_folds().and_then(|f| block_actions::fold_row_map(term, f)) {
            Some(map) => {
                let idx = *map.get(row.min(map.len().saturating_sub(1)))?;
                (idx != usize::MAX).then(|| term.grid().line(idx).map(|r| r.id)).flatten()
            }
            None => {
                let grid = term.grid();
                grid.line_id_at(row.min(grid.rows().saturating_sub(1)))
            }
        }
    }

    /// [`zest_core::Terminal::abs_pos`], but through the fold view.
    fn visual_abs_pos(
        &self,
        term: &zest_core::Terminal,
        row: usize,
        col: usize,
    ) -> Option<zest_core::AbsPos> {
        let line = self.visual_line_at(term, row)?;
        let cols = term.grid().cols();
        Some(zest_core::AbsPos::new(line, col.min(cols.saturating_sub(1))))
    }

    /// One pane's blocks as the header pass wants them: which viewport rows
    /// each header covers, plus its state and pre-formatted labels. One short
    /// terminal lock; plain data out.
    ///
    /// Per pane, not per tab: a split tab draws headers in every pane (#460),
    /// so this reads `pane` throughout — its own source, its own address for
    /// the selection, its own liveness. `pane == tab.focus` for an unsplit
    /// tab, where the two were the same thing.
    ///
    /// Empty while a screen owns the pane — see [`pane_is_covered`].
    fn build_block_views(&self, pane: usize) -> Vec<crate::chrome::blocks::BlockView> {
        if pane_is_covered(self.screen, self.tabs.app_tab_active()) {
            return Vec::new();
        }
        let Some(tab) = self.tabs.active() else { return Vec::new() };
        // A file pane has no blocks: the headers, rails and fold chevrons this
        // builds are all facts a shell told us, and a file told us none.
        let Some(session) = tab.pane_session(pane) else { return Vec::new() };
        let pane_dead = tab.pane_dead(pane);
        let term = session.terminal();
        let term = term.lock();
        // The alt screen is a separate grid whose ids restart at zero; a
        // primary-grid block would overlay whatever rows happen to collide.
        if term.in_alt_screen() {
            return Vec::new();
        }
        let grid = term.grid();
        let addr = tab.pane_addr(pane);
        let folded = self.folded_blocks.get(&addr);
        // Through the fold view when one is active, so a header sits on the
        // rows the renderer actually draws, not the ones it hid.
        let fold_map = folded.and_then(|f| block_actions::fold_row_map(&term, f));
        let row_lines: Vec<Option<u64>> = match &fold_map {
            Some(map) => map
                .iter()
                .map(|&i| (i != usize::MAX).then(|| grid.line(i).map(|r| r.id)).flatten())
                .collect(),
            None => (0..grid.rows()).map(|r| grid.line_id_at(r)).collect(),
        };
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let selected = self.selected_block.get(&addr).copied();
        // What the prompt's own `ChipKind::Cwd` chip already says, live and
        // per-prompt. A header repeats it only where it *differs* — a block
        // that ran somewhere else — which is the one case worth the room; the
        // pane header above carries the session's, so printing it again on
        // every header down the scrollback is most of the "noisy metadata"
        // complaint by itself.
        let session_cwd = crate::status::shorten_home(term.cwd());
        // The menu belongs to the focused pane: it is opened by a pointer, and
        // only the focused pane's headers answer one. Left unscoped, a block
        // sharing an id with the menu's would hold a ⋯ open in a pane nothing
        // can click.
        let menu_block = (pane == tab.focus).then(|| self.block_menu.as_ref().map(|m| m.block)).flatten();

        let views = term
            .blocks()
            .blocks()
            .iter()
            .filter_map(|b| {
                // A block still at its prompt is where the user is typing;
                // never overlaid.
                let out_line = b.output_line?;
                let header = (b.prompt_line, out_line.max(b.prompt_line + 1));
                // The *first contiguous run* of matching rows, not the span
                // from the first match to the last. Taking min and max meant a
                // block naming a wide id range -- which a corrupt index can
                // produce -- covered every row between its two ends, and the
                // header band is opaque and eats clicks. One bad block would
                // then paint over the whole pane and, through the overlap
                // de-dup below, delete every other header with it.
                let mut first = None;
                let mut last = None;
                for (r, line) in row_lines.iter().enumerate() {
                    if line.is_some_and(|l| l >= header.0 && l < header.1) {
                        first.get_or_insert(r);
                        last = Some(r + 1);
                    } else if first.is_some() {
                        break;
                    }
                }
                let rows = (first?, last?);
                // The click target is the whole block, header and output: the
                // rail is what makes a block's *extent* pointable, and a
                // one-row header never could. Same scan, widened to the
                // block's end — and clipped by construction, since it only
                // ever names rows the fold view actually drew.
                let mut hit_last = rows.1;
                for (r, line) in row_lines.iter().enumerate().skip(rows.1) {
                    let inside = line.is_some_and(|l| {
                        l >= header.0 && b.end_line.is_none_or(|e| l <= e)
                    });
                    if inside {
                        hit_last = r + 1;
                    } else {
                        break;
                    }
                }
                let is_folded = folded.is_some_and(|f| f.contains(&b.id.0));
                let running = b.is_running();
                let duration = match (b.started_ms, b.ended_ms) {
                    (Some(s), Some(e)) if !running => {
                        crate::chrome::blocks::format_duration(e.saturating_sub(s))
                    }
                    _ => String::new(),
                };
                let running_label = if running {
                    b.started_ms.map_or_else(
                        || "running".to_string(),
                        |s| {
                            format!(
                                "running {:.1}s",
                                now_ms.saturating_sub(s) as f64 / 1000.0
                            )
                        },
                    )
                } else {
                    String::new()
                };
                // Inclusive ranges: a one-line output has end == out, so
                // "printed nothing" is strictly-before.
                let no_output = !running && b.end_line.is_some_and(|e| e < out_line);
                let exit_label = match b.state {
                    // Success prints nothing. A green rail and a green command
                    // already say "exit 0", and a screen of finished blocks
                    // each restating it is noise; a *failure* is the only exit
                    // code anyone reads, so that one keeps its words.
                    zest_core::BlockState::Finished { exit_code: Some(c) } if c != 0 => {
                        format!("exit {c}")
                    }
                    _ => String::new(),
                };
                let cwd = crate::status::shorten_home(&b.cwd);
                Some(crate::chrome::blocks::BlockView {
                    id: b.id.0,
                    branch: b.context.as_ref().map(|c| c.branch.clone()).unwrap_or_default(),
                    // Empty when we ran it ourselves, which is the common case
                    // and the one that should take no room.
                    author: crate::chrome::blocks::author_label(
                        b.author,
                        self.client_identity.as_ref().map(|i| i.client_id()),
                    ),
                    rows,
                    // A block still "running" in a session whose host went
                    // away is not running anywhere; the rail says so.
                    interrupted: running && pane_dead,
                    running: running && !pane_dead,
                    failed: b.failed(),
                    no_output,
                    command: b.command.clone(),
                    cwd: if cwd == session_cwd { String::new() } else { cwd },
                    duration,
                    exit_label,
                    running_label,
                    folded: is_folded,
                    foldable: block_actions::fold_range(b).is_some(),
                    folded_lines: b
                        .end_line
                        .map_or(0, |e| (e + 1).saturating_sub(out_line) as usize),
                    hit_rows: (rows.0, hit_last),
                    selected: selected == Some(b.id.0),
                    menu_open: menu_block == Some(b.id.0),
                })
            })
            .collect::<Vec<_>>();

        // Around a reflow, a stale wire upsert can briefly hand two blocks
        // overlapping row ranges — and two headers double-printing on one
        // row is worse than either alone. One row, one header: the newer
        // block keeps it (ids only grow), the older one waits for the
        // corrected upsert.
        let mut kept: Vec<crate::chrome::blocks::BlockView> = Vec::with_capacity(views.len());
        for v in views.into_iter().rev() {
            if kept.iter().all(|k| v.rows.1 <= k.rows.0 || v.rows.0 >= k.rows.1) {
                kept.push(v);
            }
        }
        kept.reverse();
        kept
    }

    /// The live prompt's context chips (#420), extracted for this frame.
    ///
    /// Anchored to the tail block still at `Prompt` — the one
    /// [`Self::build_block_views`] deliberately skips — and filled from the
    /// session's own daemon's word (`SessionInfo.context`, off the fleet
    /// listing), so this window and a phone looking at the same session show
    /// the same chips. An in-process session has no daemon to ask; it falls
    /// back to what its own terminal parsed (cwd, shell facts), which are
    /// the same facts one layer earlier.
    fn build_prompt_chips(&self) -> Option<crate::chrome::prompt_chips::PromptChipsView> {
        use crate::chrome::prompt_chips::{Chip, ChipKind, PromptChipsView};

        let widgets = &self.settings.prompt.widgets;
        if widgets.is_empty() || pane_is_covered(self.screen, self.tabs.app_tab_active()) {
            return None;
        }
        let tab = self.tabs.active()?;
        // A prompt is a shell's; a pane holding a file has none, and chips
        // describing the tab's *other* pane over an open file would be the
        // wrong answer confidently drawn (#464).
        let session = tab.focused_session()?;
        let addr = tab.focused_addr();
        let context = self.shared.fleet.get().and_then(|f| f.session_context(addr)).or_else(|| {
            // An in-process session has no daemon listing to carry its
            // context — but this window *is* its host, so it probes like one
            // (#434): the same engine, the same filesystem, the same trust
            // posture. Gated on the origin, never on the listing's absence:
            // a daemon-backed session whose listing has not arrived yet must
            // stay blank rather than have its cwd probed against the local
            // disk, which is the ssh trap wearing a race's clothes.
            if !matches!(session.origin(), crate::source::Origin::InProcess) {
                return None;
            }
            let engine = self.shared.local_context.get_or_init(|| {
                // A no-op change callback on purpose: the chip row is
                // rebuilt per frame and the cursor blink repaints anyway,
                // so an async answer (the dirty star) surfaces within a
                // blink without a wakeup plumbed through.
                zest_daemon::context::ContextEngine::new(std::sync::Arc::new(|| {}))
            });
            let term = session.terminal();
            let term = term.lock();
            // The daemon's own merge, not a re-derivation: the probe half
            // plus the terminal's shell facts, with one copy of the rules
            // (labels, the nvm→node replacement).
            zest_daemon::context::with_shell_facts(
                engine.context_for(term.cwd(), term.cwd_host()),
                term.shell_facts().iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                0,
            )
        });

        let term = session.terminal();
        let term = term.lock();
        if term.in_alt_screen() {
            return None;
        }
        let tail = term.blocks().last()?;
        if tail.output_line.is_some() {
            return None;
        }
        let grid = term.grid();
        // Through the fold view when one is active, for the same reason the
        // headers go through it: the chips must sit on rows the renderer
        // actually draws.
        let folded = self.folded_blocks.get(&addr);
        let fold_map = folded.and_then(|f| block_actions::fold_row_map(&term, f));
        let prompt_row = match &fold_map {
            Some(map) => map.iter().position(|&i| {
                i != usize::MAX && grid.line(i).is_some_and(|r| r.id == tail.prompt_line)
            })?,
            None => grid.row_of_line(tail.prompt_line)?,
        };

        let occupied = grid
            .row(prompt_row)
            .cells()
            .iter()
            .rposition(|c| !c.is_blank())
            .map_or(0, |i| i + 1);
        // The caret counts as occupied: the collision rule protects where
        // the user is about to type, not only what they already have.
        let caret = if grid.cursor.row == prompt_row { grid.cursor.col + 1 } else { 0 };
        let row_above_blank = prompt_row > 0
            && grid.row(prompt_row - 1).cells().iter().all(zest_core::Cell::is_blank);

        let cwd = if term.cwd().is_empty() { tail.cwd.clone() } else { term.cwd().to_string() };
        let shell_facts = term.shell_facts();
        let fact = |key: &str| -> Option<&str> {
            match &context {
                Some(ctx) => {
                    ctx.facts.iter().find(|f| f.key == key).map(|f| f.value.as_str())
                }
                // The in-process fallback: the window is the host, so its
                // own parse *is* the daemon-grade source for shell facts.
                None => shell_facts.get(key).map(String::as_str),
            }
        };

        let mut chips = Vec::new();
        for name in widgets {
            let chip = match name.as_str() {
                "cwd" if !cwd.is_empty() => Some(Chip {
                    kind: ChipKind::Cwd,
                    value: cwd.clone(),
                    label: crate::status::shorten_home(&cwd),
                }),
                "git" => context.as_ref().and_then(|c| c.git.as_ref()).map(|g| {
                    // `main`, then `main*` once the probe answers dirty,
                    // then `main* ±3` once it counts — arriving in that
                    // order, which is the honest one (#432).
                    let mut label = g.branch.clone();
                    if g.dirty == Some(true) {
                        label.push('*');
                        if let Some(n) = g.changed {
                            label.push_str(&format!(" ±{n}"));
                        }
                    }
                    Chip { kind: ChipKind::Git, value: g.branch.clone(), label }
                }),
                "venv" => fact("venv").map(|v| Chip {
                    kind: ChipKind::Venv,
                    value: v.to_string(),
                    label: format!("venv {v}"),
                }),
                "conda" => fact("conda").map(|v| Chip {
                    kind: ChipKind::Conda,
                    value: v.to_string(),
                    label: format!("conda {v}"),
                }),
                "kube" => fact("kube").map(|v| Chip {
                    kind: ChipKind::Kube,
                    value: v.to_string(),
                    label: format!("kube {v}"),
                }),
                "aws" => fact("aws_profile").map(|v| Chip {
                    kind: ChipKind::Aws,
                    value: v.to_string(),
                    label: format!("aws {v}"),
                }),
                "node" => fact("node").map(|v| Chip {
                    kind: ChipKind::Node,
                    value: v.to_string(),
                    label: format!("node {v}"),
                }),
                "ssh" => fact("ssh_host").map(|v| Chip {
                    kind: ChipKind::Ssh,
                    value: v.to_string(),
                    label: format!("ssh {v}"),
                }),
                // The link, this window's own fact (#432): shown only when
                // there is a link worth naming — loopback would be a "local
                // 0.0 ms" chip, noise wearing a number.
                "link" => self.shared.fleet.get().and_then(|f| f.link_of(addr.host)).and_then(
                    |(reach, rtt)| {
                        let name = match reach {
                            zest_mesh::Reachability::Loopback => return None,
                            zest_mesh::Reachability::Lan => "lan",
                            zest_mesh::Reachability::Cloud => "relay",
                        };
                        let label = match rtt {
                            Some(ms) => {
                                format!("{name} {}", crate::chrome::layout::format_ms(ms))
                            }
                            None => name.to_string(),
                        };
                        Some(Chip { kind: ChipKind::Link, value: label.clone(), label })
                    },
                ),
                // Only a *failure* earns a chip, and only when the most
                // recently finished block is the failure: an exit 1 three
                // commands behind a success is history, and the blocks
                // already tell it. Two steps, not one `find_map` — a
                // `find_map` that skips a successful tail would walk on and
                // resurrect exactly that old failure.
                "exit" => term
                    .blocks()
                    .blocks()
                    .iter()
                    .rev()
                    .find(|b| matches!(b.state, zest_core::BlockState::Finished { .. }))
                    .and_then(|b| match b.state {
                        zest_core::BlockState::Finished { exit_code } => {
                            exit_code.filter(|&c| c != 0).map(|code| Chip {
                                kind: ChipKind::Exit,
                                value: b.id.0.to_string(),
                                label: format!("exit {code}"),
                            })
                        }
                        _ => None,
                    }),
                _ => None,
            };
            chips.extend(chip);
        }
        if chips.is_empty() {
            return None;
        }
        Some(PromptChipsView { prompt_row, row_above_blank, occupied_cols: occupied.max(caret), chips })
    }

    /// Pointer pixels to a grid cell, clamped into the viewport — through
    /// the focused pane's rectangle when the tab is split.
    fn cell_at(&self, x: f64, y: f64) -> (usize, usize) {
        let Some(fonts) = self.fonts.as_ref() else { return (0, 0) };
        let m = fonts.cell_metrics();
        let rect = self.focused_view_rect().unwrap_or_else(|| {
            let insets = self.insets();
            [insets.left, insets.top, 0.0, 0.0]
        });
        let col = ((x - f64::from(rect[0])).max(0.0) / f64::from(m.cell_w)) as usize;
        let row = ((y - f64::from(rect[1])).max(0.0) / f64::from(m.cell_h)) as usize;

        let Some(session) = self.tabs.active_source() else { return (row, col) };
        let term = session.terminal().lock();
        let grid = term.grid();
        (row.min(grid.rows().saturating_sub(1)), col.min(grid.cols().saturating_sub(1)))
    }


    /// Rasterize printable ASCII in all four styles before the first frame.
    ///
    /// Roughly 380 glyphs and a couple of milliseconds. Without it, the first
    /// frame containing a prompt pays to rasterize every character in it, which
    /// lands as a visible hitch immediately after the window appears — and then
    /// again the first time anything bold or italic shows up.
    fn prewarm_atlas(&mut self) {
        let (Some(gpu), Some(fonts)) = (self.gpu.as_mut(), self.fonts.as_mut()) else {
            return;
        };
        let started = std::time::Instant::now();

        for style in [
            zest_font::Style::new(false, false),
            zest_font::Style::new(true, false),
            zest_font::Style::new(false, true),
            zest_font::Style::new(true, true),
        ] {
            for ch in ' '..='~' {
                let Some((font, glyph)) = fonts.glyph_for(ch, style) else { continue };
                let key = fonts.key(font, glyph);
                if gpu.renderer.atlas.get(&key).is_some() {
                    continue;
                }
                if let Some(image) = fonts.rasterize(key) {
                    gpu.renderer
                        .atlas
                        .insert(&gpu.device, &gpu.queue, key, &image);
                }
            }
        }

        tracing::debug!(
            glyphs = gpu.renderer.atlas.len(),
            elapsed_ms = started.elapsed().as_millis(),
            "atlas pre-warmed"
        );
    }

    pub(crate) fn redraw(&mut self) {
        let insets = self.insets();
        self.refresh_chrome();
        // Extracted under its own short lock, before the borrows below: the
        // views are plain data, and holding the terminal across atlas work
        // would stall the reader thread.
        // Before the views: a selection whose block has been evicted must not
        // reach the band builder, which would rail nothing and light nothing
        // while the menu still claimed to be about it.
        self.prune_selected_block();
        // Every pane's, not just the focused one's: a split tab draws the
        // block state in all of them (#460). The rectangles come from the same
        // list `focused_view_rect` indexes, so a header cannot land one
        // letterbox-offset away from the glyphs it sits on.
        let pane_rects = self.pane_view_rects();
        let focus = self.tabs.active().map_or(0, |t| t.focus);
        let block_views: Vec<Vec<crate::chrome::blocks::BlockView>> =
            (0..pane_rects.len()).map(|i| self.build_block_views(i)).collect();
        self.prompt_chips_view = self.build_prompt_chips();
        // Any pane, or an unfocused pane's running ring freezes mid-turn.
        self.anim_spin = block_views.iter().flatten().any(|v| v.running);
        let anim = self.anim_phase();
        let caret_on = anim.caret_on;
        // Per pane too, and for the same reason the views are: a fold is
        // stored per session address, so a pane folded and then unfocused
        // would otherwise have its grid drawn unfolded while its headers were
        // placed through the fold view — the band on the wrong rows.
        // Nothing reads these while a screen owns the pane: no terminal is
        // built at all then, and `build_block_views` has already returned
        // empty for the same reason. `fold_row_map` walks the grid, so this is
        // the one of the two that is worth not doing per frame behind Settings.
        let fold_maps: Vec<Option<Vec<usize>>> = self
            .tabs
            .active()
            .filter(|_| !pane_is_covered(self.screen, self.tabs.app_tab_active()))
            .map(|t| {
                (0..pane_rects.len())
                    .map(|i| {
                        let folds =
                            self.folded_blocks.get(&t.pane_addr(i)).filter(|s| !s.is_empty())?;
                        // A file pane folds nothing: folds are per block, and
                        // a file has none. `None` keeps its slot in the
                        // index-parallel vector.
                        let term = t.pane_session(i)?.terminal();
                        let term = term.lock();
                        block_actions::fold_row_map(&term, folds)
                    })
                    .collect()
            })
            .unwrap_or_default();
        // What the window is painted with outside every viewport: the padding,
        // the gaps around the chrome bars, the split gutter. Taken from the
        // app's palette rather than a session's, because those pixels belong to
        // no session -- and computed here, before the borrows below.
        //
        // Which opacity depends on what owns the pane. `grid_area` excludes
        // `window.padding` on all four sides, so while a screen is up the ring
        // around it is the only thing this still paints there -- and a screen
        // has no grid under it, which makes that ring chrome rather than the
        // terminal's margin. At `window.opacity` it reads as a frame in the
        // wrong alpha around a glass Settings. Answered here rather than by
        // growing the screen's rect: a grown surface rect would overlap the
        // strip's own and erase it, replace not composite (#538).
        let backdrop = {
            let bg = self.palette.background;
            let alpha = if pane_is_covered(self.screen, self.tabs.app_tab_active()) {
                self.config.chrome_opacity
            } else {
                self.config.opacity
            };
            zest_render_wgpu::LinearRgba::from_srgb(bg.r, bg.g, bg.b, alpha)
        };
        // Copied out for the same reason as `backdrop`: the block bands are
        // built inside the borrows below, and `ChromeColors` is `Copy`.
        let band_colors = self.chrome_colors;
        // Where the cursor should be drawn, in cells. The grid's cursor is
        // where the *program* put it; this is only where the caret has caught
        // up to, and it exists at all only while `cursor.trail` is on.
        let cursor_at = self
            .tabs
            .active_source()
            .map(|s| {
                let c = s.terminal().lock().grid().cursor;
                (c.col as f32, c.row as f32)
            })
            .unwrap_or((0.0, 0.0));
        let cursor_offset = if self.config.cursor_trail && self.motion_allowed() {
            match self.cursor_trail.as_mut() {
                // Retarget rather than rebuild, so a cursor that moves again
                // mid-flight bends toward the new cell with the velocity it
                // already had -- the whole reason this is a spring.
                Some((x, y)) => {
                    x.retarget(cursor_at.0);
                    y.retarget(cursor_at.1);
                    (x.value() - cursor_at.0, y.value() - cursor_at.1)
                }
                // First frame with a trail: start *on* the cursor, or the caret
                // would fly in from wherever the origin happened to be.
                None => {
                    let mut x = crate::motion::Spring::at(cursor_at.0);
                    let mut y = crate::motion::Spring::at(cursor_at.1);
                    x.retarget(cursor_at.0);
                    y.retarget(cursor_at.1);
                    self.cursor_trail = Some((x, y));
                    (0.0, 0.0)
                }
            }
        } else {
            self.cursor_trail = None;
            (0.0, 0.0)
        };

        let selected_blocks = self.selected_block.clone();
        let predict_policy = self.predict_policy();
        // Deliberately *not* taking the focused session here (#464). It used
        // to be part of this tuple, which was invisible while every pane was a
        // shell — `active_source` was only `None` on an empty strip. The
        // moment a focused pane can hold a file it is `None` for an ordinary
        // window, and this returned before drawing **anything**: not the file,
        // not the other panes, not the chrome, and not the screenshot this
        // frame was for. The one arm that needs a session takes it there.
        // Resolved before the `&mut` borrows below, and from the fields
        // directly rather than through `find_highlights(&self)`: a method
        // taking all of `self` would collide with `gpu`/`fonts`, where these
        // three fields are disjoint from them.
        let find_hl = (self.find.is_some() && !self.find_state.hits.is_empty()).then(|| {
            zest_render_wgpu::FindHighlights {
                matches: &self.find_state.hits,
                current: self.find_state.current,
                // No new theme token: one would move the JSON schema, all five
                // built-in themes and `check-export-web` for a colour the
                // selection already has. The current hit takes the accent, so
                // "the one I am on" reads without a legend.
                bg: self.selection_bg,
                current_bg: self.accent_bg,
            }
        });
        let find_open = self.find.is_some();
        let (Some(gpu), Some(fonts), Some(window)) =
            (self.gpu.as_mut(), self.fonts.as_mut(), self.window.as_ref())
        else {
            return;
        };

        let metrics = fonts.cell_metrics();

        // Resolved once, before the viewports, for the same reason `backdrop`
        // is: the pane loop must not do file work. `Backgrounds::get` is a hash
        // lookup after the first sight of a path in this config generation, so
        // this costs nothing on the steady-state frame -- and nothing at all
        // for the overwhelming majority of windows, which name no picture.
        let background = {
            let identity = self.tabs.active().and_then(|t| t.identity.as_ref());
            let path = identity
                .and_then(|i| i.background_image.as_deref())
                .unwrap_or(self.config.background_image.as_str());
            let fit =
                identity.and_then(|i| i.background_fit).unwrap_or(self.config.background_fit);
            let dim = identity.and_then(|i| i.background_dim).unwrap_or(self.config.background_dim);
            self.backgrounds
                .get(&gpu.device, &gpu.queue, &mut gpu.renderer.images, path)
                .map(|(image, size)| zest_render_wgpu::BackgroundImage {
                    image,
                    size,
                    fit: crate::background::fit_of(fit),
                    dim,
                })
        };

        let cursor_offset_px =
            [cursor_offset.0 * metrics.cell_w as f32, cursor_offset.1 * metrics.cell_h as f32];

        // The smooth-scroll spring, in pixels. The renderer folds this into
        // the grid's own origin, so every grid pass moves together and the
        // chrome — which is not the grid's — simply never sees it.
        let scroll_px = self.scroll_spring.value() * metrics.cell_h as f32;

        // Chrome instances: rectangles come finished from the layout pass;
        // text runs resolve against the atlas here, where the GPU lives.
        // Assembled in layer order — cached base, block headers, then the
        // cached overlay — so the renderer's overlay split lands between
        // "chrome the panels cover" and "the panels themselves".
        let mut chrome = Chrome::default();
        let (overlay_rects, overlay_texts) = self
            .chrome_layout
            .as_ref()
            .map_or((0, 0), |l| (l.overlay_rects_at, l.overlay_texts_at));
        if let Some(layout) = self.chrome_layout.as_ref() {
            // Outside the base/overlay split on purpose: a surface is drawn
            // before every other instance, so it has no overlay half to sort
            // into.
            chrome.surface_rects.extend_from_slice(&layout.surface_rects);
            chrome.rects.extend_from_slice(&layout.rects[..overlay_rects]);
            for run in &layout.texts[..overlay_texts] {
                zest_render_wgpu::emit_ui_run(
                    &gpu.device,
                    &gpu.queue,
                    &mut gpu.renderer.atlas,
                    fonts,
                    &run.text,
                    zest_font::Style::new(run.bold, false),
                    run.px,
                    run.tracking,
                    run.pos,
                    run.color,
                    run.clip,
                    run.max_width,
                    &mut chrome.glyphs,
                );
            }
        }

        // Block headers ride the scrollback, so unlike the cached layout they
        // are rebuilt per frame — pure arithmetic over the views above.
        {
            let fallback = insets.grid_rect(gpu.config.width, gpu.config.height);
            let area = pane_rects.get(focus).copied().unwrap_or(fallback);
            let scale = window.scale_factor() as f32;
            // Every pane draws its headers; only the focused one's are
            // interactive. An unfocused pane's whole frame is already a single
            // click-to-focus target (`layout::panes_overlay`), so merging its
            // hit map would put block regions under a pointer that must not
            // reach them — and block ids are per session, so two panes can
            // name the same one.
            // The focused pane's, kept past the loop: it is the only one whose
            // regions answer a pointer.
            let mut block_chrome = crate::chrome::blocks::BlockChrome::default();
            for (i, views) in block_views.iter().enumerate() {
                if views.is_empty() {
                    continue;
                }
                let focused = i == focus;
                let pane_chrome = {
                    let mut measure = |t: &str, px: f32, bold: bool, tr: f32| {
                        zest_render_wgpu::measure_ui_run(
                            fonts,
                            t,
                            zest_font::Style::new(bold, false),
                            px,
                            tr,
                        )
                    };
                    crate::chrome::blocks::layout_blocks(
                        views,
                        pane_rects.get(i).copied().unwrap_or(fallback),
                        metrics.cell_h as f32,
                        scale,
                        &self.chrome_colors,
                        // Hover is the pointer's, and the pointer is the
                        // focused pane's. Passing it on would light a header
                        // in another pane that shares the hovered block's id.
                        if focused { self.chrome_hover } else { None },
                        anim.spin,
                        &mut measure,
                    )
                };
                chrome.rects.extend_from_slice(&pane_chrome.rects);
                for run in &pane_chrome.texts {
                    zest_render_wgpu::emit_ui_run(
                        &gpu.device,
                        &gpu.queue,
                        &mut gpu.renderer.atlas,
                        fonts,
                        &run.text,
                        zest_font::Style::new(run.bold, false),
                        run.px,
                        run.tracking,
                        run.pos,
                        run.color,
                        run.clip,
                        run.max_width,
                        &mut chrome.glyphs,
                    );
                }
                if focused {
                    block_chrome = pane_chrome;
                }
            }
            self.block_hits = block_chrome.hit;
            // Kept only while the ⋯ it names is still drawn: an anchor left
            // over from a previous frame would hang the menu off a header
            // that has since scrolled away.
            self.block_menu_anchor = block_chrome.menu_anchor;

            // The live prompt's chips, over the block chrome — they never
            // share rows with a header (headers skip the live prompt), so
            // the order only decides who wins a bug.
            let chip_chrome = match self.prompt_chips_view.as_ref() {
                Some(view) => {
                    let mut measure = |t: &str, px: f32, bold: bool, tr: f32| {
                        zest_render_wgpu::measure_ui_run(
                            fonts,
                            t,
                            zest_font::Style::new(bold, false),
                            px,
                            tr,
                        )
                    };
                    crate::chrome::prompt_chips::layout_prompt_chips(
                        view,
                        area,
                        metrics.cell_w as f32,
                        metrics.cell_h as f32,
                        scale,
                        &self.chrome_colors,
                        self.chrome_hover,
                        &mut measure,
                    )
                }
                None => crate::chrome::prompt_chips::ChipChrome::default(),
            };
            chrome.rects.extend_from_slice(&chip_chrome.rects);
            for run in &chip_chrome.texts {
                zest_render_wgpu::emit_ui_run(
                    &gpu.device,
                    &gpu.queue,
                    &mut gpu.renderer.atlas,
                    fonts,
                    &run.text,
                    zest_font::Style::new(run.bold, false),
                    run.px,
                    run.tracking,
                    run.pos,
                    run.color,
                    run.clip,
                    run.max_width,
                    &mut chrome.glyphs,
                );
            }
            self.chip_hits = chip_chrome.hit;
        }

        // The overlay layer last: its panels must cover every glyph above.
        chrome.overlay_rects_at = chrome.rects.len();
        chrome.overlay_glyphs_at = chrome.glyphs.len();
        if let Some(layout) = self.chrome_layout.as_ref() {
            chrome.rects.extend_from_slice(&layout.rects[overlay_rects..]);
            for run in &layout.texts[overlay_texts..] {
                zest_render_wgpu::emit_ui_run(
                    &gpu.device,
                    &gpu.queue,
                    &mut gpu.renderer.atlas,
                    fonts,
                    &run.text,
                    zest_font::Style::new(run.bold, false),
                    run.px,
                    run.tracking,
                    run.pos,
                    run.color,
                    run.clip,
                    run.max_width,
                    &mut chrome.glyphs,
                );
            }
        }

        // Build the frame FIRST, and only then acquire the swapchain texture.
        //
        // `get_current_texture` blocks until the presentation engine hands one
        // over. Acquiring first would spend that wait doing nothing and then run
        // all the CPU work afterwards, pushing past the vblank deadline. This
        // ordering overlaps the CPU work with the wait and is the single
        // highest-leverage latency trick in the renderer.
        {
            let area = insets.grid_rect(gpu.config.width, gpu.config.height);
            // `None` while a screen owns the pane: the terminal is then not
            // built at all, rather than built and painted over. The outer
            // `Option` also keeps the terminal lock untaken in that case.
            let split = (!pane_is_covered(self.screen, self.tabs.app_tab_active()))
                .then(|| self.tabs.active().filter(|t| t.is_split()).map(Tab::pane_count));
            match split {
                None => {
                    // A screen replaces the terminal; drawing it underneath
                    // leaks a pixel of it around the screen's own antialiased
                    // edge (see `pane_is_covered`).
                    self.scene.build(
                        &gpu.device,
                        &gpu.queue,
                        &mut gpu.renderer.atlas,
                        fonts,
                        metrics,
                        backdrop,
                        &[],
                        &chrome,
                    );
                }
                Some(Some(n)) => {
                    // N panes, N grids, one build — the slice the renderer
                    // took from day one (CONTRACTS, "cheap now" #3) finally
                    // carries as many elements as the tab has panes (#436).
                    let scale =
                        self.window.as_ref().map_or(1.0, |w| w.scale_factor() as f32);
                    let frames = crate::chrome::layout::pane_frames(area, scale, n);
                    let active_tab =
                        self.tabs.active().expect("split implies an active tab");
                    // Per pane, not per tab: a pane may later carry its own
                    // profile, so each viewport derives its own selection and
                    // opacity — today every pane reads the tab's identity.
                    let identity = active_tab.identity.as_ref();
                    // Index-parallel with the panes, `None` where one holds a
                    // file (#464). Keeping the slot rather than filtering here
                    // is what lets every later index — frames, rects, bands,
                    // fold maps — go on meaning the same pane.
                    let terms: Vec<Option<_>> = (0..n)
                        .map(|i| active_tab.pane_session(i).map(|s| s.terminal().lock()))
                        .collect();
                    // Each pane letterboxes its own grid (#215); the focused
                    // one must come out equal to `focused_view_rect`, which is
                    // the rectangle the pointer and IME believe. The same
                    // function answers both, so that is now a fact rather than
                    // a comment — read here under the locks the panes are
                    // rendered with, because a grid the reader thread resizes
                    // mid-frame must not leave the viewport describing the
                    // size it had a moment ago.
                    let dims: Vec<(usize, usize)> = terms
                        .iter()
                        .map(|t| {
                            t.as_ref().map_or((1, 1), |t| (t.grid().cols(), t.grid().rows()))
                        })
                        .collect();
                    let rects = crate::chrome::layout::pane_grid_rects(
                        area,
                        scale,
                        self.config.padding,
                        &dims,
                        metrics,
                    );
                    let preedit = self.ime.preedit().map(|p| {
                        zest_render_wgpu::Preedit { text: &p.text, cursor: p.cursor }
                    });
                    // The focused pane's guesses only: the keyboard feeds one
                    // pane, so only one can have any.
                    let predicted = active_tab
                        .pane_session(active_tab.focus)
                        .and_then(|s| s.predicted(predict_policy));
                    // Per pane, and each looks up its *own* address: the panes
                    // select independently, so reading `focused_addr` for all
                    // would light the same block in every one.
                    let bands: Vec<Vec<zest_render_wgpu::BlockBand>> = (0..n)
                        .map(|i| {
                            // Empty for a file pane, which has no blocks —
                            // and the slot is kept for the same reason
                            // `terms` keeps its own.
                            terms[i].as_ref().map_or_else(Vec::new, |term| {
                                Self::block_bands(
                                    &band_colors,
                                    term,
                                    selected_blocks.get(&active_tab.pane_addr(i)).copied(),
                                    active_tab.pane_dead(i),
                                )
                            })
                        })
                        .collect();
                    // A file pane pushes no viewport at all: `pane_is_covered`'s
                    // rule — do not build the terminal, rather than build it and
                    // paint over it — applied one level down, per pane. A grid
                    // drawn underneath would leak a pixel around the chrome's
                    // own antialiased edge (#253).
                    let viewports: Vec<Viewport> = (0..n)
                        .filter_map(|i| {
                            let term = terms[i].as_ref()?;
                            let focused = i == active_tab.focus;
                            Some(Viewport {
                                rect: rects[i],
                                grid: term.grid(),
                                palette: term.palette(),
                                scroll_px,
                                // The find bar has the keyboard while it is
                                // up, so the caret draws hollow and says where
                                // the keys are going.
                                focused: self.focused && focused && !find_open,
                                opacity: pane_opacity(self.config.opacity, identity),
                                // The same picture in every pane of the tab,
                                // fitted to each one: the identity is the
                                // tab's, and a split is two views of one
                                // profile rather than two profiles.
                                background,
                                blocks: &bands[i],
                                // Measured from the pane's *border*, not from
                                // its body: the body is where the grid starts,
                                // so that difference is the letterbox slack
                                // alone — which a pane sized in whole cells
                                // out of its own body makes exactly zero, and
                                // a zero gutter is a rail nobody ever sees
                                // (#460).
                                gutter: crate::chrome::layout::pane_gutter(
                                    frames[i], rects[i], scale,
                                ),
                                scale,
                                selection: term.selection(),
                                selection_bg: pane_selection_bg(self.selection_bg, identity),
                                find: if focused { find_hl.as_ref() } else { None },
                                preedit: if focused { preedit } else { None },
                                predicted: if focused {
                                    predicted.as_ref().map(|p| zest_render_wgpu::Predicted {
                                        cells: &p.cells,
                                        caret: p.caret,
                                    })
                                } else {
                                    None
                                },
                                cursor_on: caret_on,
                                features: &self.config.features,
                                ligatures: self.config.ligatures,
                                cursor_shape: term.cursor_style().shape,
                                cursor_offset: if focused { cursor_offset_px } else { [0.0, 0.0] },
                                // Each pane's own folds: they are stored per
                                // session address and survive a focus change,
                                // so the pane the fold was made in must keep
                                // drawing it after the focus moves on — and
                                // its headers are placed through the same map.
                                row_map: fold_maps.get(i).and_then(|m| m.as_deref()),
                            })
                        })
                        .collect();
                    self.scene.build(
                        &gpu.device,
                        &gpu.queue,
                        &mut gpu.renderer.atlas,
                        fonts,
                        metrics,
                        backdrop,
                        &viewports,
                        &chrome,
                    );
                }
                Some(None) => {
                    let identity = self.tabs.active().and_then(|t| t.identity.as_ref());
                    // Unsplit, so the focused pane is pane 0 — the tab's own
                    // shell, which is a session by construction. `else` here
                    // is the empty strip, and drawing nothing is right for it.
                    let Some(session) = self.tabs.active_source() else { return };
                    let term = session.terminal().lock();
                    // A grid held smaller than this pane by another attached
                    // client sits centered in it (#215).
                    let rect = crate::chrome::insets::letterbox(
                        area,
                        term.grid().cols(),
                        term.grid().rows(),
                        metrics,
                    );
                    // The rail's room: the letterbox slack plus the window
                    // padding, which `grid_rect` has already taken out of
                    // `area` — both are space no cell can occupy.
                    let scale = window.scale_factor() as f32;
                    let gutter_px =
                        (rect[0] - area[0]) + self.config.padding as f32 * scale;
                    let (dead, addr) = self
                        .tabs
                        .active()
                        .map_or((false, None), |t| (t.dead, Some(t.focused_addr())));
                    let bands = Self::block_bands(
                        &band_colors,
                        &term,
                        addr.and_then(|a| selected_blocks.get(&a).copied()),
                        dead,
                    );
                    let predicted = self
                        .tabs
                        .active_source()
                        .and_then(|s| s.predicted(predict_policy));
                    self.scene.build(
                        &gpu.device,
                        &gpu.queue,
                        &mut gpu.renderer.atlas,
                        fonts,
                        metrics,
                        backdrop,
                        &[Viewport {
                            rect,
                            grid: term.grid(),
                            palette: term.palette(),
                            scroll_px,
                            // The find bar has the keyboard while it is up, so the
                            // caret draws hollow and says where the keys are going.
                            focused: self.focused && !find_open,
                            opacity: pane_opacity(self.config.opacity, identity),
                            background,
                            blocks: &bands,
                            gutter: gutter_px,
                            scale,
                            selection: term.selection(),
                            selection_bg: pane_selection_bg(self.selection_bg, identity),
                            find: find_hl.as_ref(),
                            preedit: self.ime.preedit().map(|p| {
                                zest_render_wgpu::Preedit { text: &p.text, cursor: p.cursor }
                            }),
                            predicted: predicted.as_ref().map(|p| zest_render_wgpu::Predicted {
                                cells: &p.cells,
                                caret: p.caret,
                            }),
                            cursor_on: caret_on,
                            features: &self.config.features,
                            ligatures: self.config.ligatures,
                            cursor_shape: term.cursor_style().shape,
                            cursor_offset: cursor_offset_px,
                            row_map: fold_maps.first().and_then(|m| m.as_deref()),
                        }],
                        &chrome,
                    );
                }
            }
        } // locks released before any GPU work

        // `--screenshot`, once the shell has had its settling time: this frame
        // goes to a texture we own instead of to the swapchain. Everything
        // above is untouched — the same scene, built from the same insets,
        // chrome and palette — because a capture that took its own path would
        // eventually disagree with the window and be worth nothing.
        if self.screenshot_at.is_some_and(|at| std::time::Instant::now() >= at) {
            let shot = self.screenshot.clone().expect("a deadline implies a screenshot");
            self.exit_code = Some(capture_frame(gpu, &self.scene, &shot.path));
            self.screenshot_at = None;
            return;
        }

        let frame = match gpu.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(t)
            | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
            wgpu::CurrentSurfaceTexture::Outdated => {
                gpu.surface.configure(&gpu.device, &gpu.config);
                window.request_redraw();
                return;
            }
            other => {
                // The damage latch was already consumed for this frame, and a
                // skipped frame presents nothing — so put the damage back, or
                // a window that starts occluded shows its last (empty) frame
                // until the shell happens to print again. `Occluded(false)`
                // below is what asks for the redraw when the window returns.
                tracing::debug!(?other, "skipping frame");
                // Put the damage back on whichever pane holds the keyboard;
                // a file pane has none to put back, and its chrome flag below
                // is what brings it round again.
                if let Some(session) = self.tabs.active_source() {
                    session.mark_dirty();
                }
                self.chrome_dirty = true;
                return;
            }
        };

        let view = frame.texture.create_view(&Default::default());
        let mut encoder = gpu.device.create_command_encoder(&Default::default());
        // Assigned per frame rather than at theme-change time: it is one `Copy`
        // struct, and the alternative is a second place for the renderer's copy
        // to drift out of step with the settings.
        gpu.renderer.tuning = self.text_tuning;
        gpu.renderer
            .render(&gpu.device, &gpu.queue, &mut encoder, &view, &self.scene);
        gpu.queue.submit([encoder.finish()]);
        gpu.queue.present(frame);
        // Only a presented frame satisfies the chrome's damage; clearing the
        // latch on a skipped one is how a blank window gets stuck blank.
        self.chrome_dirty = false;
        // Last, against the map this frame just built and after the latch is
        // cleared — either order the other way swallows the frame it asks for.
        self.revalidate_hover();
    }

    /// Re-read what the pointer is over, against the hit map this frame built.
    ///
    /// `chrome_hover` is otherwise written only by `CursorMoved`, which cannot
    /// report a change it did not cause — and block headers ride the
    /// scrollback, so a wheel moves one *under* a pointer that never moved.
    /// The affordance then stays attached to the block that scrolled away.
    /// Harmless while wheeling over a header did nothing (#256); visible the
    /// moment it works.
    ///
    /// Reads the maps directly rather than through `chrome_hit`, which would
    /// call `refresh_chrome` from inside the frame that just built it. Only on
    /// an actual change, so an idle window still costs no frames.
    ///
    /// It settles in **at most two** extra frames, not one, and the second is
    /// structural rather than a wobble: a block's action chips are pushed into
    /// the hit map only while that block is hovered, so landing on one takes a
    /// frame to reveal them (`None` → `BlockHeader`) and a frame to fall onto
    /// the chip now drawn on top (`BlockHeader` → `BlockCopy`). It cannot go
    /// further, because every region that reveals the chips also keeps them
    /// revealed — `blocks.rs` matches all four on one arm.
    fn revalidate_hover(&mut self) {
        // A drag keeps the grid, exactly as in `CursorMoved`: a selection
        // dragged across a header must not die there.
        if self.mouse.is_dragging() {
            return;
        }
        let (x, y) = (self.pointer_pos.0 as f32, self.pointer_pos.1 as f32);
        let over = self
            .chrome_layout
            .as_ref()
            .and_then(|l| l.hit.hit(x, y))
            .or_else(|| self.chip_hits.hit(x, y))
            .or_else(|| self.block_hits.hit(x, y));
        if over != self.chrome_hover {
            self.chrome_hover = over;
            // Not the bare `chrome_dirty` latch: `hover` feeds the *cached*
            // layout too, so dropping the cache is the only answer that is
            // right for both layers.
            self.mark_chrome_dirty();
        }
    }

    /// Re-read the config and apply whatever changed, at its own cost.
    ///
    /// Nothing here reaches for a restart: a class the app cannot yet act on is
    /// logged as such, so "this needs a restart" is a statement rather than an
    /// excuse. The layers are re-read from scratch rather than patched, because
    /// a removed key has to fall back through the cascade, and there is no way
    /// to know what it falls back *to* without redoing the merge.
    fn reload_config(&mut self) {
        // The theme directory rides the same reload: an imported file lands
        // moments before the `appearance.theme` write that triggers this,
        // and a hand-edited theme file deserves the same pickup an edited
        // config.toml gets.
        crate::themes::reload();
        let load = zest_config::load(&zest_config::Options {
            profile: self.profile.clone(),
            workspace_dir: std::env::current_dir().ok(),
            cli: Some(self.cli_layer.clone()),
        });

        if !load.errors.is_empty() {
            // The last good settings stay in place. A config being saved
            // mid-edit is the common case, not an exception.
            for e in &load.errors {
                tracing::error!(error = %e, "config reload failed; keeping the last good settings");
            }
            return;
        }

        let new = &load.resolved.settings;
        let class = zest_config::diff(&self.settings, new);
        let changed = zest_config::invalidate::changed_keys(&self.settings, new);
        if class == zest_config::Invalidation::None {
            // No live value moved, but the *files* may still have: a typo key
            // added or removed changes the unknown-keys category (and its
            // provenance) with no settings diff — the open tab must not keep
            // warning about a key the user just fixed.
            if self.unknown_keys != load.resolved.unknown_keys {
                self.unknown_keys = load.resolved.unknown_keys;
                self.provenance = load.resolved.provenance;
                if self.tabs.settings_open() {
                    self.mark_chrome_dirty();
                }
            }
            return;
        }
        tracing::info!(?class, keys = ?changed, "config changed");

        self.settings = new.clone();
        self.config = Config::from(new);
        self.provenance = load.resolved.provenance;
        self.unknown_keys = load.resolved.unknown_keys;
        // Before the invalidation acts: `apply_theme` below reseeds from the
        // identities, and reseeding from stale ones would apply the *old*
        // profile colours under the new config's name.
        self.tabs.reresolve_identities(&self.settings);
        // The overlay, if open, is showing values that just moved under it.
        if self.settings_ui.is_some() {
            self.mark_chrome_dirty();
        }

        // A new generation for the picture cache, outside the `match` because
        // it is right for every class: the file behind an unchanged path may
        // still have been saved over, and a path the settings no longer name
        // holds up to 64 MB of VRAM until something drops it.
        if let Some(gpu) = self.gpu.as_mut() {
            self.backgrounds.invalidate(&mut gpu.renderer.images);
        }

        match class {
            zest_config::Invalidation::None => {}
            zest_config::Invalidation::Free => self.apply_theme(),
            // Everything above Free needs the font stack rebuilt, and geometry
            // needs the pty told afterwards. `rebuild_fonts` handles both,
            // because the second without the first leaves the shell drawing for
            // a grid whose cells are the wrong size.
            zest_config::Invalidation::AtlasBump | zest_config::Invalidation::Geometry => {
                self.apply_theme();
                self.rebuild_fonts();
            }
            zest_config::Invalidation::SurfaceRebuild => {
                self.apply_theme();
                self.apply_transparency();
                if let Some(w) = self.window.as_ref() {
                    // The backdrop is a window attribute, not a surface one,
                    // but it shares this class because both change what is
                    // behind the pixels -- and a backdrop is only ever visible
                    // through pixels the surface above left transparent, which
                    // is why `apply_transparency` runs first.
                    platform::set_backdrop(w, self.config.backdrop);
                    let size = w.inner_size();
                    self.resize_surface(size.width, size.height);
                }
            }
            zest_config::Invalidation::Restart => {
                tracing::warn!(keys = ?changed, "these settings apply on the next launch");
            }
        }

        if let Some(session) = self.tabs.active_source() {
            session.mark_dirty();
        }
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// The palette a session with this identity should be seeded with — the
    /// identity's scheme when it names one that exists, the window's palette
    /// otherwise (unknown warns and falls back; never a failure).
    fn palette_for(
        &self,
        identity: Option<&crate::tabs::ProfileIdentity>,
    ) -> zest_core::PaletteSnapshot {
        seed_palette(&self.palette, identity)
    }

    /// The theme id this window is wearing, OS appearance taken into account.
    ///
    /// Every read goes through here rather than reaching for `config.theme`
    /// directly, or the gallery highlights a row the window is not using and
    /// the profiles editor previews against the wrong palette.
    fn effective_theme(&self) -> &str {
        theme_id(&self.config, self.system_light)
    }

    /// Everything a terminal must be told about this window before it draws.
    ///
    /// One function rather than a line per spawn site, because there are four
    /// of them and a fifth is one feature away — the same shape as the
    /// `Terminal::remote` trap in `AGENTS.md`, where a thing that had to happen
    /// at two doors was done at one and the other went unnoticed for months.
    fn seed_terminal(&self, term: &mut zest_core::Terminal, seed: zest_core::PaletteSnapshot) {
        term.set_palette(seed);
        // The shape a *replica* draws is this window's, not the host's, exactly
        // as `pane_opacity` decides for opacity: the far machine owns the
        // session, this one owns how it looks here.
        term.set_default_cursor_style(zest_core::CursorStyle {
            shape: self.config.cursor_shape,
            ..zest_core::CursorStyle::default()
        });
    }

    /// Re-resolve the theme into the live palette.
    fn apply_theme(&mut self) {
        let theme = crate::themes::get(self.effective_theme())
            .unwrap_or_else(zest_theme::builtin::obsidian);
        let resolved = zest_theme::resolve(&theme);
        self.chrome_colors = ChromeColors::new(
            &theme.ui,
            &theme.effects,
            self.config.chrome_opacity,
            self.config.opacity,
        );
        self.text_tuning = resolve_text_tuning(&self.config);
        self.mark_chrome_dirty();
        self.palette = to_core_palette(&resolved);
        self.selection_bg = zest_core::Rgb::new(
            resolved.selection_bg.r,
            resolved.selection_bg.g,
            resolved.selection_bg.b,
        );
        self.accent_bg =
            zest_core::Rgb::new(theme.ui.accent.r, theme.ui.accent.g, theme.ui.accent.b);
        // Seeding replaces the palette the escape sequences mutate, so an
        // `OSC 4` set before the theme change is deliberately lost -- a theme
        // change is exactly the moment the seed should win. Every tab: a
        // background grid repainted later with a stale palette is a bug
        // nobody can reproduce on demand. Per terminal, through the seed
        // seam: a profile tab keeps its own scheme across a window theme
        // change, and a split's second pane is a terminal too — skipping it
        // left it stranded on the old palette.
        for tab in self.tabs.iter() {
            // One resolve per tab, not per terminal: a split's pane borrows
            // its tab's identity until panes carry their own profile, so a
            // second seed_palette call would repeat the scheme resolve (and
            // its unknown-scheme warn) for the same answer.
            let seed = seed_palette(&self.palette, tab.identity.as_ref());
            for pane in &tab.panes {
                // A file pane has no palette to seed; its ink comes from the
                // window's tokens, not a session's ANSI row (ADR-012).
                if let Some(session) = pane.session() {
                    self.seed_terminal(&mut session.terminal().lock(), seed.clone());
                }
            }
            self.seed_terminal(&mut tab.source().terminal().lock(), seed);
        }
        if let Some(w) = self.window.as_ref() {
            let bg = resolved.background;
            platform::set_background_color(w, bg.r, bg.g, bg.b);
        }
    }

    /// Rebuild the font stack and resize the grid to match the new cell size.
    fn rebuild_fonts(&mut self) {
        // The window's scale factor has to be composed back in.
        //
        // `config.typography` carries the *logical* size from the settings
        // file; the scale factor is a property of the display and lives on the
        // window. Rebuilding from the config alone rasterizes every glyph at
        // 1x, so on a Retina display a config reload silently halved the text.
        let scale = self.window.as_ref().map_or(1.0, |w| w.scale_factor() as f32);
        let typo = Typography { scale_factor: scale, ..self.config.typography };
        let antialias = self.effective_antialias();
        match Fonts::new(&self.config.font_families, typo) {
            Ok(mut fonts) => {
                fonts.set_builtin_box_drawing(self.config.builtin_box_drawing);
                fonts.set_text_antialias(antialias);
                fonts.set_hinting(self.config.text_hinting);
                fonts.set_grid_antialias(self.config.text_antialias);
                self.fonts = Some(fonts);
            }
            Err(e) => {
                // Keeping the old fonts is the only safe answer: there is no
                // such thing as a terminal with no font.
                tracing::error!(error = %e, "new font stack unusable; keeping the previous one");
                return;
            }
        }
        if let Some(gpu) = self.gpu.as_mut() {
            // Before the clear, not after: switching mode recreates the mask
            // texture and starts its own generation, and the rasterizer and the
            // atlas must never disagree about how wide a texel is.
            gpu.renderer.set_text_antialias(&gpu.device, antialias);
        }
        self.sync_antialias();
        if let (Some(gpu), Some(w)) = (self.gpu.as_mut(), self.window.as_ref()) {
            gpu.renderer.clear_atlas();
            let size = w.inner_size();
            self.resize_surface(size.width, size.height);
        }
    }

    /// Make the rasterizer agree with the renderer about coverage.
    ///
    /// The renderer has the last word, and it is not the same word: it refuses
    /// subpixel outright on a device that cannot blend per channel. Asking the
    /// config alone would leave `Fonts` emitting four-byte masks into a
    /// one-byte texture — a validation error or three columns of garbage, on
    /// exactly the machines the fallback exists to serve and never on one that
    /// has the feature. So read the answer back rather than predicting it.
    fn sync_antialias(&mut self) {
        let Some(effective) = self.gpu.as_ref().map(|g| g.renderer.text_antialias()) else {
            return;
        };
        if let Some(fonts) = self.fonts.as_mut() {
            fonts.set_text_antialias(effective);
        }
    }

    /// The antialiasing mode this configuration can actually have.
    ///
    /// Subpixel coverage is three alphas per pixel, and compositing that
    /// against a *translucent* destination is undefined — the compositor holds
    /// one alpha and cannot divide by three. So a translucent window forces
    /// grayscale, regardless of the setting — and *either* opacity makes it
    /// translucent ([`Config::translucent_surface`]). The atlas holds one mask
    /// format for the whole window, so translucent chrome over an opaque grid
    /// still has to pick, and only grayscale is defined for both. On Windows this costs almost
    /// nothing in practice: DX12 reports only `Opaque` (ADR-003), so opacity is
    /// already forced to 1 there and the two fallbacks agree rather than fight.
    ///
    /// The *other* gate — whether the GPU can blend per channel at all — is the
    /// renderer's, because only it knows the device.
    fn effective_antialias(&self) -> zest_font::TextAntialias {
        Self::antialias_for(&self.config)
    }

    /// As [`App::effective_antialias`], against a config alone so it is testable.
    /// What the *renderer* should composite, which is a capability question
    /// and not a preference one.
    ///
    /// `appearance.text_antialias` deliberately does not appear here. It is the
    /// terminal grid's setting and reaches the rasterizer through
    /// `Fonts::set_grid_antialias`; the chrome is pinned to whatever the
    /// renderer can actually do, so that turning the grid down to grayscale
    /// does not drag the window's own furniture with it.
    /// What this process can actually deliver, for the settings form.
    ///
    /// Every field is observed rather than assumed. `transparency` asks
    /// `alpha_mode_for` -- the same function the surface is configured
    /// through -- what this surface's *supported* modes would give a window
    /// that wanted alpha, so the form and the surface cannot disagree about
    /// what the adapter can do (it is adapter-dependent on Windows, ADR-003,
    /// and follows the visual on X11).
    ///
    /// Deliberately **not** the currently configured `alpha_mode`: that is
    /// `Opaque` whenever both opacities are 1.0, which is the ordinary state
    /// of a window nobody has made translucent yet. Reading it would report
    /// "this machine cannot do transparency" to everyone who had not already
    /// turned it on -- greying out the very control they were reaching for.
    ///
    /// Before a surface exists there is nothing to report, and claiming a
    /// limit we have not seen would grey out a control that turns out to be
    /// fine.
    fn capabilities(&self) -> crate::platform::Capabilities {
        let transparency = self.gpu.as_ref().is_none_or(|g| {
            alpha_mode_for(true, &g.alpha_modes) != wgpu::CompositeAlphaMode::Opaque
        });
        crate::platform::Capabilities {
            transparency,
            live_opacity: self.session != crate::platform::Session::X11,
            // Compile-time: there is no Linux implementation to probe for, and
            // probing would be inventing one.
            backdrop: cfg!(windows) || cfg!(target_os = "macos"),
        }
    }

    fn antialias_for(config: &Config) -> zest_font::TextAntialias {
        if config.translucent_surface() {
            return zest_font::TextAntialias::Grayscale;
        }
        zest_font::TextAntialias::Subpixel
    }

    /// The physical window size that holds `window.columns` × `window.rows`
    /// cells, plus the font stack it was measured with.
    ///
    /// Returns `None` when no monitor can be asked or the font stack will not
    /// build — both of which the ordinary path handles perfectly well a moment
    /// later, so this degrades to the default size rather than failing a
    /// launch over a preference.
    ///
    /// The scale factor comes from the primary monitor, because there is no
    /// window yet to ask. If the window then opens on a differently-scaled
    /// display the metrics are wrong, which is why the fonts are handed back:
    /// the caller compares scales and keeps these only when they match.
    fn window_size_in_cells(&self, el: &ActiveEventLoop) -> Option<SizedFromCells> {
        let scale = el.primary_monitor()?.scale_factor() as f32;
        let typo = Typography { scale_factor: scale, ..self.config.typography };
        let fonts = Fonts::new(&self.config.font_families, typo).ok()?;
        let metrics = fonts.cell_metrics();
        let (w, h) =
            self.insets_at(scale).window_size(metrics, self.config.columns, self.config.rows);
        tracing::debug!(
            cols = self.config.columns,
            rows = self.config.rows,
            w,
            h,
            scale,
            "sizing the window from window.columns/rows"
        );
        Some(SizedFromCells { width: w, height: h, scale, fonts: Box::new(fonts) })
    }

}

/// The new value for a list or key/value row with one item removed.
///
/// Free-standing because both editors need the identical transformation, and a
/// second copy is how they drift: the Settings tab and the §12 profiles tab
/// draw these rows with the same `draw_control`, so they must also agree about
/// what its × does. `None` means "nothing to do", never "write null".
fn list_value_without(
    widget: zest_config::ui::Widget,
    current: &serde_json::Value,
    item: usize,
) -> Option<serde_json::Value> {
    use zest_config::ui::Widget;
    match widget {
        Widget::FontList | Widget::TagList => {
            let mut arr = current.as_array().cloned().unwrap_or_default();
            if item >= arr.len() {
                return None;
            }
            arr.remove(item);
            Some(serde_json::Value::Array(arr))
        }
        Widget::KeyValue => {
            // By position, because that is what the × was drawn beside: the
            // control renders the map in iteration order, and resolving the
            // click back to a *key* here is what keeps the two in step.
            let map = current.as_object()?;
            let key = map.keys().nth(item).cloned()?;
            let mut map = map.clone();
            map.remove(&key);
            Some(serde_json::Value::Object(map))
        }
        _ => None,
    }
}

/// The new **own** env for the §12 `env` row whose x was clicked at `item`.
///
/// `env` is the one control whose drawn map and written map are different
/// maps, and both directions were wrong before #550. The index comes from the
/// *merged* environment, because that is what the row drew; the value returned
/// is the profile's *own* table, because that is what the file holds. Which
/// branch applies is decided by ownership:
///
/// - an entry the profile owns is **deleted** from its table, so it falls back
///   through Defaults again if Defaults also names it;
/// - an inherited entry becomes an **empty value**, which is the design's
///   per-variable drop ("an empty value unsets a variable"). Removing it from
///   a table it is not in writes nothing at all, and `fold_meta` merges
///   Defaults straight back on the next read -- which is exactly what made
///   the x look broken while every call reported success.
///
/// So an entry already showing `unset` is *deleted* rather than re-emptied,
/// and inheritance resumes. That is the only reading of "remove this override"
/// that leaves the row reversible from the keyboard-free path.
///
/// `None` means "nothing to do", never "write null", like its neighbours.
fn env_value_without(
    merged: &serde_json::Value,
    own: &serde_json::Value,
    item: usize,
) -> Option<serde_json::Value> {
    let key = merged.as_object()?.keys().nth(item)?.clone();
    let mut own = own.as_object().cloned().unwrap_or_default();
    if own.remove(&key).is_none() {
        own.insert(key, serde_json::Value::String(String::new()));
    }
    Some(serde_json::Value::Object(own))
}

/// `KEY=VALUE`, with a bare `KEY` meaning an empty value -- the
/// empty-means-unset spelling all the way down to the pty. `None` when there
/// is no key at all, which is the one input that cannot be an entry.
///
/// One copy, because add and edit must agree: a rule that accepts `Q=a=b` on
/// an add and rejects it on an edit is a row whose entries can be created and
/// then never corrected.
fn split_env_entry(text: &str) -> Option<(String, String)> {
    let (key, value) = match text.split_once('=') {
        Some((k, v)) => (k.trim(), v.trim()),
        None => (text, ""),
    };
    (!key.is_empty()).then(|| (key.to_string(), value.to_string()))
}

/// The new value for a list or key/value row whose entry at `item` becomes
/// `text`.
///
/// The third of the trio beside [`list_value_without`] and [`list_value_with`],
/// free-standing for their reason: both editors draw these rows with the same
/// `draw_control`, so they must agree about what its controls do.
///
/// `FontList` is absent on purpose. A family is chosen from the roster the
/// client brought, not typed -- its dashed row opens that menu, and its
/// per-item region already means drag-to-reorder.
fn list_value_replacing(
    widget: zest_config::ui::Widget,
    current: &serde_json::Value,
    item: usize,
    text: &str,
) -> Option<serde_json::Value> {
    use zest_config::ui::Widget;
    match widget {
        Widget::TagList => {
            let mut arr = current.as_array().cloned().unwrap_or_default();
            let slot = arr.get_mut(item)?;
            *slot = serde_json::Value::String(text.to_string());
            Some(serde_json::Value::Array(arr))
        }
        Widget::KeyValue => {
            let map = current.as_object()?;
            // By position, like the x beside it: the control renders the map
            // in iteration order, and resolving the click back to a *key*
            // here is what keeps the two in step.
            let old = map.keys().nth(item)?.clone();
            let (key, value) = split_env_entry(text)?;
            let mut map = map.clone();
            map.remove(&old);
            map.insert(key, serde_json::Value::String(value));
            Some(serde_json::Value::Object(map))
        }
        _ => None,
    }
}

/// The new **own** env for the §12 `env` row whose entry at `item` becomes
/// `text`.
///
/// [`env_value_without`]'s twin, and **three** maps because a rename spans all
/// three layers: the index names an entry in the `merged` environment the row
/// drew, the value is written to the profile's `own` table, and whether the
/// old key survives that write is `defaults`' business.
///
/// Editing an **inherited** entry creates an override for it, which is the
/// only thing the gesture can mean -- the person clicked a row showing a value
/// and typed a different one.
///
/// A **rename has to take the old key with it**, and removing it from the own
/// table is not enough when Defaults also names it: `fold_meta` merges it back
/// on the next read and one edit becomes two variables, the old one still
/// reaching the shell under a name nobody meant to keep. The tombstone is an
/// explicit empty value -- the same per-variable unset the x already writes,
/// so it is a state the row can already draw and the person can already undo.
///
/// `defaults` is empty when the profile being edited *is* Defaults, which is
/// `resolve_profile`'s own rule ("resolving `defaults` itself must not fall
/// through to itself"): there, removing the key really does remove it, and a
/// tombstone would leave an `unset` row over a variable nothing sets.
fn env_value_replacing(
    merged: &serde_json::Value,
    own: &serde_json::Value,
    defaults: &serde_json::Value,
    item: usize,
    text: &str,
) -> Option<serde_json::Value> {
    let old = merged.as_object()?.keys().nth(item)?.clone();
    let (key, value) = split_env_entry(text)?;
    let mut own = own.as_object().cloned().unwrap_or_default();
    own.remove(&old);
    if key != old && defaults.get(&old).is_some() {
        own.insert(old, serde_json::Value::String(String::new()));
    }
    own.insert(key, serde_json::Value::String(value));
    Some(serde_json::Value::Object(own))
}

/// The text an entry's edit buffer opens with: what the row drew, spelled the
/// way the add chip would accept it, so one parse serves both.
fn list_entry_text(
    widget: zest_config::ui::Widget,
    current: &serde_json::Value,
    item: usize,
) -> Option<String> {
    use zest_config::ui::Widget;
    match widget {
        Widget::TagList => current.as_array()?.get(item)?.as_str().map(str::to_string),
        Widget::KeyValue => {
            let (k, v) = current.as_object()?.iter().nth(item)?;
            Some(format!("{k}={}", v.as_str().unwrap_or_default()))
        }
        _ => None,
    }
}

/// The new value for a list or key/value row with `text` appended.
///
/// `KEY=VALUE` for a key/value row; a bare `KEY` gets an empty value, which is
/// the empty-means-unset spelling all the way down to the pty. `None` means
/// the input cannot be an entry, which the caller shows as a buffer error
/// rather than swallowing.
fn list_value_with(
    widget: zest_config::ui::Widget,
    current: &serde_json::Value,
    text: &str,
) -> Option<serde_json::Value> {
    use zest_config::ui::Widget;
    match widget {
        Widget::TagList => {
            let mut arr = current.as_array().cloned().unwrap_or_default();
            arr.push(serde_json::Value::String(text.to_string()));
            Some(serde_json::Value::Array(arr))
        }
        Widget::KeyValue => {
            let (key, value) = split_env_entry(text)?;
            let mut map = current.as_object().cloned().unwrap_or_default();
            map.insert(key, serde_json::Value::String(value));
            Some(serde_json::Value::Object(map))
        }
        _ => None,
    }
}



/// One watching connection to a window's daemon keeps its session list
/// fresh through pushes — and carries the approval queue, whose pushes
/// raise the modal in every window.
pub(crate) fn watch_pairings(
    fleet: &crate::fleet::FleetModel,
    route: HostRoute,
    approval: ApprovalCell,
    proxy: &EventLoopProxy<Wakeup>,
) {
    let proxy = proxy.clone();
    fleet.watch(
        move || route.dialer(),
        move |event| {
            let proxy = proxy.clone();
            let post: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
                let _ = proxy.send_event(Wakeup::PairingChanged);
            });
            match event {
                crate::fleet::PairingEvent::Requested {
                    client,
                    label,
                    remote,
                    code,
                    expires_in_secs,
                } => arm_approval_request(
                    &approval,
                    client,
                    label,
                    remote,
                    code,
                    expires_in_secs,
                    post,
                ),
                // Someone answered — at the daemon's stdin, in another
                // window — or the device gave up. Either way there is
                // nothing left to decide.
                crate::fleet::PairingEvent::Resolved { client } => {
                    let mut queue = approval.lock();
                    let before = queue.len();
                    queue.retain(|r| r.client != client);
                    if queue.len() != before {
                        drop(queue);
                        post();
                    }
                }
            }
        },
    );
}

/// What the event loop should do once it runs out of work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NextWake {
    /// A screenshot's delay has elapsed: draw the frame now, in place.
    CaptureNow,
    After(std::time::Duration),
    /// Nothing is pending — sleep until the OS has something to say.
    Idle,
}

/// Wait for something to happen rather than polling — unless something on
/// screen is animating, in which case the clock names the *one* deadline it
/// needs. A resting window schedules nothing (the 0%-idle guarantee); a
/// blinking cursor costs exactly its two frames a second, which is the price of
/// the setting being on. The screenshot deadline is one more thing that wants
/// waking for, and the earlier of the two wins — a blinking cursor must not
/// push the capture past its delay, and the capture must not stop the cursor
/// blinking in the frame it captures.
///
/// An *elapsed* screenshot deadline is its own answer rather than a zero-length
/// wait, and both halves of that matter. Scheduling `WaitUntil(now)` wakes the
/// loop immediately and re-schedules the same thing, for ever: measured at
/// 35,189 wake-ups in twelve seconds, a busy loop wearing the costume of the
/// idle guarantee. And the wake-up could not have helped anyway, since what it
/// does is ask the window to repaint and screenshot mode has no visible window
/// to repaint. Both are why `--screenshot-delay` wrote nothing at all (#255).
/// What a keystroke is to the echo predictor, read off the key the keyboard
/// reported — never off the encoded bytes.
///
/// A printable is a single code point with no Ctrl, Alt or Super held (Shift
/// is part of the character). Everything else is `Other`: the predictor
/// flushes on it, because what Enter, an arrow or a chord does is the
/// shell's business. The predictor applies its own width rule on top.
fn predict_key(key: &winit::keyboard::Key, mods: ModifiersState) -> zest_proto::Key {
    use winit::keyboard::{Key, NamedKey};
    if mods.control_key() || mods.alt_key() || mods.super_key() {
        return zest_proto::Key::Other;
    }
    match key {
        Key::Named(NamedKey::Backspace) => zest_proto::Key::Backspace,
        Key::Named(NamedKey::Space) => zest_proto::Key::Printable(' '),
        Key::Character(s) => {
            let mut it = s.chars();
            match (it.next(), it.next()) {
                (Some(c), None) => zest_proto::Key::Printable(c),
                _ => zest_proto::Key::Other,
            }
        }
        _ => zest_proto::Key::Other,
    }
}

fn next_wake(
    screenshot_in: Option<std::time::Duration>,
    anim_in: Option<std::time::Duration>,
) -> NextWake {
    if screenshot_in == Some(std::time::Duration::ZERO) {
        return NextWake::CaptureNow;
    }
    match [screenshot_in, anim_in].into_iter().flatten().min() {
        Some(delay) => NextWake::After(delay),
        None => NextWake::Idle,
    }
}


/// Stem darkening, from the settings, clamped.
///
/// Until this existed, `Renderer::tuning` was assigned `TextTuning::default()`
/// once and never touched again, so `appearance.text_gamma` was a control that
/// visibly did nothing — and `Config::from(&Settings)` did not even carry it
/// across.
///
/// Clamped rather than trusted: this comes from a file a person edits by hand,
/// and a gamma of zero is a division by zero in the shader.
fn resolve_text_tuning(config: &Config) -> zest_render_wgpu::TextTuning {
    zest_render_wgpu::TextTuning {
        gamma: config.text_gamma.clamp(0.5, 4.0),
        contrast: config.text_contrast.clamp(0.0, 1.0),
    }
}

/// The palette one terminal should be seeded with: its identity's scheme when
/// it has one, the window's otherwise. Unknown or unset falls back to the
/// window with a warn, never a failure (`tabs::resolve_scheme`).
///
/// The seam `apply_theme` reseeds through, one call per terminal — split
/// panes included, because a pane may later carry its own profile. Pure so
/// the reseed decision is testable without a window: this is where "a window
/// theme change wiped every profile tab's scheme" lived. Resolving here is
/// fine — seeding happens per spawn and per theme change, not per frame.
fn seed_palette(
    window: &zest_core::PaletteSnapshot,
    identity: Option<&crate::tabs::ProfileIdentity>,
) -> zest_core::PaletteSnapshot {
    identity
        .and_then(|i| i.scheme.as_deref())
        .and_then(crate::tabs::resolve_scheme)
        .map_or_else(|| window.clone(), |r| to_core_palette(&r))
}

/// The selection wash for one viewport, from the same scheme its grid uses.
///
/// A profile grid selected in the *window's* selection colour can be
/// unreadable — a dark window's wash over paper's light background — so the
/// selection follows the scheme, not the window. Reads the identity's
/// *cached* wash rather than resolving the scheme: this runs per pane per
/// frame, where a resolve is a full theme lookup plus an allocation, and a
/// deleted scheme's warn would repeat on every caret-blink repaint.
fn pane_selection_bg(
    window: zest_core::Rgb,
    identity: Option<&crate::tabs::ProfileIdentity>,
) -> zest_core::Rgb {
    identity.and_then(|i| i.selection_bg).unwrap_or(window)
}

/// A `Browse…` click that has not opened its dialog yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PickRequest {
    /// Index into the open editor's `fields`, resolved at click time: the row
    /// list is rebuilt every frame, so a row index would not survive the wait.
    field: usize,
    /// Which editor asked, and therefore which write path the answer takes.
    to_profiles: bool,
}

/// Where a file dropped on the window lands.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DropTarget {
    /// `window.background_image` in the config file.
    Window,
    /// One profile's override of it. The name is `defaults` for the Defaults
    /// layer, which is a real profile table as far as the writer is concerned.
    Profile(String),
    /// Nothing on screen claims a dropped file.
    Unclaimed,
}

/// Which surface claims a dropped file.
///
/// **Decided by which screen is open, never by where the pointer is.** winit
/// discards the cursor position on all three of its OLE callbacks —
/// `DragEnter`, `DragOver` and `Drop` each take a `_pt: *const POINTL` and drop
/// it — and emits no event at all for `DragOver`, so there is neither a
/// coordinate on the drop nor a `CursorMoved` during the drag to infer one
/// from. A positional drop target (§12 draws a 96px `<image-slot>`) needs our
/// own `IDropTarget` behind a window subclass, which is the same unsolved
/// problem as Snap Layouts' `WM_NCHITTEST`.
///
/// A terminal claims nothing, and that is the deliberate part. Dropping a file
/// on a terminal conventionally **types its path** — iTerm2, Windows Terminal,
/// GNOME Terminal and Konsole all do it — so quietly changing the wallpaper
/// instead would seize that gesture for exactly the file someone is most likely
/// to drag in on purpose, with no way to undo it from the keyboard. Leaving it
/// unclaimed keeps that meaning available for whoever implements it.
fn drop_target(settings_tab: bool, profiles_tab: bool, profile: Option<&str>) -> DropTarget {
    match (profiles_tab, profile) {
        // The profiles editor is the narrower claim, and it is checked first so
        // the answer does not depend on whether two tabs can be active at once.
        (true, Some(name)) => DropTarget::Profile(name.to_string()),
        _ if settings_tab => DropTarget::Window,
        _ => DropTarget::Unclaimed,
    }
}

/// The cell-background opacity for one viewport: the identity's override when
/// it set one, the window's otherwise. Rides the viewport only — whether the
/// *surface* can show the desktop through stays a window-level decision made
/// at surface creation.
fn pane_opacity(window: f32, identity: Option<&crate::tabs::ProfileIdentity>) -> f32 {
    identity.and_then(|i| i.opacity).map_or(window, |o| o.clamp(0.0, 1.0))
}

/// `value` when it is a real number, `fallback` when it is not.
///
/// `f32::clamp` preserves NaN, so clamping a hand-edited `nan` out of a config
/// file does nothing at all — and a NaN reaching the spring integrator makes an
/// animation that can never report rest, which is the event loop never sleeping
/// again. The fallback is the schema's own default, so a nonsense value behaves
/// as though the key had been left out.
fn finite_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value
    } else {
        tracing::warn!(?value, "a motion setting is not a real number; using the default");
        fallback
    }
}

/// The theme this window should actually be wearing.
///
/// Resolved here rather than in the cascade, which is where it used to live and
/// where it could not work: the OS-appearance layer sat *below* the user's
/// file, so an explicit `theme =` beat it — that being everyone who cared
/// enough to choose one — and it hardcoded `paper` rather than reading
/// `light_theme` at all. Following the OS is a question about the live window,
/// which changes without any file changing, so it belongs on the repaint path.
///
/// An empty `light_theme` means "follow `theme` regardless", exactly as the
/// schema promises: someone who has deliberately picked a dark theme and turned
/// following on has not asked to be given a light one nobody chose.
fn theme_id(config: &Config, system_light: bool) -> &str {
    if config.follow_system_theme && system_light && !config.light_theme.is_empty() {
        &config.light_theme
    } else {
        &config.theme
    }
}

/// Layer `shell.cwd` and `shell.env` onto a spec `default_shell` just built.
///
/// Free-standing so the two orderings it decides are testable without an
/// `App`, a window or an event loop — both are judgement calls, and a comment
/// asserting one is not the same as a test that would notice it flipping.
///
/// **cwd is a default the caller may overwrite.** A profile's
/// `starting_directory` is applied after `build_spec` returns and has to win,
/// being the more specific of the two.
///
/// **env goes last, so the user's entries win a collision**, and `injected`
/// makes one collision loud. Both rules, and the reasoning behind them, live on
/// [`CommandSpec::layer_env`] — the daemon needs the identical behaviour and
/// this function is not reachable from it. Which mattered more than it sounds:
/// while this was the only copy, the only path that applied `shell.env` at all
/// was the in-process `--no-daemon` fallback, so the setting did nothing for
/// every ordinary session (#488).
fn apply_shell_settings(spec: &mut CommandSpec, config: &Config, injected: &[String]) {
    spec.cwd.clone_from(&config.shell_cwd);
    spec.layer_env(config.shell_env.iter().cloned(), injected);
}

/// The most rows one wheel event may move, in either direction.
///
/// Two orders of magnitude above the fastest real gesture — a violent trackpad
/// flick is tens of rows, and the largest configurable step is 50 — so nothing
/// a hand can do reaches it. It exists for what a hand cannot do: the row count
/// bounds a loop *and* a `Vec::with_capacity`, and a float-to-int cast in Rust
/// saturates rather than wrapping, so one synthetic `PixelDelta` carrying a
/// huge or infinite value arrives as `isize::MAX` rows and the allocation
/// aborts the process.
const MAX_WHEEL_ROWS: isize = 10_000;

/// Rows one wheel gesture moves: whole notches times `scrolling.lines_per_notch`.
///
/// Signed, because the sign is the direction and both consumers need it — the
/// scrollback move takes it as-is, and the alternate screen sends that many
/// arrow keys. Extracted so there is exactly one answer: the two used to be
/// separate literals, and a wheel that scrolled `less` at a different rate to
/// history is the kind of thing nobody reports and everybody feels.
/// Ask the host for the page before the oldest row held, once the reader has
/// scrolled to the top of what this window has (#545).
///
/// Without it a replica's scrollback ends wherever the stream happened to
/// start: the view pins, and the rows above exist only on the daemon. Checked
/// after the scroll rather than before, so the pull happens on the gesture
/// that reached the top rather than one gesture later.
fn pull_history_at_top(session: &dyn crate::source::SessionSource) {
    let at_top = {
        let term = session.terminal();
        let term = term.lock();
        term.grid().display_offset() >= term.grid().scrollback_len()
    };
    if at_top {
        let _ = session.backfill_history();
    }
}

fn wheel_rows(notches: f32, per_notch: usize) -> isize {
    // Saturating, then clamped: `notches` is an accumulated trackpad delta, so
    // neither bound is reachable by scrolling — see `MAX_WHEEL_ROWS` for what
    // they are actually for. NaN casts to 0, which is the right answer: a
    // gesture that moves nothing must not become a full-clamp jump.
    (notches as isize).saturating_mul(per_notch.max(1) as isize).clamp(-MAX_WHEEL_ROWS, MAX_WHEEL_ROWS)
}

/// `zest-theme` and `zest-core` deliberately do not depend on each other, so the
/// app owns this conversion.
fn to_core_palette(r: &zest_theme::ResolvedPalette) -> zest_core::PaletteSnapshot {
    let conv = |c: zest_theme::Rgba8| zest_core::Rgb::new(c.r, c.g, c.b);
    let mut colors = [zest_core::Rgb::default(); 256];
    for (i, c) in r.colors.iter().enumerate() {
        colors[i] = conv(*c);
    }
    zest_core::PaletteSnapshot {
        colors,
        foreground: conv(r.foreground),
        background: conv(r.background),
        cursor: conv(r.cursor),
    }
}


#[cfg(test)]
mod next_wake_tests {
    use super::{next_wake, NextWake};
    use std::time::Duration;

    #[test]
    fn an_elapsed_screenshot_deadline_captures_instead_of_rescheduling() {
        // The bug this pins is two bugs (#255). `WaitUntil(now)` fires at once
        // and re-arms itself — 35,189 wake-ups in twelve seconds, measured —
        // and every one of them was wasted, because waking asks the *window*
        // to repaint and screenshot mode never shows one. So the capture has
        // to be its own answer here, not a zero-length sleep.
        assert_eq!(next_wake(Some(Duration::ZERO), None), NextWake::CaptureNow);
        assert_eq!(
            next_wake(Some(Duration::ZERO), Some(Duration::from_millis(500))),
            NextWake::CaptureNow,
            "and it outranks the animation clock — a blink must not defer the shot"
        );
    }

    #[test]
    fn the_earlier_deadline_wins_while_both_are_ahead() {
        assert_eq!(
            next_wake(Some(Duration::from_millis(400)), Some(Duration::from_millis(90))),
            NextWake::After(Duration::from_millis(90)),
            "a cursor blink inside the delay still gets its frame"
        );
        assert_eq!(
            next_wake(Some(Duration::from_millis(30)), Some(Duration::from_millis(500))),
            NextWake::After(Duration::from_millis(30)),
            "and the capture is not pushed past its delay by a slow animation"
        );
    }

    #[test]
    fn a_resting_window_schedules_nothing() {
        // The 0%-idle guarantee, in the one place that can break it.
        assert_eq!(next_wake(None, None), NextWake::Idle);
        assert_eq!(
            next_wake(None, Some(Duration::from_millis(550))),
            NextWake::After(Duration::from_millis(550))
        );
    }
}

#[cfg(test)]
mod host_slot_tests {
    use super::HostSlots;
    use zest_proto::{HostId, SessionAddr, SessionId};

    fn id(b: u8) -> HostId {
        HostId::from_bytes([b; 32])
    }

    fn live(host: HostId, n: u64) -> SessionAddr {
        SessionAddr::new(host, SessionId(n))
    }

    #[test]
    fn a_connecting_tab_and_the_live_tab_it_becomes_keep_the_same_accent() {
        // #273's guard-rail: the table is rebuilt every chrome refresh, so
        // "the placeholder adopts the id's slot" has to fall out of the keying
        // rules across two refreshes — one before the launch settles, one
        // after. If it does not, every successful cross-host launch is a
        // visible colour flicker at the moment it connects.
        let alpha = live(id(1), 1);
        let before = {
            let mut slots = HostSlots::new();
            slots.slot(alpha, "alpha");
            slots.slot(crate::tabs::placeholder_addr(1), "beta")
        };
        let after = {
            let mut slots = HostSlots::new();
            slots.slot(alpha, "alpha");
            slots.slot(live(id(2), 7), "beta")
        };
        assert_eq!(
            before, after,
            "the tab's slot survives its address going from placeholder to real"
        );

        // And the sibling case inside one refresh: a second launch to the
        // same host is still a placeholder when the first settles, and the
        // two must not split — the settled id claims the slot its label
        // opened, and the label still finds it there.
        let mut slots = HostSlots::new();
        let settled = slots.slot(live(id(2), 7), "beta");
        let sibling = slots.slot(crate::tabs::placeholder_addr(2), "beta");
        assert_eq!(
            settled, sibling,
            "a still-connecting sibling shares the slot of the launch that settled"
        );
    }

    #[test]
    fn renaming_a_host_does_not_reshuffle_other_hosts_accents() {
        // The headline bug: keyed by label, a renamed machine is a new entry.
        // A tab's label is captured when the tab opens, so after a rename one
        // machine's tabs can carry both names at once — two entries for one
        // host, and every host after it moves down a colour. Keyed by id, a
        // renamed machine is the same machine.
        let (a, b) = (live(id(1), 1), live(id(2), 2));
        let (before_a, before_b) = {
            let mut slots = HostSlots::new();
            (slots.slot(a, "alpha"), slots.slot(b, "beta"))
        };
        let (old_a, new_a, after_b) = {
            let mut slots = HostSlots::new();
            (
                slots.slot(a, "alpha"),
                slots.slot(live(id(1), 3), "zulu"),
                slots.slot(b, "beta"),
            )
        };
        assert_eq!(before_a, old_a, "the renamed host keeps its own slot");
        assert_eq!(old_a, new_a, "its tab opened under the new name is still the same machine");
        assert_eq!(before_b, after_b, "and the host after it keeps its colour");
    }

    #[test]
    fn two_hosts_sharing_a_label_get_distinct_slots() {
        // The same bug read the other way: two machines that happen to share
        // a display name collapsed into one slot. Their ids differ, so their
        // colours must too.
        let mut slots = HostSlots::new();
        let first = slots.slot(live(id(1), 1), "dev");
        let second = slots.slot(live(id(2), 2), "dev");
        assert_ne!(first, second, "distinct machines take distinct colours, whatever they are called");
    }
}


#[cfg(test)]
mod code_entry_tests {
    use super::*;

    fn entry() -> crate::settings_ui::EditBuffer {
        crate::settings_ui::EditBuffer {
            field_idx: 0,
            buffer: TextField::default(),
            error: true,
            list: crate::settings_ui::ListEdit::Value,
        }
    }

    #[test]
    fn a_pasted_code_survives_its_wrapping() {
        // The realistic clipboard: the code copied with whitespace around it,
        // or the whole "type this code" line. What must land is the code —
        // filtered to the alphabet, uppercased, clamped — because a paste
        // that has to be pre-cleaned is barely better than no paste (#228).
        let mut edit = entry();
        push_code_chars(&mut edit, "  wxkm-4t9c\n");
        assert_eq!(edit.buffer.text(), "WXKM4T9C", "separators and whitespace drop, case folds");
        assert!(!edit.error, "accepted input clears the error mark");

        push_code_chars(&mut edit, "MORE");
        assert_eq!(
            edit.buffer.text(),
            "WXKM4T9C",
            "the clamp holds for paste exactly as for a held key"
        );
    }

    #[test]
    fn select_all_then_paste_replaces_a_full_code() {
        // The clamp counts what will remain, not what is there: with the box
        // full and everything selected, breaking on the current length would
        // leave a code that can never be replaced — and ⌘A-then-paste is
        // exactly how someone corrects a mistyped one.
        let mut edit = entry();
        push_code_chars(&mut edit, "WXKM4T9C");
        assert_eq!(edit.buffer.text(), "WXKM4T9C", "the box is full");

        edit.buffer.select_all();
        push_code_chars(&mut edit, "2345PQRS");
        assert_eq!(edit.buffer.text(), "2345PQRS", "the selection was replaced, not refused");
    }

    #[test]
    fn junk_pastes_change_nothing() {
        let mut edit = entry();
        push_code_chars(&mut edit, " \n\t—·—");
        assert_eq!(edit.buffer.text(), "", "nothing from the alphabet, nothing in the box");
        assert!(edit.error, "and an error mark is not cleared by input that put nothing in");
    }

    #[test]
    fn confusables_are_dropped_because_no_code_contains_them() {
        // The alphabet excludes 0/O, 1/I/L and U precisely so a code cannot
        // be misread between screens. Letting them into the box would spend
        // slots the real characters then cannot fill, and the eventual
        // refusal would name the code rather than the typo.
        let mut edit = entry();
        push_code_chars(&mut edit, "0O1IlLuUWX2Z");
        assert_eq!(
            edit.buffer.text(),
            "WX2Z",
            "every excluded confusable drops; alphabet characters land in order"
        );
    }
}

#[cfg(test)]
mod fleet_device_row_tests {
    use super::fleet_device_rows;
    use crate::chrome::model::FleetDeviceAction;
    use crate::fleet::AccountDevice;
    use zest_proto::ClientId;

    fn device(id: u8, label: &str, kind: &str, approved: bool) -> AccountDevice {
        AccountDevice {
            id: ClientId::from_bytes([id; 32]),
            label: label.into(),
            kind: kind.into(),
            approved,
        }
    }

    #[test]
    fn the_verb_table_is_the_rows_state() {
        // Pending approves, approved vouches, the own key offers nothing —
        // the whole approver surface in one table, because a wrong verb here
        // is a button that mints a statement the server must refuse.
        let own = ClientId::from_bytes([3; 32]);
        let rows = fleet_device_rows(
            &[
                device(1, "work-browser", "browser", false),
                device(2, "andy-phone", "phone", true),
                device(3, "studio-app", "desktop", true),
            ],
            Some(own),
        );
        assert_eq!(rows.len(), 3, "every device is a row; hiding one hides a pending key");
        assert_eq!(rows[0].action, FleetDeviceAction::Approve, "pending → Approve");
        assert_eq!(rows[0].detail, "browser · pending");
        assert_eq!(rows[1].action, FleetDeviceAction::Vouch, "approved, not mine → Vouch");
        assert_eq!(rows[1].detail, "phone · approved");
        assert_eq!(
            rows[2].action,
            FleetDeviceAction::None,
            "a key cannot vouch for itself, so the own row offers nothing"
        );
        assert_eq!(rows[2].detail, "desktop · this app", "and says why it is different");
    }

    #[test]
    fn an_unknown_own_key_leaves_the_own_row_actionable() {
        // Before the keychain is ever consulted the app cannot tell its own
        // row apart; the click path re-checks with the identity loaded and
        // refuses with a name. Hiding the button instead would require a
        // keychain read per chrome rebuild — the trade the helper documents.
        let rows = fleet_device_rows(&[device(3, "studio-app", "desktop", true)], None);
        assert_eq!(rows[0].action, FleetDeviceAction::Vouch);
        assert_eq!(rows[0].detail, "desktop · approved");
    }
}

#[cfg(test)]
mod typography_tests {
    use super::Config;

    fn config_with(features: &[&str], ligatures: bool) -> Config {
        let mut s = zest_config::Settings::default();
        s.typography.features = features.iter().map(|f| (*f).to_string()).collect();
        s.typography.ligatures = ligatures;
        Config::from(&s)
    }

    #[test]
    fn the_documented_spellings_all_parse() {
        // The schema says "`liga`-style. Prefix with `-` to disable", and the
        // parser takes the Alacritty/Ghostty/Kitty forms so an existing config
        // pastes straight in.
        let c = config_with(&["calt", "-liga", "+ss01", "cv02=2"], false);
        assert_eq!(c.features.len(), 4, "every documented spelling survives");
        assert_eq!(c.features[1].value, 0, "a `-` prefix disables");
        assert_eq!(c.features[2].value, 1, "a `+` prefix enables");
        assert_eq!(c.features[3].value, 2, "`=n` carries the value");
    }

    #[test]
    fn a_bad_tag_is_dropped_and_the_rest_survive() {
        // A typo in one tag must not cost the user their whole config: the
        // schema calls this a list of tags, not a grammar.
        let c = config_with(&["calt", "nonsense-tag", "ss01"], false);
        assert_eq!(c.features.len(), 2, "the two real tags still apply");
    }

    #[test]
    fn the_default_config_asks_for_no_shaping_at_all() {
        // The risk control for the whole group. `glyph_for` is a charmap lookup
        // with no GSUB, so shaping is what makes these settings possible -- and
        // it runs only when one of them is set. A default session keeps the
        // per-character path it has always had, and the throughput targets
        // with it.
        let c = Config::default();
        assert!(c.features.is_empty());
        assert!(!c.ligatures);
    }
}

#[cfg(test)]
mod motion_settings_tests {
    use super::Config;
    use crate::motion::Spring;

    fn config_with(enabled: bool, respect: bool) -> Config {
        let mut s = zest_config::Settings::default();
        s.motion.enabled = enabled;
        s.motion.respect_system_reduce_motion = respect;
        Config::from(&s)
    }

    /// `App::motion_allowed` without an `App` — the same expression, so the
    /// truth table is pinned even though the OS half cannot be faked here.
    fn allowed(config: &Config, os_reduces: bool) -> bool {
        config.motion_enabled && !(config.respect_reduce_motion && os_reduces)
    }

    #[test]
    fn reduce_motion_overrides_a_user_who_asked_for_animation() {
        // The accessibility setting wins when it is being respected: someone
        // with a vestibular disorder has said, at the OS level, that motion
        // makes the machine unpleasant to use, and a per-app default is not the
        // place to argue.
        let on = config_with(true, true);
        assert!(allowed(&on, false));
        assert!(!allowed(&on, true), "the OS asked for less motion and is being respected");

        // ...and stops winning when it is not.
        let ignoring = config_with(true, false);
        assert!(allowed(&ignoring, true), "opting out of the OS setting must actually opt out");

        // `enabled = false` is absolute either way round.
        for respect in [true, false] {
            for os in [true, false] {
                assert!(!allowed(&config_with(false, respect), os), "motion.enabled = false wins");
            }
        }
    }

    #[test]
    fn the_shipped_defaults_animate_and_defer_to_the_os() {
        let d = Config::default();
        assert!(d.motion_enabled);
        assert!(d.respect_reduce_motion);
        assert!(d.smooth_scroll);
    }

    #[test]
    fn the_spring_parameters_are_clamped_before_they_reach_the_integrator() {
        // A hand-edited `spring_response = 0` is a division by zero wearing a
        // preference's clothes -- the angular frequency is TAU/response.
        let mut s = zest_config::Settings::default();
        s.motion.spring_response = 0.0;
        s.motion.spring_damping = 0.0;
        let c = Config::from(&s);
        assert!(c.spring_response >= 0.01 && c.spring_damping >= 0.1);

        let mut s = zest_config::Settings::default();
        s.motion.spring_response = 99.0;
        s.motion.spring_damping = 99.0;
        let c = Config::from(&s);
        assert!(c.spring_response <= 2.0 && c.spring_damping <= 2.0);
    }

    #[test]
    fn a_nonsense_spring_setting_falls_back_to_the_default() {
        // TOML accepts `nan` and `inf` as float literals, and `f32::clamp`
        // preserves NaN -- so without sanitizing, one character in a config
        // file produces a spring that never settles and an event loop that
        // never sleeps.
        let default = zest_config::Settings::default().motion;
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut s = zest_config::Settings::default();
            s.motion.spring_response = bad;
            s.motion.spring_damping = bad;
            let c = Config::from(&s);
            assert_eq!(c.spring_response, default.spring_response, "{bad} -> the schema default");
            assert_eq!(c.spring_damping, default.spring_damping);
        }
    }

    #[test]
    fn a_settled_scroll_spring_asks_for_no_frames() {
        // The 0%-idle guarantee, at the seam this group adds to it: a spring
        // that has arrived must report `false` from `moving()`, because that is
        // exactly the condition `anim_deadline` consults before scheduling a
        // wake. A spring that only ever approached its target would keep the
        // event loop awake for ever at a fraction of a pixel per frame.
        let mut spring = Spring::at(0.0);
        spring.nudge(30.0);
        spring.retarget(0.0);
        assert!(spring.moving(), "the fixture must actually be in motion");
        for _ in 0..600 {
            if !spring.step(1.0 / 60.0, 0.16, 1.0) {
                break;
            }
        }
        assert!(!spring.moving(), "a settled spring must stop asking for frames");
        assert_eq!(spring.value(), 0.0, "and land exactly home, so scroll_px is exactly zero");
    }
}



#[cfg(test)]
mod theme_following_tests {
    use super::{theme_id, Config};

    fn config_with(theme: &str, light: &str, follow: bool) -> Config {
        let mut s = zest_config::Settings::default();
        s.appearance.theme = theme.to_string();
        s.appearance.light_theme = light.to_string();
        s.appearance.follow_system_theme = follow;
        Config::from(&s)
    }

    #[test]
    fn following_off_ignores_the_os_entirely() {
        // Including when a light theme is named: configuring one is not the
        // same as asking for it to be used.
        let config = config_with("obsidian", "paper", false);
        assert_eq!(theme_id(&config, true), "obsidian");
        assert_eq!(theme_id(&config, false), "obsidian");
    }

    #[test]
    fn following_on_swaps_only_on_a_light_desktop() {
        let config = config_with("obsidian", "paper", true);
        assert_eq!(theme_id(&config, true), "paper");
        assert_eq!(theme_id(&config, false), "obsidian", "dark is still `theme`");
    }

    #[test]
    fn an_empty_light_theme_follows_theme_regardless() {
        // The schema's exact promise, and the reason this is not a fallback
        // waiting to be filled in: someone who deliberately chose a dark theme
        // and turned following on has not asked to be handed a light one
        // nobody picked. It is the shipped default, so getting it wrong would
        // give every new user a theme they never chose.
        let config = config_with("obsidian", "", true);
        assert_eq!(theme_id(&config, true), "obsidian");
        assert_eq!(theme_id(&config, false), "obsidian");
        assert!(
            zest_config::Settings::default().appearance.light_theme.is_empty(),
            "this is the default, so it is the path almost everyone takes"
        );
    }

    #[test]
    fn the_dark_theme_is_never_replaced_by_the_light_one() {
        // The inverse mistake: a dark desktop must never reach `light_theme`,
        // however it is configured.
        for follow in [true, false] {
            assert_eq!(theme_id(&config_with("nord", "paper", follow), false), "nord");
        }
    }
}

#[cfg(test)]
mod shell_settings_tests {
    use super::Config;

    fn config_with(cwd: &str, env: &[(&str, &str)]) -> Config {
        let mut s = zest_config::Settings::default();
        s.shell.cwd = cwd.to_string();
        s.shell.env = env.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect();
        Config::from(&s)
    }

    #[test]
    fn an_unset_working_directory_inherits_rather_than_spawning_in_nothing() {
        // `""` is the schema's "inherit", and it must not survive as a cwd:
        // an empty path is not a directory, so passing it through would fail
        // every spawn on a config nobody edited.
        assert_eq!(config_with("", &[]).shell_cwd, None);
        assert_eq!(config_with("   ", &[]).shell_cwd, None, "whitespace is not a directory either");
        assert_eq!(
            config_with("/tmp", &[]).shell_cwd,
            Some(std::path::PathBuf::from("/tmp")),
            "a real path reaches the spawn"
        );
    }

    #[test]
    fn an_empty_value_is_carried_through_as_an_unset() {
        // Both pty backends read an empty value as *unset* rather than "set to
        // the empty string" -- the convention `terminal_env` already relies on
        // to strip another terminal's stale identity. The projection must not
        // filter those out on the way, or the one way to remove an inherited
        // variable stops working.
        let config = config_with("", &[("STALE", ""), ("KEEP", "1")]);
        assert!(
            config.shell_env.contains(&("STALE".to_string(), String::new())),
            "an empty value is an instruction, not an absence"
        );
        assert!(config.shell_env.contains(&("KEEP".to_string(), "1".to_string())));
    }

    #[test]
    fn a_users_variable_beats_the_terminal_identity() {
        // The ordering decision, asserted rather than commented. Both backends
        // apply in order, so what matters is which entry comes *last*: a
        // `shell.env` entry the identity silently overrode would be a setting
        // that does nothing, which is the whole point of this work.
        let mut spec = zest_pty::CommandSpec::default_shell();
        assert!(
            spec.env.iter().any(|(k, _)| k == "TERM"),
            "the fixture must actually contain the entry being overridden, or \
             this test passes for a reason unrelated to the code"
        );
        super::apply_shell_settings(&mut spec, &config_with("", &[("TERM", "xterm-direct")]), &[]);
        let effective = spec.env.iter().rfind(|(k, _)| k == "TERM");
        assert_eq!(
            effective.map(|(_, v)| v.as_str()),
            Some("xterm-direct"),
            "the user's entry is applied last, so it is the one the child sees"
        );
    }

    #[test]
    fn a_users_variable_beats_shell_integrations_too() {
        // Copilot's catch on #305: `enable_shell_integration` appends
        // environment of its own -- zsh is hooked entirely through `ZDOTDIR` --
        // so applying the settings before it left the injection winning and the
        // documented precedence untrue. The integration now runs first and the
        // user's entries go last, which is the order `build_spec` uses.
        let mut spec = zest_pty::CommandSpec::default_shell();
        spec.env.push(("ZDOTDIR".into(), "/from/integration".into()));
        let injected = vec!["ZDOTDIR".to_string()];
        super::apply_shell_settings(
            &mut spec,
            &config_with("", &[("ZDOTDIR", "/from/user")]),
            &injected,
        );
        assert_eq!(
            spec.env.iter().rfind(|(k, _)| k == "ZDOTDIR").map(|(_, v)| v.as_str()),
            Some("/from/user"),
            "the user's entry is applied last, whatever put the first one there"
        );
    }

    #[test]
    fn the_working_directory_is_a_default_a_profile_can_still_overwrite() {
        // `build_spec` sets this, and `open_shell_tab` overwrites it with the
        // profile's `starting_directory` afterwards. Encoded here so the order
        // survives someone moving either line.
        let mut spec = zest_pty::CommandSpec::default_shell();
        super::apply_shell_settings(&mut spec, &config_with("/from/settings", &[]), &[]);
        assert_eq!(spec.cwd, Some(std::path::PathBuf::from("/from/settings")));
        spec.cwd = Some("/from/profile".into());
        assert_eq!(
            spec.cwd,
            Some(std::path::PathBuf::from("/from/profile")),
            "the more specific of the two wins, and it is applied second"
        );
    }

    #[test]
    fn the_defaults_ask_for_nothing() {
        // The shipped default must leave a spawn exactly as it was before this
        // was wired: no cwd, no extra environment.
        let config = Config::default();
        assert_eq!(config.shell_cwd, None);
        assert!(config.shell_env.is_empty());
    }
}

#[cfg(test)]
mod scrolling_tests {
    use super::{wheel_rows, Config};

    fn config_with(lines_per_notch: usize, scroll_on_keypress: bool) -> Config {
        let mut s = zest_config::Settings::default();
        s.scrolling.lines_per_notch = lines_per_notch;
        s.scrolling.scroll_on_keypress = scroll_on_keypress;
        Config::from(&s)
    }

    #[test]
    fn a_notch_moves_the_configured_number_of_rows() {
        assert_eq!(wheel_rows(1.0, 1), 1);
        assert_eq!(wheel_rows(1.0, 3), 3, "the shipped default, unchanged");
        assert_eq!(wheel_rows(1.0, 10), 10);
        assert_eq!(wheel_rows(2.0, 10), 20, "two notches in one event still scale");
    }

    #[test]
    fn the_sign_is_the_direction_and_survives_scaling() {
        // Both consumers need it: the scrollback move takes it as-is, and the
        // alternate screen picks its arrow key from it.
        assert_eq!(wheel_rows(-1.0, 5), -5);
        assert_eq!(wheel_rows(-3.0, 3), -9);
        assert_eq!(wheel_rows(1.0, 5).unsigned_abs(), wheel_rows(-1.0, 5).unsigned_abs());
    }

    #[test]
    fn an_absurd_delta_cannot_allocate_the_process_to_death() {
        // `count` bounds a loop *and* a `Vec::with_capacity`, and a float ->
        // int cast saturates in Rust rather than wrapping, so one synthetic
        // `PixelDelta` reaches `isize::MAX` rows and the allocation aborts the
        // process. Raising `lines_per_notch` to its ceiling of 50 multiplies
        // the reachable size by ~17 over the literal 3 this replaced, which is
        // what makes an old latent hazard worth closing now.
        assert_eq!(wheel_rows(f32::MAX, 50), super::MAX_WHEEL_ROWS);
        assert_eq!(wheel_rows(f32::MIN, 50), -super::MAX_WHEEL_ROWS);
        assert_eq!(wheel_rows(f32::INFINITY, 1), super::MAX_WHEEL_ROWS);
        assert_eq!(wheel_rows(f32::NEG_INFINITY, 1), -super::MAX_WHEEL_ROWS);
        // NaN casts to 0, and a gesture that moves nothing must stay nothing
        // rather than becoming a full-clamp jump in some arbitrary direction.
        assert_eq!(wheel_rows(f32::NAN, 3), 0);
    }

    #[test]
    fn the_clamp_is_far_above_any_real_gesture() {
        // The bound has to be unreachable in use or it is a scroll bug wearing
        // a safety hat: the fastest trackpad flick is tens of rows, and the
        // largest configurable step is 50.
        assert_eq!(wheel_rows(100.0, 50), 5_000, "a violent flick at the max step is untouched");
    }

    #[test]
    fn a_zero_step_still_scrolls() {
        // `Config::from` clamps to the schema's 1..=50, and the helper floors at
        // 1 again. A hand-edited `0` reaching either would be a wheel that does
        // nothing at all, which reads as a broken mouse rather than a setting.
        assert_eq!(config_with(0, true).lines_per_notch, 1);
        assert_eq!(wheel_rows(1.0, 0), 1);
        assert_eq!(config_with(9_000, true).lines_per_notch, 50);
    }

    /// What reaches the predictor is the key, never the bytes: a chord is
    /// `Other` even when it would encode to a printable-looking byte, a shifted
    /// letter is the letter, and a ZWJ sequence the IME hands over is not one
    /// character. The predictor's own width rule sits on top of this.
    #[test]
    fn predict_key_reads_the_key_not_the_bytes() {
        use super::{predict_key, ModifiersState};
        use winit::keyboard::{Key, NamedKey};
        use zest_proto::Key as P;
        let ev = |k: Key| k;
        let none = ModifiersState::empty();
        assert_eq!(predict_key(&ev(Key::Character("a".into())), none), P::Printable('a'));
        assert_eq!(
            predict_key(&ev(Key::Character("A".into())), ModifiersState::SHIFT),
            P::Printable('A'),
            "shift is part of the character"
        );
        assert_eq!(predict_key(&ev(Key::Named(NamedKey::Space)), none), P::Printable(' '));
        assert_eq!(predict_key(&ev(Key::Named(NamedKey::Backspace)), none), P::Backspace);
        assert_eq!(
            predict_key(&ev(Key::Character("c".into())), ModifiersState::CONTROL),
            P::Other,
            "^C echoes nothing a guess could stand for"
        );
        assert_eq!(predict_key(&ev(Key::Named(NamedKey::Enter)), none), P::Other);
        assert_eq!(predict_key(&ev(Key::Named(NamedKey::ArrowLeft)), none), P::Other);
        assert_eq!(
            predict_key(&ev(Key::Character("👨‍👩".into())), none),
            P::Other,
            "more than one code point is not one character"
        );
    }

    #[test]
    fn scroll_on_keypress_reaches_the_config() {
        // The flag the two typing sites read. Its default is on, because that
        // is what every terminal does.
        assert!(Config::default().scroll_on_keypress, "on by default, as everywhere");
        assert!(!config_with(3, false).scroll_on_keypress);
    }
}

#[cfg(test)]
mod drop_tests {
    use super::{drop_target, DropTarget};

    #[test]
    fn a_terminal_claims_nothing() {
        // The load-bearing case. A drop on a terminal must stay unclaimed so
        // "type the path" -- what every other terminal does with this gesture
        // -- is still available to implement. Setting a wallpaper instead would
        // take the gesture and could not be undone from the keyboard.
        assert_eq!(drop_target(false, false, None), DropTarget::Unclaimed);
        assert_eq!(drop_target(false, false, Some("powershell")), DropTarget::Unclaimed);
    }

    #[test]
    fn the_settings_tab_claims_it_for_the_window() {
        assert_eq!(drop_target(true, false, None), DropTarget::Window);
    }

    #[test]
    fn the_profiles_tab_claims_it_for_the_selected_profile() {
        assert_eq!(
            drop_target(false, true, Some("powershell")),
            DropTarget::Profile("powershell".to_string())
        );
        // Defaults is a profile like any other to the writer, so a drop there
        // sets the layer every profile falls through to -- which is the useful
        // answer, not an edge case to refuse.
        assert_eq!(
            drop_target(false, true, Some("defaults")),
            DropTarget::Profile("defaults".to_string())
        );
    }

    #[test]
    fn the_narrower_claim_wins_and_the_answer_never_depends_on_that() {
        // Both flags true should not be reachable -- they are different tabs --
        // but a rule whose answer changes when it becomes reachable is a bug
        // waiting for a refactor, so the order is pinned rather than assumed.
        assert_eq!(
            drop_target(true, true, Some("nightly")),
            DropTarget::Profile("nightly".to_string())
        );
    }

    #[test]
    fn a_profiles_tab_with_no_profile_selected_claims_nothing() {
        // `profiles_ui` is None between opening the tab and building its state.
        // Writing to `profiles.` with an empty name would create a table nobody
        // asked for.
        assert_eq!(drop_target(false, true, None), DropTarget::Unclaimed);
    }
}

#[cfg(test)]
mod palette_tests {
    use super::{pane_opacity, pane_selection_bg, seed_palette, to_core_palette};
    use crate::tabs::ProfileIdentity;

    /// An identity as the launcher will build one; only the palette-relevant
    /// fields matter here.
    fn identity(scheme: Option<&str>) -> ProfileIdentity {
        ProfileIdentity {
            name: "test".into(),
            scheme: scheme.map(str::to_string),
            selection_bg: scheme.and_then(crate::tabs::scheme_selection_wash),
            tab_color: None,
            icon: None,
            color_from: None,
            opacity: None,
            background_image: None,
            background_fit: None,
            background_dim: None,
            title: zest_config::TabTitle::FromShell,
        }
    }

    fn window() -> zest_core::PaletteSnapshot {
        to_core_palette(&zest_theme::resolve(&zest_theme::builtin::obsidian()))
    }

    fn nord() -> zest_core::PaletteSnapshot {
        to_core_palette(&zest_theme::resolve(&zest_theme::builtin::nord()))
    }

    #[test]
    fn a_tab_with_a_scheme_keeps_it_across_a_window_theme_change() {
        // THE bug this item exists for: apply_theme() reseeded every tab with
        // the window palette, so switching the window theme silently wiped a
        // profile tab's scheme. The reseed decision runs through this seam.
        let seed = seed_palette(&window(), Some(&identity(Some("nord"))));
        assert_eq!(
            seed,
            nord(),
            "a profile tab's seed is its scheme's palette, not the window's"
        );
        assert_ne!(seed, window(), "nord and obsidian differ, or this test proves nothing");
    }

    #[test]
    fn a_schemeless_tab_follows_the_window() {
        assert_eq!(
            seed_palette(&window(), Some(&identity(None))),
            window(),
            "an identity without a scheme is a window-palette tab"
        );
        assert_eq!(seed_palette(&window(), None), window(), "and so is a plain tab");
    }

    #[test]
    fn an_unknown_scheme_falls_back_to_the_window_palette() {
        // Never a failure: a profile naming a scheme that was deleted still
        // launches and still repaints — the never-crash rule.
        assert_eq!(seed_palette(&window(), Some(&identity(Some("no-such-scheme")))), window());
    }

    #[test]
    fn split_panes_seed_independently() {
        // One seed function, called per terminal: panes may later host
        // different profiles, and the seam must already answer per pane
        // rather than per tab.
        let left = seed_palette(&window(), Some(&identity(Some("nord"))));
        let right = seed_palette(&window(), Some(&identity(Some("paper"))));
        assert_ne!(left, right, "two panes, two schemes, two seeds");
    }

    #[test]
    fn selection_follows_the_scheme_not_the_window() {
        let obsidian = zest_theme::resolve(&zest_theme::builtin::obsidian());
        let win = zest_core::Rgb::new(
            obsidian.selection_bg.r,
            obsidian.selection_bg.g,
            obsidian.selection_bg.b,
        );
        let paper = zest_theme::resolve(&zest_theme::builtin::paper());
        let want =
            zest_core::Rgb::new(paper.selection_bg.r, paper.selection_bg.g, paper.selection_bg.b);
        assert_eq!(
            pane_selection_bg(win, Some(&identity(Some("paper")))),
            want,
            "a light scheme selected in a dark window's wash is unreadable"
        );
        assert_eq!(pane_selection_bg(win, None), win, "plain tabs keep the window's");
        assert_eq!(
            pane_selection_bg(win, Some(&identity(Some("no-such-scheme")))),
            win,
            "unknown falls back, never fails"
        );
    }

    #[test]
    fn the_render_path_reads_the_cached_wash_and_never_resolves_the_scheme() {
        // pane_selection_bg runs per pane per frame. Resolving the scheme
        // there meant a deleted scheme warned on every caret-blink repaint —
        // one warn every ~500ms, forever — and a valid one paid a theme
        // resolve + allocation per pane per frame. The wash is resolved once,
        // at identity (re-)resolve time; render must only read the cache.
        let win = zest_core::Rgb::new(1, 2, 3);

        // A real scheme but an empty cache: a render path that resolves
        // would return paper's wash here and betray a per-frame lookup.
        let mut id = identity(Some("paper"));
        id.selection_bg = None;
        assert_eq!(
            pane_selection_bg(win, Some(&id)),
            win,
            "render reads the cached wash, never the scheme name"
        );

        // The mirror: a dead scheme with a cache still serves the cache —
        // and, crucially, without re-running the unknown-scheme warn.
        let cached = zest_core::Rgb::new(9, 9, 9);
        let mut id = identity(Some("no-such-scheme"));
        id.selection_bg = Some(cached);
        assert_eq!(
            pane_selection_bg(win, Some(&id)),
            cached,
            "the cache is the render path's only source"
        );
    }

    #[test]
    fn per_tab_opacity_rides_the_viewport_not_the_window() {
        let mut id = identity(None);
        id.opacity = Some(0.5);
        assert!((pane_opacity(1.0, Some(&id)) - 0.5).abs() < f32::EPSILON);
        assert!(
            (pane_opacity(0.8, None) - 0.8).abs() < f32::EPSILON,
            "no identity, no override"
        );
        assert!(
            (pane_opacity(0.8, Some(&identity(None))) - 0.8).abs() < f32::EPSILON,
            "an identity without an opacity follows the window"
        );
        id.opacity = Some(7.0);
        assert!(
            (pane_opacity(1.0, Some(&id)) - 1.0).abs() < f32::EPSILON,
            "a hand-edited value is clamped, not trusted (never-crash rule)"
        );
    }
}





#[cfg(test)]
mod roster_menu_tests {
    use super::{MenuState, TextField};

    fn picker(options: &[&str], filter: &str) -> MenuState {
        let mut menu = MenuState::roster(
            0,
            options.iter().map(|s| (*s).to_string()).collect(),
            None,
        );
        menu.filter = TextField::new(filter);
        menu
    }

    #[test]
    fn filtering_matches_anywhere_in_the_name() {
        // Prefix matching would be useless here: the family is "MesloLGM NF"
        // and the words someone reaches for are "meslo", "nerd" or "mono",
        // only one of which starts it.
        let p = picker(&["Cascadia Mono", "MesloLGM NF", "MesloLGM Nerd Font"], "nerd");
        assert_eq!(p.matching(), vec!["MesloLGM Nerd Font"]);

        let p = picker(&["Cascadia Mono", "MesloLGM NF", "JetBrainsMono Nerd Font"], "mono");
        assert_eq!(
            p.matching(),
            vec!["Cascadia Mono", "JetBrainsMono Nerd Font"],
            "and it is case-insensitive"
        );
    }

    #[test]
    fn an_empty_filter_offers_everything() {
        let p = picker(&["A", "B", "C"], "");
        assert_eq!(p.matching().len(), 3);
    }

    #[test]
    fn a_filter_matching_nothing_offers_nothing_rather_than_everything() {
        // The failure that would be worse than useless: a typo silently
        // showing the whole list again, so Enter picks an arbitrary font.
        let p = picker(&["Cascadia Mono", "Consolas"], "zzz");
        assert!(p.matching().is_empty());
    }

    #[test]
    fn a_roster_field_gets_a_menu_even_though_the_schema_has_no_variants() {
        // The reported bug, at its root (#259): the menu builder bailed on
        // `field.variants.is_empty()`, and a theme roster comes from
        // `zest_theme::builtin::all()`, not from the schema. Arming the menu
        // therefore produced nothing at all — "left the theme pill dead" —
        // and the ⌘K command palette was used as the escape hatch, which is
        // why clicking a ▾ said "type to run a command".
        let fields = zest_config::ui::fields();
        let idx = fields
            .iter()
            .position(|f| f.key == "appearance.theme")
            .expect("appearance.theme exists");
        assert!(
            fields[idx].variants.is_empty(),
            "the roster is the client's, so the schema has no variants — the bail's premise"
        );
        let actions = vec![crate::settings_ui::RowAction::Field(idx)];
        let menu = MenuState::roster(
            0,
            vec!["obsidian".to_string(), "nord".to_string(), "paper".to_string()],
            Some("nord"),
        );
        let model = super::menu_model(&menu, &actions, &fields, Some("nord"), None)
            .expect("a roster field has a menu");
        assert_eq!(model.options.len(), 3, "every theme is an option");
        assert_eq!(model.current, Some(1), "the ✓ is on the theme that is set");
        assert_eq!(model.selected, 1, "and the keyboard opens on it, so Enter is a no-op");
    }

    #[test]
    fn a_choice_that_cannot_resolve_leaves_the_menu_alone() {
        // Enter on a search that matched nothing used to *close* the
        // dropdown having applied nothing, which reads as the menu breaking
        // rather than as the filter being wrong — and leaves the person to
        // reopen it and retype. `matching()` is what the choice resolves
        // against, so an out-of-range index has no answer and must be a
        // no-op, filter and all.
        let mut menu = MenuState::roster(
            0,
            vec!["obsidian".to_string(), "nord".to_string()],
            Some("nord"),
        );
        menu.filter = TextField::new("zzz");
        assert!(menu.matching().is_empty(), "nothing matches, so Enter has nothing to apply");
        assert_eq!(menu.matching().first(), None, "and the selection does not resolve");
    }

    #[test]
    fn a_searched_menu_offers_only_what_matched() {
        // `current` and `selected` index the *visible* options; resolving
        // them against the unfiltered roster is how a menu picks the wrong
        // entry the moment someone types.
        let fields = zest_config::ui::fields();
        let idx = fields
            .iter()
            .position(|f| f.key == "appearance.theme")
            .expect("appearance.theme exists");
        let actions = vec![crate::settings_ui::RowAction::Field(idx)];
        let mut menu = MenuState::roster(
            0,
            vec!["obsidian".to_string(), "nord".to_string(), "paper".to_string()],
            Some("nord"),
        );
        menu.filter = TextField::new("pa");
        let model = super::menu_model(&menu, &actions, &fields, Some("nord"), None)
            .expect("a menu, even with everything filtered out");
        assert_eq!(model.options.len(), 1, "only `paper` matches");
        assert_eq!(model.options[0].value, "paper");
        assert_eq!(model.current, None, "the set theme is not among them, so no ✓");
    }
}

#[cfg(test)]
mod tuning_tests {
    use super::{resolve_text_tuning, Config};
    use zest_render_wgpu::TextTuning;

    fn config(gamma: f32, contrast: f32) -> Config {
        let mut s = zest_config::Settings::default();
        s.appearance.text_gamma = gamma;
        s.appearance.text_contrast = contrast;
        Config::from(&s)
    }

    #[test]
    fn the_antialias_setting_reaches_the_font_system() {
        // The same hole #82 fell down: a setting that projects into `Config`
        // but is never read is a control that visibly does nothing.
        for (from, want) in [
            (zest_config::TextAntialias::Subpixel, zest_font::TextAntialias::Subpixel),
            (zest_config::TextAntialias::Grayscale, zest_font::TextAntialias::Grayscale),
        ] {
            let mut s = zest_config::Settings::default();
            s.appearance.text_antialias = from;
            assert_eq!(Config::from(&s).text_antialias, want, "{from:?} must survive");
        }
    }

    #[test]
    fn the_hinting_setting_reaches_the_font_system() {
        for (from, want) in [
            (zest_config::TextHinting::None, zest_font::Hinting::None),
            (zest_config::TextHinting::Full, zest_font::Hinting::Full),
        ] {
            let mut s = zest_config::Settings::default();
            s.appearance.text_hinting = from;
            assert_eq!(Config::from(&s).text_hinting, want, "{from:?} must survive");
        }
    }

    #[test]
    fn hinting_is_independent_of_antialiasing() {
        // The point of the setting: all four combinations are reachable. They
        // used to be welded together, which hid the one that matters -- at 9pt
        // it is grayscale + full that matches Windows Terminal, and that pair
        // was not expressible.
        let mut s = zest_config::Settings::default();
        s.appearance.text_antialias = zest_config::TextAntialias::Grayscale;
        s.appearance.text_hinting = zest_config::TextHinting::Full;
        let c = Config::from(&s);
        assert_eq!(c.text_antialias, zest_font::TextAntialias::Grayscale);
        assert_eq!(c.text_hinting, zest_font::Hinting::Full);
    }

    #[test]
    fn the_two_antialias_defaults_agree() {
        // `zest-font` cannot depend on `zest-config`, so the shipping default
        // is written twice. This is the only place the two meet.
        let s = zest_config::Settings::default();
        assert_eq!(
            Config::from(&s).text_antialias,
            zest_font::TextAntialias::default(),
            "the schema default and the font layer's default have drifted apart"
        );
    }

    #[test]
    fn a_translucent_window_refuses_subpixel_text() {
        // Per-channel coverage against a translucent destination is undefined:
        // the compositor holds one alpha and cannot divide by three. This is
        // the gate, and it must win over the setting.
        let mut s = zest_config::Settings::default();
        s.appearance.text_antialias = zest_config::TextAntialias::Subpixel;
        s.window.opacity = 0.85;
        let cfg = Config::from(&s);
        assert_eq!(cfg.text_antialias, zest_font::TextAntialias::Subpixel, "the setting stands");
        assert_eq!(
            super::App::antialias_for(&cfg),
            zest_font::TextAntialias::Grayscale,
            "but a translucent window must force grayscale anyway"
        );

        // The second opacity is the same gate. The atlas holds one mask format
        // for the whole window, so glass chrome over a solid grid still has to
        // pick, and only grayscale is defined against a translucent
        // destination.
        let mut s = zest_config::Settings::default();
        s.appearance.text_antialias = zest_config::TextAntialias::Subpixel;
        s.window.chrome_opacity = 0.3;
        assert_eq!(
            super::App::antialias_for(&Config::from(&s)),
            zest_font::TextAntialias::Grayscale,
            "a translucent *chrome* makes the surface translucent too"
        );
    }

    #[test]
    fn either_opacity_makes_the_surface_translucent() {
        // Four callers ask this -- `with_transparent` and the swapchain's
        // `want_transparency`, both in `open_window`, `apply_transparency` on
        // a reload, and `antialias_for` -- and the way the second opacity gets
        // forgotten is each spelling `opacity < 1.0` for itself. `chrome_opacity` below 1 is what makes `window.backdrop`
        // visible at `opacity = 1`: a material can only show through pixels the
        // surface leaves transparent.
        let surface = |opacity: f32, chrome: f32| {
            let mut s = zest_config::Settings::default();
            s.window.opacity = opacity;
            s.window.chrome_opacity = chrome;
            Config::from(&s).translucent_surface()
        };
        assert!(!surface(1.0, 1.0), "both solid: an opaque surface, as before");
        assert!(surface(0.8, 1.0), "a translucent grid");
        assert!(surface(1.0, 0.3), "a glass titlebar over a solid grid -- #522");
        assert!(surface(0.8, 0.3), "both");
    }

    #[test]
    fn the_setting_reaches_the_renderer() {
        // The whole complaint in #82: `Config::from(&Settings)` did not carry
        // these across at all, and `Renderer::tuning` was assigned
        // `TextTuning::default()` once and never touched -- so both controls
        // repainted and changed nothing.
        let t = resolve_text_tuning(&config(1.8, 0.25));
        assert!((t.gamma - 1.8).abs() < f32::EPSILON, "gamma must survive the projection");
        assert!((t.contrast - 0.25).abs() < f32::EPSILON, "and so must contrast");
    }

    #[test]
    fn the_two_defaults_agree() {
        // They were 1.0 in the schema and 1.3 in the renderer, with nothing
        // connecting them, so the schema documented a number that was never
        // applied. `zest-config` cannot reference the renderer's constant --
        // it does not depend on it, and should not -- so this is the only
        // place the two can be compared. If either moves, this fails.
        let s = zest_config::Settings::default();
        assert!(
            (s.appearance.text_gamma - TextTuning::DEFAULT_GAMMA).abs() < f32::EPSILON,
            "settings default {} != renderer default {}",
            s.appearance.text_gamma,
            TextTuning::DEFAULT_GAMMA
        );
        assert!((s.appearance.text_contrast - TextTuning::DEFAULT_CONTRAST).abs() < f32::EPSILON);
    }

    #[test]
    fn absurd_values_are_clamped_rather_than_trusted() {
        // Config is a file a person edits by hand, and a gamma of zero is a
        // division by zero in the shader.
        let t = resolve_text_tuning(&config(-5.0, 99.0));
        assert!((0.5..=2.5).contains(&t.gamma), "gamma clamped, got {}", t.gamma);
        assert!((0.0..=1.0).contains(&t.contrast), "contrast clamped, got {}", t.contrast);
    }
}


#[cfg(test)]
mod run_on_host_tests {
    use super::{with_return, Pending};
    use crate::tabs::{placeholder_addr, Tab};

    #[test]
    fn a_command_reaches_the_shell_the_way_a_person_would_send_it() {
        // A shell runs a line when it sees the Return, not when it sees the
        // bytes — the same thing the ⏎-here path has always written.
        assert_eq!(with_return("ls -la"), b"ls -la\r".to_vec());
        // No trailing newline of its own, and no interpretation: whatever the
        // block recorded is what runs. A command with an embedded quote or a
        // trailing backslash is the far shell's problem, exactly as it was
        // when it ran the first time.
        assert_eq!(with_return("echo \"a b\""), b"echo \"a b\"\r".to_vec());
        assert_eq!(with_return(""), b"\r".to_vec(), "an empty command is a bare Return");
    }

    #[test]
    fn an_armed_command_is_taken_once_and_only_once() {
        // "Taking is the point": a command runs once. If it survived being
        // read, a tab that got adopted twice — a refused duplicate healing a
        // persisted tabs.json, say — would run it again, and re-running a
        // `rm` because a strip reconciled is the failure worth designing out.
        let mut tab = Tab::pending_for_test(placeholder_addr(1))
            .with_pending_input(Some(with_return("make ship")));
        assert_eq!(tab.take_pending_input(), Some(b"make ship\r".to_vec()));
        assert_eq!(tab.take_pending_input(), None, "a command runs once");
    }

    #[test]
    fn a_tab_with_nothing_armed_stays_empty() {
        // Every ordinary tab: ⌘T, a restore, a picker attach with nothing
        // pending. Arming is the exception, and an unarmed tab must not write
        // a stray Return into somebody's shell.
        let mut tab = Tab::pending_for_test(placeholder_addr(1));
        assert_eq!(tab.take_pending_input(), None);
        let mut tab = Tab::pending_for_test(placeholder_addr(2)).with_pending_input(None);
        assert_eq!(tab.take_pending_input(), None);
    }

    #[test]
    fn a_remote_attach_pins_the_host_it_dials_and_a_local_one_does_not() {
        // `expect_host` is what checks the machine that answered is the one
        // the roster claimed — the reason the host signs first, so a client
        // can hang up before revealing anything. A remote attach without it
        // lets a stale or poisoned route attach a session to the wrong
        // machine, which is what routing this arm through
        // `spawn_tab_worker_pinned` briefly did (it passed `None`).
        //
        // The rule, as both call sites compute it: pin every remote, never
        // the loopback one — our own socket's permissions are the answer
        // there, and `AttachOptions::expect_host` is documented `None` on
        // loopback for exactly that reason.
        let pin = |route: &crate::route::HostRoute, addr: zest_proto::SessionAddr| {
            (!route.is_local()).then_some(addr.host)
        };
        let far = zest_proto::SessionAddr::new(
            zest_proto::HostId::from_bytes([7; 32]),
            zest_proto::SessionId(1),
        );
        assert_eq!(
            pin(&crate::route::HostRoute::Tcp("10.0.0.7:7717".into()), far),
            Some(zest_proto::HostId::from_bytes([7; 32])),
            "a LAN attach names the machine it expects"
        );
        assert_eq!(
            pin(
                &crate::route::HostRoute::Relay {
                    host: zest_proto::HostId::from_bytes([7; 32]),
                    relay_origin: "wss://relay.example".into(),
                },
                far,
            ),
            Some(zest_proto::HostId::from_bytes([7; 32])),
            "and so does a tunnelled one — the pipe is not the peer"
        );
        assert_eq!(
            pin(&crate::route::HostRoute::LocalSocket("/tmp/s".into()), far),
            None,
            "loopback pins nothing: reaching the socket is the authorization"
        );
    }

    #[test]
    fn the_two_pending_kinds_are_distinct() {
        // One slot, two riders (§12's ask_host profile and §6's block
        // command), and a host row has to tell them apart: one launches a
        // profile, the other opens a shell and types. Conflating them would
        // make ⇧⏎ on a block launch a profile named after the command.
        assert_ne!(
            Pending::Profile("nightly".into()),
            Pending::Command("nightly".into()),
            "same string, different intent"
        );
        // And the third rider (#436) carries nothing but its intent: a host
        // row with it becomes a pane of the active tab, never a tab.
        assert_ne!(Pending::Split, Pending::Command(String::new()));
    }
}


#[cfg(test)]
mod presence_tests {
    use super::{presence_of, tab_presence, TabOrigin, TabPresence};
    use crate::fleet::FleetHost;
    use zest_mesh::discovery::Presence;
    use zest_proto::HostId;

    fn host(id: u8, label: &str, presence: Presence) -> FleetHost {
        let mut h = zest_fleet::fixture::host(id, label);
        h.presence = presence;
        h
    }

    /// A settled remote tab's origin: id `[id; 32]`, matching the fixture's.
    fn remote(id: u8, label: &str) -> TabOrigin {
        TabOrigin::Remote { host: HostId::from_bytes([id; 32]), label: label.to_string() }
    }

    /// A launch still connecting: no id yet, only a name.
    fn connecting(label: &str) -> TabOrigin {
        TabOrigin::Remote { host: HostId::from_bytes([0; 32]), label: label.to_string() }
    }

    fn with_local(rest: Vec<FleetHost>) -> Vec<FleetHost> {
        let mut all = vec![zest_fleet::fixture::local(1, "studio")];
        all.extend(rest);
        all
    }

    #[test]
    fn the_host_dropdown_lists_this_machine_once_and_never_drops_a_pin() {
        use super::{host_menu_roster, HOST_MENU_LOCAL};
        let fleet = with_local(vec![host(2, "forge", Presence::Online)]);

        assert_eq!(
            host_menu_roster(&fleet, None),
            Some(vec![HOST_MENU_LOCAL.to_string(), "forge".to_string()]),
            "this machine appears once, by the spelling that writes an empty host — \
             listing it again under `studio` would be two rows doing one thing"
        );

        // A pin the fleet has never heard of: a machine that is off, or a
        // typo. Either way the profile must stay editable, and must not lose
        // what it has to whatever the menu happens to open on.
        let roster = host_menu_roster(&fleet, Some("nowhere")).expect("a roster");
        assert!(roster.contains(&"nowhere".to_string()), "{roster:?}");

        // And a pin differing only by case is the machine already listed — the
        // same ASCII fold `resolve_host` and `bucket_for` use.
        let roster = host_menu_roster(&fleet, Some("FORGE")).expect("a roster");
        assert_eq!(roster.len(), 2, "no duplicate for a case difference: {roster:?}");
    }

    #[test]
    fn a_host_labelled_like_the_local_row_keeps_the_field_a_text_edit() {
        use super::{host_menu_roster, HOST_MENU_LOCAL};
        // Labels are arbitrary text — mDNS `lbl` is whatever the far machine
        // advertises — so a host really called "(this machine)" is legal. The
        // menu carries one string per option, so the choice comes back as text
        // with no way to tell the two apart: picking that host would silently
        // *clear* the pin it meant to set.
        //
        // The parentheses were supposed to prevent this and cannot; the first
        // version of that comment claimed they were enough.
        let fleet = with_local(vec![host(2, HOST_MENU_LOCAL, Presence::Online)]);
        assert_eq!(
            host_menu_roster(&fleet, None),
            None,
            "a wrong write is worse than no dropdown — and a menu whose rows a \
             person could not tell apart either is not much of a menu"
        );

        // And from the other direction, which is the half the first guard
        // missed: a profile *pinned to* that name, for a machine that is
        // simply offline and absent from the snapshot. It folds onto the local
        // row, so opening the menu and pressing Enter — the gesture that is
        // supposed to be a no-op — would rewrite a real pin to empty.
        let fleet = with_local(vec![host(2, "forge", Presence::Online)]);
        assert_eq!(host_menu_roster(&fleet, Some(HOST_MENU_LOCAL)), None);
        assert_eq!(
            host_menu_roster(&fleet, Some("(THIS MACHINE)")),
            None,
            "case-insensitively, like every other label comparison here"
        );
    }

    #[test]
    fn nothing_but_this_machine_is_not_worth_a_dropdown() {
        use super::host_menu_roster;
        // The fleet has not started, or there is genuinely nowhere else. A
        // one-row dropdown is worse than the text field it replaced.
        assert_eq!(host_menu_roster(&with_local(Vec::new()), None), None);
        assert_eq!(host_menu_roster(&[], None), None, "and before the fleet exists at all");
    }

    #[test]
    fn the_dropdown_opens_on_the_pin_it_already_has_however_it_is_spelled() {
        use super::{host_menu_roster, host_menu_selection, HOST_MENU_LOCAL};
        // `MenuState` selects and marks the ✓ by *exact* string equality, so a
        // pin written `Forge` against a host advertising `forge` matches
        // nothing: the menu opens on row 0 and Enter rewrites the pin to
        // "(this machine)". Losing a ✓ is cosmetic; a destructive default is
        // not.
        let fleet = with_local(vec![host(2, "forge", Presence::Online)]);
        let roster = host_menu_roster(&fleet, Some("Forge")).expect("a roster");
        assert_eq!(
            host_menu_selection(&roster, Some("Forge")),
            "forge",
            "folded to the roster's own spelling, which is what exact equality will find"
        );
        assert!(
            roster.iter().any(|r| r == "forge"),
            "and that spelling really is in the list: {roster:?}"
        );

        // No pin, and a pin the fleet has never heard of, both land somewhere
        // real: the local row, and the appended row respectively.
        assert_eq!(host_menu_selection(&roster, None), HOST_MENU_LOCAL);
        // And the on-disk spelling of "no pin" is an *empty string*, not the
        // menu's label — so the same fold has to carry the ✓, or the row every
        // unpinned profile lands on is the one row that never shows a tick.
        assert_eq!(host_menu_selection(&roster, Some("")), HOST_MENU_LOCAL);
        let roster = host_menu_roster(&fleet, Some("nowhere")).expect("a roster");
        assert_eq!(host_menu_selection(&roster, Some("nowhere")), "nowhere");
    }

    #[test]
    fn a_machine_that_stopped_answering_makes_its_tabs_say_so() {
        // Three of `TabPresence`'s four variants had never been produced for a
        // tab: `presence` was hard-coded `Online`, so the chip's
        // "· unreachable" was drawn by code nothing could reach. A session on
        // a machine whose port had stopped answering (#22's dead daemon behind
        // a live mDNS record) read exactly like one on a healthy machine until
        // you typed into it.
        let fleet = [host(2, "forge", Presence::Unreachable)];
        assert_eq!(tab_presence(&remote(2, "forge"), &fleet), TabPresence::Unreachable);

        let fleet = [host(2, "forge", Presence::Away)];
        assert_eq!(tab_presence(&remote(2, "forge"), &fleet), TabPresence::Away);
    }

    #[test]
    fn reachability_outranks_discoverys_word() {
        // #237's rule, and the reason `presence_of` exists rather than a match
        // at each call site: a machine reachable only through the relay has no
        // discovery presence to be `Online` in, and calling it *unseen* while
        // clicking it opens a shell is what that issue was reported for.
        let mut relayed = host(2, "pi", Presence::Unseen);
        relayed.address = None;
        relayed.relay_online = true;
        assert_eq!(presence_of(&relayed), TabPresence::Online);
        assert_eq!(tab_presence(&remote(2, "pi"), &[relayed]), TabPresence::Online);
    }

    #[test]
    fn a_tab_resolves_its_machine_by_id_not_by_display_name() {
        // Two machines may share a label — the keying bug #268 fixed twice, in
        // the launcher's group map and again in its provenance lookup. A tab's
        // origin carries the id of the machine it is actually attached to.
        // The default fixture fleet IS the trap: both remotes are called
        // `mac`, so a lookup that slid back to the label cannot pass here.
        let mut fleet = zest_fleet::fixture::fleet();
        fleet[2].presence = Presence::Unreachable;
        assert_eq!(
            tab_presence(&remote(3, "mac"), &fleet),
            TabPresence::Unreachable,
            "the tab on the refusing machine says so, whatever the other one called itself"
        );
        assert_eq!(tab_presence(&remote(2, "mac"), &fleet), TabPresence::Online);
    }

    #[test]
    fn a_connecting_tab_falls_back_to_its_label() {
        // A placeholder address has no real host id yet. Those tabs draw faint
        // under `connecting` anyway, so the worst case is a dot that catches up
        // when the dial settles — but reading the *wrong* machine's presence
        // would be worse than reading none.
        let fleet = [host(2, "forge", Presence::Unreachable)];
        assert_eq!(
            tab_presence(&connecting("forge"), &fleet),
            TabPresence::Unreachable,
            "matched by label, since there is no id to match by"
        );

        // And never onto the *local* row. The origin is already `Remote`, so a
        // local match is definitionally wrong — and the worst wrong answer
        // available, since the local row is loopback and `Online`: a tab
        // connecting to a machine that happens to share this one's display name
        // would read as reaching the desk it is sitting on.
        let fleet = [
            zest_fleet::fixture::local(1, "twin"),
            host(2, "twin", Presence::Unreachable),
        ];
        assert_eq!(
            super::fleet_host_of(&connecting("twin"), &fleet).map(|h| h.host),
            Some(HostId::from_bytes([2; 32])),
            "the remote twin, not the one under our hands"
        );
        assert_eq!(tab_presence(&connecting("twin"), &fleet), TabPresence::Unreachable);
    }

    #[test]
    fn presence_and_link_resolve_the_same_machine() {
        use super::fleet_host_of;
        // These were two lookups matching differently: `presence` by id,
        // `LinkKind` by exact label. With duplicate labels a tab could report
        // host A's presence beside host B's route — contradictory chrome about
        // one machine, which is worse than either fact being missing. The
        // default fixture fleet supplies the duplicate labels.
        let mut fleet = zest_fleet::fixture::fleet();
        fleet[2].presence = Presence::Unreachable;
        fleet[2].reachability = Some(zest_mesh::Reachability::Cloud);

        let found = fleet_host_of(&remote(3, "mac"), &fleet).expect("the second mac");
        assert_eq!(found.host, HostId::from_bytes([3; 32]), "by id, not by name");
        assert_eq!(
            found.reachability,
            Some(zest_mesh::Reachability::Cloud),
            "so the route and the presence describe one machine"
        );
        assert_eq!(tab_presence(&remote(3, "mac"), &fleet), TabPresence::Unreachable);

        // A local tab has no fleet row to resolve: its link is loopback by
        // construction and its presence is Online.
        assert!(fleet_host_of(&TabOrigin::Local, &fleet).is_none());
    }

    #[test]
    fn the_local_tab_is_online_and_a_broken_socket_does_not_change_that() {
        // Presence is about the machine; `LinkKind` is about our connection to
        // it. A daemon that is up while our socket is down is `Online` and
        // `Reconnecting`, and saying both is the point — collapsing them would
        // lose exactly the distinction that tells you whether to wait or to go
        // and look.
        assert_eq!(tab_presence(&TabOrigin::Local, &[]), TabPresence::Online);
    }

    #[test]
    fn a_host_nothing_can_vouch_for_is_unseen_rather_than_online() {
        // We are attached to it, so it was reachable once — but no discovery
        // record and no account row means nothing here can say it still is,
        // and `Online` would be a claim rather than an observation.
        assert_eq!(tab_presence(&remote(9, "ghost"), &[]), TabPresence::Unseen);
    }
}


#[cfg(test)]
mod attention_tests {
    use super::attention_is_news;

    #[test]
    fn a_signal_in_the_tab_you_are_looking_at_is_not_news() {
        assert!(
            !attention_is_news(true, true),
            "the active tab of a focused window is being looked at"
        );
    }

    #[test]
    fn an_unfocused_window_is_not_looking_at_anything() {
        // The half that is easy to drop, and the one that matters most: the
        // bell that arrives while zesterm sits behind a browser is exactly
        // the case the dot exists for. A rule written on the active tab alone
        // is silent precisely when it is needed.
        assert!(attention_is_news(true, false), "even the active tab, if nobody is here");
        assert!(attention_is_news(false, false));
    }

    #[test]
    fn a_background_tab_is_always_news() {
        assert!(attention_is_news(false, true), "you are looking at a different tab");
    }
}

#[cfg(test)]
mod close_policy_tests {
    use super::{close_policy, what_is_running, ClosePolicy, TabFacts};
    use zest_config::settings::CloseAction;

    /// A live, idle, local tab — the case every other one varies from.
    const fn live() -> TabFacts {
        TabFacts {
            already_exited: false,
            dead: false,
            local: true,
            connecting: false,
            busy: false,
            can_detach: true,
        }
    }

    #[test]
    fn the_default_is_exactly_what_the_app_did_before() {
        // The one property that must survive this change: someone who never
        // opens Settings must not find ⌘W has quietly started meaning
        // something else.
        assert_eq!(
            close_policy(CloseAction::Kill, true, live()),
            ClosePolicy::Close,
            "an idle local tab closes without a question, as it always has"
        );
        let cfg = zest_config::settings::Tabs::default();
        assert_eq!(cfg.close_action, CloseAction::Kill, "and that is the shipped default");
        assert!(cfg.confirm_close_when_busy);
    }

    #[test]
    fn nothing_of_ours_to_end_is_never_a_question() {
        // Four different reasons there is no decision to make, and all four
        // must reach `Close` — which *drops* rather than kills. A modal here
        // would be a question with one answer, and it would fire on the exit
        // path (`Wakeup::TabExited`), where nobody pressed anything at all.
        for (name, facts) in [
            ("an exited child", TabFacts { already_exited: true, busy: true, ..live() }),
            ("a session the host says is gone", TabFacts { dead: true, busy: true, ..live() }),
            ("a shell on another machine", TabFacts { local: false, busy: true, ..live() }),
            ("a tab still being dialled", TabFacts { connecting: true, busy: true, ..live() }),
        ] {
            for action in [CloseAction::Kill, CloseAction::Detach, CloseAction::Ask] {
                assert_eq!(
                    close_policy(action, true, facts),
                    ClosePolicy::Close,
                    "{name}, under {action:?}"
                );
            }
        }
    }

    #[test]
    fn busy_is_what_turns_a_close_into_a_question() {
        assert_eq!(
            close_policy(CloseAction::Kill, true, TabFacts { busy: true, ..live() }),
            ClosePolicy::Ask,
            "a running command is the whole reason to stop someone"
        );
        assert_eq!(
            close_policy(CloseAction::Kill, false, TabFacts { busy: true, ..live() }),
            ClosePolicy::Close,
            "and switching the confirm off really does switch it off"
        );
    }

    #[test]
    fn detach_needs_no_confirmation_because_it_destroys_nothing() {
        // The asymmetry is the point: the question exists because one answer
        // is irreversible. Configured to detach, ⌘W is not.
        assert_eq!(
            close_policy(CloseAction::Detach, true, TabFacts { busy: true, ..live() }),
            ClosePolicy::Detach
        );
        assert_eq!(close_policy(CloseAction::Detach, true, live()), ClosePolicy::Detach);
    }

    #[test]
    fn detach_cannot_be_honoured_where_there_is_nothing_to_detach_to() {
        // The setting must never be the thing that ends a shell. An
        // in-process pty has no daemon holding it, so `finish_close_tab`'s
        // "detach" is a drop, and a drop hangs it up — `Detach` here would be
        // the option chosen *to avoid* killing quietly killing.
        let no_daemon = TabFacts { can_detach: false, ..live() };
        assert_eq!(
            close_policy(CloseAction::Detach, true, no_daemon),
            ClosePolicy::Close,
            "closing is all closing can mean without a daemon"
        );
        assert_eq!(
            close_policy(CloseAction::Detach, true, TabFacts { busy: true, ..no_daemon }),
            ClosePolicy::Close,
            "and being busy does not conjure a daemon"
        );
        // The setting still works where it can be honoured.
        assert_eq!(close_policy(CloseAction::Detach, true, live()), ClosePolicy::Detach);
    }

    #[test]
    fn ask_asks_even_when_nothing_is_running() {
        // `Ask` is the answer to "which of these did you mean", not to "are
        // you sure" — so it does not consult `busy`, and `confirm_when_busy`
        // cannot switch it off.
        assert_eq!(close_policy(CloseAction::Ask, false, live()), ClosePolicy::Ask);
        assert_eq!(
            close_policy(CloseAction::Ask, false, TabFacts { busy: true, ..live() }),
            ClosePolicy::Ask
        );
    }

    #[test]
    fn the_question_names_what_it_would_end() {
        assert_eq!(what_is_running(Some("cargo build --release"), false), "cargo build --release");
        assert_eq!(what_is_running(Some("  npm test  "), false), "npm test", "trimmed");
        // The alternate screen records no OSC 133 markers at all, so this is
        // the *usual* case for a TUI rather than an edge one.
        assert_eq!(what_is_running(None, true), "A full-screen program");
        assert_eq!(what_is_running(Some("   "), true), "A full-screen program");
        // A running block whose command text never arrived.
        assert_eq!(what_is_running(None, false), "A command");
    }
}

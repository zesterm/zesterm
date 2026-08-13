//! The winit application: window, surface, and the frame loop.

use std::sync::Arc;

use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::keyboard::ModifiersState;
use winit::window::{Window, WindowId};

use zest_font::{Fonts, Typography};
use zest_pty::{CommandSpec, PtySize};
use zest_render_wgpu::{Chrome, Renderer, Scene, Viewport};

use zest_input::{key, mouse, select, MouseState};
use crate::block_actions;
use crate::pipeline_cache;
use crate::chrome::hit::{CaptionButton, HitRegion};
use crate::chrome::layout::ChromeLayout;
use crate::chrome::model::{
    ChromeMetrics, ChromeModel, TabModel, TabOrigin, TabPresence, WindowControls,
};
use crate::chrome::theme::ChromeColors;
use crate::chrome::Insets;
use crate::keymap;
use crate::platform;
use crate::session::{Session, Wakeup};
use crate::source::{Origin, SessionSource};
use crate::tabs::{Tab, TabStrip};


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
const DAEMON_START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

pub struct Config {
    pub font_families: Vec<String>,
    pub typography: Typography,
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
    pub scrollback: usize,
    pub opacity: f32,
    /// Space between the window edge and the grid, in logical pixels.
    pub padding: u32,
    /// The strip's own alpha, independent of the grid's (ADR-003).
    pub chrome_opacity: f32,
    /// Draw our own titlebar: no OS caption, caption buttons and resize edges
    /// out of the chrome's own layout pass. Resolved from the tri-state
    /// setting here, so nothing downstream has to know what `Auto` means.
    pub custom_chrome: bool,
    /// What the compositor puts behind the window (Mica and friends).
    pub backdrop: zest_config::settings::Backdrop,
    /// The tab strip's knobs, taken whole — the chrome reads all of them.
    pub tabs: zest_config::settings::Tabs,
    pub shell: Option<String>,
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
            scrollback: s.scrolling.scrollback,
            opacity: s.window.opacity.clamp(0.0, 1.0),
            padding: s.window.padding.min(64),
            chrome_opacity: s.window.chrome_opacity.clamp(0.0, 1.0),
            // `Auto` means borderless on Windows and nowhere else. macOS
            // already gets its integrated look from the transparent
            // full-size titlebar, which is strictly better than borderless
            // there (WS-C2); Linux has no implementation yet.
            custom_chrome: match s.window.custom_chrome {
                zest_config::settings::CustomChrome::On => true,
                zest_config::settings::CustomChrome::Off => false,
                zest_config::settings::CustomChrome::Auto => cfg!(windows),
            },
            backdrop: s.window.backdrop,
            tabs: s.tabs.clone(),
            shell: (!s.shell.command.is_empty()).then(|| s.shell.command.clone()),
            cursor_blink: s.cursor.blink,
            cursor_blink_interval_ms: s.cursor.blink_interval_ms.clamp(100, 5000) as u32,
            scroll_on_output: s.scrolling.scroll_on_output,
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

/// How this window dials the daemon its tabs live on.
///
/// One route per window today — ⌘T opens on the same daemon the window is
/// attached to, which *is* the "current tab's host" rule while every tab
/// shares a host. The fleet model replaces this with per-host routes.
#[derive(Clone)]
enum HostRoute {
    /// This machine's daemon socket.
    LocalSocket(String),
    /// Another machine's daemon at `host:port` (`--attach`).
    Tcp(String),
}

impl HostRoute {
    fn is_local(&self) -> bool {
        matches!(self, HostRoute::LocalSocket(_))
    }

    fn dialer(&self) -> crate::remote::Dialer {
        match self {
            HostRoute::LocalSocket(path) => {
                let path = path.clone();
                Box::new(move || {
                    let a = crate::daemon::find_or_spawn(&path, DAEMON_START_TIMEOUT)
                        .map_err(|e| crate::remote::RemoteError::Io(e.to_string()))?;
                    Ok((a.read, a.write))
                })
            }
            HostRoute::Tcp(addr) => {
                let addr = addr.clone();
                Box::new(move || {
                    let stream = std::net::TcpStream::connect(&addr)
                        .map_err(|e| crate::remote::RemoteError::Io(e.to_string()))?;
                    // A terminal's writes are keystrokes: small, latency-bound,
                    // never worth coalescing.
                    let _ = stream.set_nodelay(true);
                    let read = stream
                        .try_clone()
                        .map_err(|e| crate::remote::RemoteError::Io(e.to_string()))?;
                    Ok((
                        Box::new(read) as Box<dyn std::io::Read + Send>,
                        Box::new(stream) as Box<dyn std::io::Write + Send>,
                    ))
                })
            }
        }
    }
}

/// The picker's transient state while it is open, and the action list
/// parallel to the drawn rows — built in the same pass as the row models, so
/// index `n` means the same thing in both by construction.
struct PickerState {
    selected: usize,
    filter: String,
    scroll: f32,
    /// Bring the selection into view on the next layout — keyboard only.
    scroll_to_selected: bool,
    actions: Vec<PickerAction>,
    /// A profile waiting for this picker to choose its host (`ask_host`,
    /// design §12): picking a host row launches the profile there instead
    /// of a bare shell. On the picker's state, not the app's, so it dies
    /// with the picker — a stale pending launch surviving a dismissal would
    /// hijack the next ⌘K's host row.
    pending_profile: Option<String>,
}

/// The command palette's transient state while it is open, and the action
/// list parallel to the drawn rows — built in the same `keymap::palette`
/// pass, so index `n` means the same thing in both by construction. `None`
/// entries are headers and reference rows the selection skips.
struct PaletteState {
    selected: usize,
    filter: String,
    scroll: f32,
    /// Bring the selection into view on the next layout — keyboard
    /// navigation only, never the wheel.
    scroll_to_selected: bool,
    actions: Vec<Option<keymap::Action>>,
}

/// The Settings tab's state — created when the tab opens, dropped when it
/// closes, surviving activation changes in between: the tab is a place you
/// sit in (design §11), and its selection, filter and buffers belong to it.
struct SettingsUiState {
    selected: usize,
    /// The rail's selected category, by label — a label, not an index,
    /// because the filter hides empty categories and an index would slide.
    category: String,
    filter: String,
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
    /// An open dropdown menu: (row index, keyboard selection).
    menu: Option<(usize, usize)>,
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
        if self.menu.is_some() {
            return;
        }
        if let Some(edit) = self.editing.as_mut() {
            edit.buffer.push_str(text);
            edit.error = false;
        } else {
            self.filter.push_str(text);
            self.selected = 0;
            self.scroll_to_selected = true;
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
    filter: String,
    scroll: f32,
    /// Bring the selection into view on the next layout — keyboard only.
    scroll_to_selected: bool,
    /// Parallel to the drawn rows, same-pass built (the picker discipline).
    actions: Vec<crate::settings_ui::RowAction>,
    /// `profiles::fields()`, cached at open like the settings walk.
    fields: Vec<zest_config::ui::UiField>,
    /// A typed edit in progress; while `Some`, characters belong to it.
    editing: Option<crate::settings_ui::EditBuffer>,
    /// An open dropdown menu: (row index, keyboard selection) — backdrop's.
    menu: Option<(usize, usize)>,
    /// The last profile write that failed, shown as a banner.
    error: Option<String>,
}

impl ProfilesUiState {
    /// Composed (IME) text, routed exactly like the Settings tab's: menu
    /// swallows, an open buffer takes it, otherwise the filter.
    fn commit_text(&mut self, text: &str) {
        if self.menu.is_some() {
            return;
        }
        if let Some(edit) = self.editing.as_mut() {
            edit.buffer.push_str(text);
            edit.error = false;
        } else {
            self.filter.push_str(text);
            self.selected = 0;
            self.scroll_to_selected = true;
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

/// Choosing one value from a long list, drawn through the command palette's
/// overlay because it is the same shape of thing: a filtered, scrollable list
/// with one selection.
///
/// Cycling with the arrow keys is fine for five themes and useless for 266
/// installed font families, which is what this exists for.
struct ValuePickerState {
    /// Index into the settings tab's `fields`.
    field: usize,
    /// Append the choice to a list value (the font stack's add row) instead
    /// of replacing it.
    append: bool,
    /// Everything choosable, unfiltered and in display order.
    options: Vec<String>,
    /// Parallel to the drawn rows, same-pass built — the picker discipline
    /// used by the palette and the settings overlay alike.
    visible: Vec<String>,
    selected: usize,
    filter: String,
    scroll: f32,
    scroll_to_selected: bool,
}

impl ValuePickerState {
    /// The options a filter admits, matched case-insensitively on a substring.
    ///
    /// Substring rather than prefix on purpose: the family someone wants is
    /// `MesloLGM NF`, and they will type `meslo`, but it is just as likely to
    /// be `nerd` or `mono`.
    fn matching(&self) -> Vec<String> {
        if self.filter.is_empty() {
            return self.options.clone();
        }
        let needle = self.filter.to_lowercase();
        self.options
            .iter()
            .filter(|o| o.to_lowercase().contains(&needle))
            .cloned()
            .collect()
    }
}

/// Which full-pane screen the window shows in place of the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppScreen {
    Terminal,
    Fleet,
    Themes,
    /// The Profiles tab's pane. Unlike Fleet/Themes this one is tab-shaped:
    /// `AppTabs` says the tab exists, this says it holds the pane — Esc (or
    /// activating a session) leaves it open in the strip, inactive.
    Profiles,
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
    /// Re-run a command from the fleet's history. Enter types it into the
    /// *current* session ("run here"); ⇧⏎ into the session it came from —
    /// the closest honest reading of "run on host…" until a chooser exists.
    RunBlock { origin: zest_proto::SessionAddr, command: String },
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

/// Last-output instants by session, shared with every tab's wake callback.
type ActivityMap =
    Arc<parking_lot::Mutex<std::collections::HashMap<zest_proto::SessionAddr, std::time::Instant>>>;

/// Settled profile launches, parked by workers for `Wakeup::TabsChanged` —
/// the connecting tab's placeholder address, and the session (or the error
/// its pane will carry).
type PendingLaunches = Arc<
    parking_lot::Mutex<Vec<(zest_proto::SessionAddr, Result<crate::remote::RemoteSession, String>)>>,
>;

/// The live GPU state, created once the window exists.
struct Gpu {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    renderer: Renderer,
}

pub struct App {
    config: Config,
    proxy: EventLoopProxy<Wakeup>,

    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
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
    /// A pulsing session dot is on screen — set by the chrome rebuild.
    anim_pulse: bool,
    /// The daemon link is down (`Wakeup::Detached` .. `Reattached`): the
    /// status bar says "reconnecting" in danger until it heals.
    link_down: bool,
    /// Hit regions of the per-frame block headers, consulted where the
    /// cached chrome layout says nothing. Rebuilt every redraw.
    block_hits: crate::chrome::hit::ChromeHitMap,
    /// Folded blocks, per session — a view preference, never on the wire:
    /// two clients watching one session may disagree.
    folded_blocks: std::collections::HashMap<
        zest_proto::SessionAddr,
        std::collections::BTreeSet<u32>,
    >,
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
    /// Distinct placeholder addresses for sessions with no real one.
    next_placeholder: u64,
    /// Hosts, presence, and session lists — the picker's data source.
    fleet: Option<crate::fleet::FleetModel>,
    /// The fleet picker's transient state, while open.
    picker: Option<PickerState>,
    palette_ui: Option<PaletteState>,
    settings_ui: Option<SettingsUiState>,
    /// The Profiles tab's editor state, while that tab exists (§12).
    profiles_ui: Option<ProfilesUiState>,
    /// Open over the settings overlay while a long-list field is being chosen.
    value_picker: Option<ValuePickerState>,
    /// The + launcher menu's transient state, while open — one of the
    /// mutually exclusive overlays, like the three above.
    launcher: Option<LauncherState>,
    /// The app tabs this window holds open (Profiles today) — the singleton
    /// state the launcher's Manage-profiles row and ⌘⇧, both go through.
    app_tabs: crate::tabs::AppTabs,
    /// Where each non-default setting came from, kept from the last resolve —
    /// the settings tab's "set by profile `k8s`" chips read it.
    provenance: std::collections::BTreeMap<String, zest_config::Source>,
    /// Keys the cascade kept that the schema does not know — the settings
    /// tab's ninth category. Kept from the last resolve, like provenance.
    unknown_keys: Vec<String>,
    /// Restart-class keys edited this run. On `App` rather than the overlay
    /// state: closing and reopening the overlay does not un-owe the restart.
    restart_pending: std::collections::BTreeSet<String>,
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
    /// The persisted identity used for hosts that are not this machine,
    /// loaded lazily on first need so the keychain stays off the startup
    /// path (and off it entirely for people who never leave loopback).
    remote_identity: Option<Arc<zest_mesh::identity::ClientIdentity>>,
    fonts: Option<Fonts>,
    palette: zest_core::PaletteSnapshot,

    /// The chrome's resolved palette; rebuilt with the theme.
    chrome_colors: ChromeColors,
    /// Stem darkening, resolved from the user's settings and the theme.
    ///
    /// Stored rather than recomputed per frame: resolving it looks the theme up
    /// by name, and this is read on the render path.
    text_tuning: zest_render_wgpu::TextTuning,
    /// Last laid-out chrome, shared by redraw and the input path so a click
    /// is tested against exactly what is on screen.
    chrome_layout: Option<ChromeLayout>,
    /// The chrome's own damage latch, beside the session's. Set only by
    /// discrete events (hover, focus, title, config), so 0%-idle holds.
    chrome_dirty: bool,
    /// What the pointer was last over, for hover fills.
    chrome_hover: Option<HitRegion>,
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
    mouse: MouseState,
    /// Pointer position in cells, updated on every move.
    pointer_cell: (usize, usize),
    clipboard: Option<arboard::Clipboard>,
    /// Composition state for the input method. See `zest_input::ime`.
    ime: zest_input::Ime,
    selection_bg: zest_core::Rgb,
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
    /// Dropping this stops watching the config file.
    config_watcher: Option<zest_config::Watcher>,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartScreen {
    Fleet,
    Themes,
    Settings,
    Palette,
    /// The + launcher menu, open over the default screen (design §1).
    Launcher,
    /// The Profiles tab (design §12 — the placeholder pane, until the
    /// editor's work item lands).
    Profiles,
}

impl App {
    pub fn new(
        resolved: zest_config::Resolved,
        cli_layer: toml::Table,
        profile: Option<String>,
        proxy: EventLoopProxy<Wakeup>,
    ) -> Self {
        // Taken whole rather than as bare settings: provenance is the part
        // of a resolve that is easy to drop and expensive to add back — the
        // settings tab's "set by ..." chips are built from it, and its
        // unknown-keys category from the keys the cascade kept.
        let zest_config::Resolved { settings, provenance, unknown_keys } = resolved;
        let config = Config::from(&settings);
        let theme = zest_theme::builtin::get(&config.theme)
            .unwrap_or_else(zest_theme::builtin::obsidian);
        let resolved = zest_theme::resolve(&theme);
        let palette = to_core_palette(&resolved);
        let chrome_colors = ChromeColors::new(&theme.ui, &theme.effects, config.chrome_opacity);
        let text_tuning = resolve_text_tuning(&config);
        let selection_bg = zest_core::Rgb::new(
            resolved.selection_bg.r,
            resolved.selection_bg.g,
            resolved.selection_bg.b,
        );

        Self {
            config,
            text_tuning,
            proxy,
            window: None,
            gpu: None,
            tabs: TabStrip::default(),
            screen: AppScreen::Terminal,
            anim_epoch: std::time::Instant::now(),
            anim_spin: false,
            anim_pulse: false,
            link_down: false,
            block_hits: crate::chrome::hit::ChromeHitMap::default(),
            folded_blocks: std::collections::HashMap::new(),
            activity: ActivityMap::default(),
            route: None,
            client_identity: None,
            next_placeholder: 0,
            fleet: None,
            picker: None,
            palette_ui: None,
            value_picker: None,
            settings_ui: None,
            profiles_ui: None,
            launcher: None,
            app_tabs: crate::tabs::AppTabs::default(),
            provenance,
            unknown_keys,
            restart_pending: std::collections::BTreeSet::new(),
            settings_error: None,
            slider_drag: None,
            pending_tabs: Arc::new(parking_lot::Mutex::new(Vec::new())),
            pending_launches: Arc::new(parking_lot::Mutex::new(Vec::new())),
            remote_identity: None,
            fonts: None,
            palette,
            chrome_colors,
            chrome_layout: None,
            chrome_dirty: true,
            chrome_hover: None,
            cursor: winit::window::CursorIcon::Default,
            strip_scroll: 0.0,
            strip_ensure_visible: false,
            pointer_pos: (0.0, 0.0),
            last_drag_click: None,
            window_title: String::new(),
            scene: Scene::default(),
            modifiers: ModifiersState::empty(),
            focused: true,
            mouse: MouseState::default(),
            pointer_cell: (0, 0),
            // Created once: constructing a Clipboard opens an OS connection, and
            // doing that per copy is both slow and flaky under contention.
            clipboard: arboard::Clipboard::new()
                .map_err(|e| tracing::warn!(error = %e, "clipboard unavailable"))
                .ok(),
            ime: zest_input::Ime::new(),
            selection_bg,
            scroll_accum: 0.0,
            settings,
            cli_layer,
            profile,
            config_watcher: None,
            startup_probe: false,
            no_daemon: false,
            attach_probe: false,
            new_session: false,
            attach_addr: None,
            screenshot: None,
            screenshot_at: None,
            start_screen: None,
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

    /// What the process should exit with, once the event loop has returned.
    ///
    /// Zero for every ordinary run; non-zero only when `--screenshot` could not
    /// write its file. Read by `main` rather than acted on here, so the exit
    /// runs every destructor on the way out.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        self.exit_code.unwrap_or(0)
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
        let attached = match crate::daemon::find_or_spawn(&socket, DAEMON_START_TIMEOUT) {
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
        self.next_placeholder += 1;
        let addr_cell = Arc::new(parking_lot::Mutex::new(crate::tabs::placeholder_addr(
            self.next_placeholder,
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
            let a = crate::daemon::find_or_spawn(&redial_socket, DAEMON_START_TIMEOUT)
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
                            let a = crate::daemon::find_or_spawn(&socket, DAEMON_START_TIMEOUT)
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

        self.next_placeholder += 1;
        let addr_cell = Arc::new(parking_lot::Mutex::new(crate::tabs::placeholder_addr(
            self.next_placeholder,
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
                cols,
                rows,
                scrollback: self.config.scrollback,
                adopt: !self.new_session,
                local: false,
                // The address was typed by a person, not learned from an
                // advertisement; pinning a HostId here is future work along
                // with the stored identity.
                expect_host: None,
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

    /// Start watching the config file.
    ///
    /// Deliberately not fatal if it fails: the terminal works fine without hot
    /// reload, and a filesystem that will not support a watch — a network share,
    /// a container mount — is not a reason to refuse to run.
    fn watch_config(&mut self) {
        // `config_file()` first: in portable mode the file is `zesterm.toml`
        // beside the binary, and watching `config.toml` there would watch a
        // file nobody writes. The fallback names the file that will exist
        // once the first save creates it.
        let Some(path) = zest_config::paths::config_file()
            .or_else(|| zest_config::paths::config_dir().map(|d| d.join(zest_config::paths::CONFIG_FILE)))
        else {
            return;
        };
        let proxy = self.proxy.clone();
        match zest_config::Watcher::new(&path, move || {
            let _ = proxy.send_event(Wakeup::ConfigChanged);
        }) {
            Ok(w) => {
                tracing::debug!(path = %path.display(), "watching config");
                self.config_watcher = Some(w);
            }
            Err(e) => tracing::warn!(error = %e, "config hot reload unavailable"),
        }
    }

    fn set_clipboard(&mut self, text: String) {
        let Some(clipboard) = self.clipboard.as_mut() else { return };
        if let Err(e) = clipboard.set_text(text) {
            tracing::warn!(error = %e, "copy failed");
        }
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

    /// Copy what the last command printed — not its prompt, not the command.
    ///
    /// Targets the most recent block with output rather than the one the cursor
    /// is in: at a prompt the cursor's block has printed nothing, which is
    /// almost always.
    fn copy_block_output(&mut self) {
        // Same borrow discipline as `copy_selection`: read through the session,
        // drop the lock, then touch the clipboard behind `&mut self`.
        let text = self.tabs.active_source().and_then(|s| {
            let term = s.terminal().lock();
            let block = block_actions::last_with_output(&term)?;
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

    /// Run the last command again.
    ///
    /// Sent as [`ClientMessage::Input`](zest_proto::ClientMessage) would be —
    /// the command text and a carriage return. There is no "re-run" message,
    /// because the host would have to take the client's word for the command
    /// anyway, and typing it is exactly what re-running means.
    fn rerun_last_command(&mut self) {
        let Some(session) = self.tabs.active_source() else { return };
        let bytes = {
            let term = session.terminal().lock();
            block_actions::last_with_output(&term)
                .as_ref()
                .and_then(block_actions::rerun_bytes)
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

    fn paste(&mut self) {
        let Some(session) = self.tabs.active_source() else { return };
        let Some(clipboard) = self.clipboard.as_mut() else { return };
        match clipboard.get_text() {
            Ok(text) if !text.is_empty() => {
                // The terminal owns the encoding: it knows whether the program
                // asked for bracketed paste, and it normalizes line endings.
                let bytes = session.terminal().lock().encode_paste(&text);
                session.write(bytes);
                session.terminal().lock().scroll_to_bottom();
            }
            Ok(_) => {}
            Err(e) => tracing::debug!(error = %e, "nothing to paste"),
        }
    }

    /// Run one table-resolved action.
    ///
    /// Exhaustive on purpose — no `_` arm: a new [`keymap::Action`] that can
    /// be reached from the table without being handled here is a compile
    /// error, not a dead shortcut.
    fn perform(&mut self, action: keymap::Action, el: &ActiveEventLoop) {
        use keymap::Action;
        match action {
            Action::NewTab => self.new_tab(),
            Action::CloseTab => {
                // App tabs first, whichever holds the pane: closing one is
                // closing a tab (§11's rule), and their chips deliberately
                // draw no × — ⌘W is the close affordance.
                if self.settings_tab_active() {
                    self.close_settings_tab();
                    return;
                }
                if self.screen == AppScreen::Profiles {
                    self.close_profiles_tab();
                    self.show_screen(AppScreen::Terminal);
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
                    self.close_tab(addr, false, el);
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
        }
    }

    /// ⌘D: give the active tab a second pane on the same host; on a tab that
    /// already has one, move the keyboard to the other pane instead — the
    /// chord stays useful after the split.
    fn split_right(&mut self) {
        if self.tabs.active().is_none() {
            return;
        }
        if self.tabs.active().is_some_and(|t| t.split.is_some()) {
            if let Some(tab) = self.tabs.active_mut() {
                tab.focus_right = !tab.focus_right;
            }
            self.mark_chrome_dirty();
            return;
        }

        // Sized for the pane it will occupy, not the whole grid: the shell's
        // first prompt should wrap where the pane edge is.
        let (cols, rows) = self.split_pane_dims();

        // Seeded with the tab's palette, not the window's: the pane shares
        // its tab's identity until panes carry their own profile.
        let seed = self.palette_for(self.tabs.active().and_then(|t| t.identity.as_ref()));

        let pane = match (&self.route, &self.client_identity) {
            (Some(route), Some(identity)) => {
                self.next_placeholder += 1;
                let cell = Arc::new(parking_lot::Mutex::new(crate::tabs::placeholder_addr(
                    self.next_placeholder,
                )));
                let wake = wake_for(&self.proxy, Arc::clone(&cell), Arc::clone(&self.activity));
                let command = match route {
                    HostRoute::LocalSocket(_) => self.config.shell.clone().unwrap_or_default(),
                    HostRoute::Tcp(_) => String::new(),
                };
                let session = crate::remote::RemoteSession::create_and_attach(
                    route.dialer(),
                    &crate::remote::AttachOptions {
                        identity,
                        label: "zesterm",
                        command: &command,
                        cwd: "",
                        cols,
                        rows,
                        scrollback: self.config.scrollback,
                        adopt: false,
                        local: route.is_local(),
                        expect_host: None,
                    },
                    wake,
                );
                match session {
                    Ok(session) => {
                        *cell.lock() = session.addr();
                        session.terminal().lock().set_palette(seed);
                        let local = route.is_local();
                        crate::tabs::SplitPane::daemon(session, local, (cols, rows))
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "could not open a split pane");
                        return;
                    }
                }
            }
            _ => {
                self.next_placeholder += 1;
                let addr = crate::tabs::placeholder_addr(self.next_placeholder);
                let cell = Arc::new(parking_lot::Mutex::new(addr));
                match Session::spawn(
                    &self.build_spec(),
                    PtySize::new(cols, rows),
                    self.config.scrollback,
                    wake_for(&self.proxy, cell, Arc::clone(&self.activity)),
                ) {
                    Ok(session) => {
                        session.terminal().lock().set_palette(seed);
                        crate::tabs::SplitPane::in_process(session, addr, (cols, rows))
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "could not spawn a split pane");
                        return;
                    }
                }
            }
        };

        if let Some(tab) = self.tabs.active_mut() {
            tab.split = Some(Box::new(pane));
            tab.focus_right = true;
        }
        self.resize_split_panes();
        self.mark_chrome_dirty();
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Cols/rows that fit one pane of a split, from the current window.
    fn split_pane_dims(&self) -> (u16, u16) {
        let geometry = self.window.as_ref().zip(self.fonts.as_ref());
        let Some((window, fonts)) = geometry else { return (80, 24) };
        let scale = window.scale_factor() as f32;
        let size = window.inner_size();
        let area = self.insets_at(scale).grid_rect(size.width, size.height);
        let (_, right) = crate::chrome::layout::pane_frames(area, scale);
        let body = crate::chrome::layout::pane_body(right, scale);
        let cm = fonts.cell_metrics();
        let cols = ((body[2] / cm.cell_w as f32) as u16).max(2);
        let rows = ((body[3] / cm.cell_h as f32) as u16).max(2);
        (cols, rows)
    }

    /// The rectangle the focused terminal is drawn in: the grid area, or the
    /// focused pane's body when the active tab is split. Everything that
    /// maps pixels to cells reads this — one rectangle, one truth.
    fn focused_view_rect(&self) -> Option<[f32; 4]> {
        let window = self.window.as_ref()?;
        let scale = window.scale_factor() as f32;
        let size = window.inner_size();
        let area = self.insets_at(scale).grid_rect(size.width, size.height);
        let tab = self.tabs.active()?;
        if tab.split.is_some() {
            let (l, r) = crate::chrome::layout::pane_frames(area, scale);
            let frame = if tab.focus_right { r } else { l };
            Some(crate::chrome::layout::pane_body(frame, scale))
        } else {
            Some(area)
        }
    }

    /// Resize both panes of a split tab to their body rectangles.
    fn resize_split_panes(&mut self) {
        let geometry = self.window.as_ref().zip(self.fonts.as_ref());
        let Some((window, fonts)) = geometry else { return };
        let scale = window.scale_factor() as f32;
        let size = window.inner_size();
        let area = self.insets_at(scale).grid_rect(size.width, size.height);
        let (left, right) = crate::chrome::layout::pane_frames(area, scale);
        let cm = fonts.cell_metrics();
        let dims = |body: [f32; 4]| {
            (
                ((body[2] / cm.cell_w as f32) as u16).max(2),
                ((body[3] / cm.cell_h as f32) as u16).max(2),
            )
        };
        let (ld, rd) = (
            dims(crate::chrome::layout::pane_body(left, scale)),
            dims(crate::chrome::layout::pane_body(right, scale)),
        );
        if let Some(tab) = self.tabs.active_mut() {
            if tab.split.is_some() {
                tab.source().resize(ld.0, ld.1);
                tab.sized = ld;
                if let Some(split) = tab.split.as_mut() {
                    split.source().resize(rd.0, rd.1);
                    split.sized = rd;
                }
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
        use crate::chrome::model::{FleetCard, ScreenModel, ThemeCard};
        use crate::fleet::SessionsState;
        match self.screen {
            AppScreen::Terminal => None,
            AppScreen::Fleet => {
                let cards = fleet_hosts
                    .iter()
                    .map(|h| {
                        let online =
                            h.local || h.presence == zest_mesh::discovery::Presence::Online;
                        let mut rows: Vec<(String, String, u8)> = Vec::new();
                        // Only what is actually known: an os row we cannot
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
                        rows.push(("key".into(), h.host.short(), 0));
                        if let SessionsState::Fresh(sessions) = &h.sessions {
                            let n = sessions.len();
                            let label =
                                if n == 1 { "1 session".into() } else { format!("{n} sessions") };
                            rows.push(("sessions".into(), label, 0));
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
                            rows,
                        }
                    })
                    .collect();
                Some(ScreenModel::Fleet { cards })
            }
            AppScreen::Themes => {
                let active = if zest_theme::builtin::get(&self.config.theme).is_some() {
                    self.config.theme.clone()
                } else {
                    zest_theme::builtin::DEFAULT_DARK.to_string()
                };
                let cards = zest_theme::builtin::all()
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
                        let qualifier =
                            if default { format!("{mode} · default") } else { mode.to_string() };
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
                Some(ScreenModel::Themes { cards })
            }
            // Built in refresh_chrome beside the Settings model — it needs
            // &mut access to the editor state, which &self here cannot give.
            AppScreen::Profiles => None,
        }
    }

    /// The split tab's pane headers, when the active tab has a split.
    fn build_panes_model(
        &self,
        fleet_hosts: &[crate::fleet::FleetHost],
    ) -> Option<[crate::chrome::model::PaneModel; 2]> {
        use crate::chrome::model::PaneModel;
        let tab = self.tabs.active()?;
        let split = tab.split.as_ref()?;
        let describe = |source: &dyn crate::source::SessionSource| {
            let (host, accent, remote) = match source.origin() {
                Origin::Daemon { host, local: false } => (host, 1, true),
                _ => (
                    fleet_hosts
                        .iter()
                        .find(|h| h.local)
                        .map_or_else(|| "local".to_string(), |h| h.label.clone()),
                    0,
                    false,
                ),
            };
            let cwd = {
                let term = source.terminal();
                let term = term.lock();
                if term.cwd().is_empty() {
                    term.blocks().last().map(|b| b.cwd.clone()).unwrap_or_default()
                } else {
                    term.cwd().to_string()
                }
            };
            let cwd = if remote { cwd } else { crate::status::shorten_home(&cwd) };
            let path = fleet_hosts
                .iter()
                .find(|h| h.label == host)
                .and_then(|h| match h.reachability {
                    Some(zest_mesh::Reachability::Cloud) => Some(match h.rtt_ms {
                        Some(ms) => {
                            format!("tunnel {}", crate::chrome::layout::format_ms(ms))
                        }
                        None => "tunnel".to_string(),
                    }),
                    _ => None,
                });
            let sub = match (cwd.is_empty(), path) {
                (false, Some(p)) => format!("{cwd} · {p}"),
                (false, None) => cwd,
                (true, Some(p)) => p,
                (true, None) => String::new(),
            };
            (host, sub, accent)
        };
        let (lh, ls, la) = describe(tab.source());
        let (rh, rs, ra) = describe(split.source());
        Some([
            PaneModel { host: lh, sub: ls, focused: !tab.focus_right, accent: la },
            PaneModel { host: rh, sub: rs, focused: tab.focus_right, accent: ra },
        ])
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
    fn anim_deadline(&self) -> Option<std::time::Duration> {
        let mut next: Option<u64> = None;
        let mut consider = |ms: u64| next = Some(next.map_or(ms, |n: u64| n.min(ms)));
        if self.anim_spin {
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
        next.map(std::time::Duration::from_millis)
    }

    /// Back to the terminal if a full-pane screen is up; free otherwise.
    fn leave_screen(&mut self) {
        if self.screen != AppScreen::Terminal {
            self.screen = AppScreen::Terminal;
            self.mark_chrome_dirty();
        }
    }

    /// Open or close a full-pane screen; closing always lands on the grid.
    /// The settings *tab* survives — a screen draws over it and Esc returns,
    /// exactly as over a session's grid.
    fn show_screen(&mut self, screen: AppScreen) {
        self.screen = screen;
        self.picker = None;
        self.palette_ui = None;
        self.launcher = None;
        self.mark_chrome_dirty();
    }

    /// Open (or activate) the Profiles tab — the ⌘⇧, / Manage-profiles /
    /// `--screen profiles` singleton: at most one exists, and reopening it
    /// shows the one that does.
    fn open_profiles_tab(&mut self) {
        self.app_tabs.open_profiles();
        if self.profiles_ui.is_none() {
            self.profiles_ui = Some(ProfilesUiState {
                profile: zest_config::profiles::RESERVED_PROFILE.to_string(),
                selected: 0,
                filter: String::new(),
                scroll: 0.0,
                scroll_to_selected: true,
                actions: Vec::new(),
                fields: zest_config::profiles::fields(),
                editing: None,
                menu: None,
                error: None,
            });
        }
        self.show_screen(AppScreen::Profiles);
    }

    /// Close the Profiles tab — its state lives as long as the tab, exactly
    /// like the Settings tab's (§11's rule, applied to §12).
    fn close_profiles_tab(&mut self) {
        self.app_tabs.close_profiles();
        self.profiles_ui = None;
        if self.screen == AppScreen::Profiles {
            self.screen = AppScreen::Terminal;
        }
        self.mark_chrome_dirty();
    }

    /// The Profiles editor holds the keyboard and the grid area.
    fn profiles_tab_active(&self) -> bool {
        self.screen == AppScreen::Profiles && self.profiles_ui.is_some()
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
                self.value_picker = None;
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
            LauncherAction::Launch(name) => {
                self.launcher = None;
                self.mark_chrome_dirty();
                self.launch_profile(&name);
            }
            LauncherAction::LaunchDefault => {
                self.launcher = None;
                self.mark_chrome_dirty();
                self.new_tab();
            }
            LauncherAction::RunOnHost => {
                // The fleet picker is the "choose the machine" surface the
                // design points ⇧⏎ at; toggle_launcher's exclusivity closed
                // us already, but the order matters: toggle_picker closes
                // every sibling, so the launcher must go first.
                self.launcher = None;
                self.toggle_picker();
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
                    if let Some(p) = self.value_picker.as_mut() {
                        // The value picker takes the keys from the tab
                        // (see the key path's ordering); a composed family
                        // name belongs to its filter.
                        p.filter.push_str(&text);
                        p.selected = 0;
                        p.scroll_to_selected = true;
                    } else if let Some(ui) = self.settings_ui.as_mut() {
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
                    session.write(text.into_bytes());
                    let mut term = session.terminal().lock();
                    term.scroll_to_bottom();
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
        let mut insets = Insets::padding_only(self.config.padding, scale);
        if self.strip_shown() {
            match self.config.tabs.position {
                zest_config::settings::TabsPosition::Top => {
                    insets.top += self.config.tabs.strip_height as f32 * scale;
                }
                zest_config::settings::TabsPosition::Left => {
                    insets.left += self.config.tabs.sidebar_width as f32 * scale;
                    // The full-width header over the sidebar + pane row: the
                    // vertical layout's counterpart of the strip, and it was
                    // once forgotten here — the grid painted its first two
                    // rows straight over the session name.
                    insets.top += crate::chrome::layout::HEADER_H * scale;
                }
            }
            // Nothing reserves the bottom edge: the status bar is gone
            // (design §1), so below the grid there is only `window.padding`.
        }
        insets
    }

    /// Whether the strip is drawn at all.
    ///
    /// `custom_chrome` forces it, and that is load-bearing rather than a
    /// preference: with `show_single_tab` off and one tab open, a borderless
    /// window would have no titlebar, no caption buttons and nothing to drag —
    /// an undecorated rectangle with no way to move, maximize or close it.
    fn strip_shown(&self) -> bool {
        self.config.custom_chrome || self.config.tabs.show_single_tab || self.tabs.len() > 1
    }

    fn mark_chrome_dirty(&mut self) {
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
            && self.launcher.is_none()
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
        let hosts_searched = self.fleet.as_ref().map_or(0, |f| f.snapshot().len());
        let anim = self.anim_phase();
        let caret_on = anim.caret_on;
        let early_geometry = self.window.as_ref().map(|w| {
            let scale = w.scale_factor() as f32;
            (scale, w.inner_size())
        });
        let picker_rows = self.picker.is_some().then(|| self.build_picker());
        let picker_model = picker_rows.map(|(rows, actions)| {
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
                filter: state.filter.clone(),
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

        // The value picker draws through the palette's overlay -- same shape of
        // thing, and `PaletteRow` is display-only, so the chrome needs to know
        // nothing about it.
        let value_picker_model = self.value_picker.as_mut().map(|state| {
            state.visible = state.matching();
            state.selected = state.selected.min(state.visible.len().saturating_sub(1));
            crate::chrome::model::PaletteModel {
                rows: state
                    .visible
                    .iter()
                    .map(|name| crate::chrome::model::PaletteRow::Command {
                        name: name.clone(),
                        chord: String::new(),
                        runnable: true,
                    })
                    .collect(),
                selected: state.selected,
                filter: state.filter.clone(),
                scroll: state.scroll,
                ensure_visible: state.scroll_to_selected,
            }
        });

        let palette_model = self.palette_ui.as_mut().map(|state| {
            let (rows, actions) = keymap::palette(&state.filter);
            state.actions = actions;
            // A filter edit can strand the selection on a header, a
            // reference row, or past the end; land it on the nearest
            // runnable command instead.
            state.selected = keymap::nearest_runnable(&state.actions, state.selected);
            crate::chrome::model::PaletteModel {
                rows,
                selected: state.selected,
                filter: state.filter.clone(),
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
                self.restart_pending.clone(),
                self.settings_error.clone(),
                self.unknown_keys.clone(),
                // The rail's visible categories — computed outside the &mut
                // borrow, by the same helper the click handler resolves
                // `SettingsCategory(i)` with, so a click can never land on
                // a different list than was drawn.
                self.visible_categories(),
            )
        });
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
                        &ui.filter,
                        &zest_config::schema::keys(),
                    );
                    let empty = rows.is_empty().then(|| {
                        if unknown_keys.is_empty() {
                            "Every key in your files is a setting this build knows.".to_string()
                        } else {
                            format!("nothing matches \u{201c}{}\u{201d}", ui.filter)
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
                        &ui.filter,
                        ui.editing.as_ref(),
                        &restart_pending,
                        error.as_deref(),
                        &ui.installed,
                    );
                    let empty = rows
                        .is_empty()
                        .then(|| format!("nothing matches \u{201c}{}\u{201d}", ui.filter));
                    (rows, actions, empty)
                };
                ui.actions = actions;
                // A filter edit can strand the selection on a banner or past
                // the end; land it on the nearest real row instead.
                ui.selected = sui::nearest_field(&ui.actions, ui.selected);

                // The open dropdown, resolved against the selected row's
                // variants — same-pass, so the menu can never outlive the
                // row it hangs off.
                let menu = ui.menu.and_then(|(row, selected)| {
                    let field_idx = match ui.actions.get(row) {
                        Some(sui::RowAction::Field(i)) => *i,
                        _ => return None,
                    };
                    let field = ui.fields.get(field_idx)?;
                    if field.variants.is_empty() {
                        return None;
                    }
                    let current = zest_config::ui::value_at(&values, &field.key)
                        .and_then(serde_json::Value::as_str)
                        .and_then(|v| field.variants.iter().position(|o| o.value == v));
                    Some(crate::chrome::model::SettingsMenuModel {
                        row,
                        options: field
                            .variants
                            .iter()
                            .map(|v| crate::chrome::model::SettingsMenuOption {
                                label: sui::humanize_value(&v.value),
                                value: v.value.clone(),
                                doc: v.description.lines().next().unwrap_or_default().to_string(),
                            })
                            .collect(),
                        current,
                        selected: selected.min(field.variants.len().saturating_sub(1)),
                    })
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
                    filter: ui.filter.clone(),
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
            let hosts: Vec<(String, bool, bool)> = self
                .fleet
                .as_ref()
                .map(|f| {
                    f.snapshot()
                        .into_iter()
                        .map(|h| {
                            let online = h.local
                                || h.presence == zest_mesh::discovery::Presence::Online;
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
                self.config.theme.clone(),
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
                    &ui.filter,
                    ui.editing.as_ref(),
                    ui.error.as_deref(),
                );
                ui.actions = actions;
                // A filter edit can strand the selection on a section rule
                // or past the end; land it on the nearest real row.
                ui.selected = sui::nearest_field(&ui.actions, ui.selected);

                // The open dropdown, resolved same-pass against the row's
                // variants (window.backdrop's menu; the pickers have none).
                let overrides = pui::overrides_json(&resolved);
                let menu = ui.menu.and_then(|(row, selected)| {
                    let field_idx = match ui.actions.get(row) {
                        Some(sui::RowAction::Field(i)) => *i,
                        _ => return None,
                    };
                    let field = ui.fields.get(field_idx)?;
                    if field.variants.is_empty() {
                        return None;
                    }
                    let current = pui::effective_value(field, &resolved, &overrides, &ctx);
                    let current = current
                        .as_str()
                        .and_then(|v| field.variants.iter().position(|o| o.value == v));
                    Some(crate::chrome::model::SettingsMenuModel {
                        row,
                        options: field
                            .variants
                            .iter()
                            .map(|v| crate::chrome::model::SettingsMenuOption {
                                label: sui::humanize_value(&v.value),
                                value: v.value.clone(),
                                doc: v.description.lines().next().unwrap_or_default().to_string(),
                            })
                            .collect(),
                        current,
                        selected: selected.min(field.variants.len().saturating_sub(1)),
                    })
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
                    .then(|| format!("nothing matches \u{201c}{}\u{201d}", ui.filter));

                Box::new(crate::chrome::model::ProfilesScreenModel {
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
                    filter: ui.filter.clone(),
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
        let fleet_hosts = self.fleet.as_ref().map(|f| f.snapshot()).unwrap_or_default();
        let screen_model = profiles_model
            .map(crate::chrome::model::ScreenModel::Profiles)
            .or_else(|| self.build_screen_model(&fleet_hosts));
        let panes = self.build_panes_model(&fleet_hosts);
        let grid_area = early_geometry.map_or([0.0; 4], |(scale, size)| {
            self.insets_at(scale).grid_rect(size.width, size.height)
        });

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
            drawn_caption: self.config.custom_chrome && !fullscreen,
            maximized: window.is_maximized(),
            // A maximized window that resized from its edge would un-maximize
            // under the pointer, which is not what the drag meant.
            resizable_edges: self.config.custom_chrome
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
        let mut remote_slots: Vec<String> = Vec::new();

        let tab_models: Vec<TabModel> = self
            .tabs
            .iter()
            .map(|tab| {
                // A brief lock per tab per chrome rebuild — rebuilds are
                // event-driven, so this is microseconds, not a frame cost.
                let (title, cwd, running) = {
                    let term = tab.source().terminal();
                    let term = term.lock();
                    let title = term.title().trim().to_string();
                    // A remote terminal's cwd never crosses the wire directly;
                    // its blocks do, and each carries the cwd it ran in.
                    let cwd = if term.cwd().is_empty() {
                        term.blocks().last().map(|b| b.cwd.clone()).unwrap_or_default()
                    } else {
                        term.cwd().to_string()
                    };
                    let running = term.blocks().last().is_some_and(|b| b.is_running());
                    (title, cwd, running)
                };
                let title = if title.is_empty() { "shell".to_string() } else { title };
                let origin = match tab.source().origin() {
                    Origin::Daemon { host, local: false } => {
                        TabOrigin::Remote { host_label: host }
                    }
                    _ => TabOrigin::Local,
                };
                let (host_label, accent, cwd) = match &origin {
                    TabOrigin::Remote { host_label } => {
                        let slot = remote_slots
                            .iter()
                            .position(|l| l == host_label)
                            .unwrap_or_else(|| {
                                remote_slots.push(host_label.clone());
                                remote_slots.len() - 1
                            });
                        (host_label.clone(), slot + 1, cwd)
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
                } else {
                    match &origin {
                        TabOrigin::Remote { host_label } => {
                            let fleet = fleet_hosts.iter().find(|h| &h.label == host_label);
                            match fleet.and_then(|h| h.reachability) {
                                Some(zest_mesh::Reachability::Cloud) => {
                                    crate::chrome::model::LinkKind::Tunnel
                                }
                                Some(zest_mesh::Reachability::Loopback) => {
                                    crate::chrome::model::LinkKind::Loopback
                                }
                                _ => crate::chrome::model::LinkKind::Lan,
                            }
                        }
                        TabOrigin::Local => crate::chrome::model::LinkKind::Loopback,
                    }
                };
                TabModel {
                    addr: tab.addr,
                    kind: crate::chrome::model::TabKind::Session,
                    title: if tab.dead { format!("{title} · ended") } else { title },
                    host: host_label,
                    cwd,
                    origin,
                    // Presence joins in with the fleet model; until then a
                    // reachable tab is simply online.
                    presence: TabPresence::Online,
                    accent,
                    tab_accent: crate::chrome::model::tab_accent(tab.identity.as_ref(), accent),
                    running,
                    age,
                    // Dead tabs borrow the connecting style (faint text): not
                    // live, not interactive, still present. A launching tab
                    // wears it for real (issue #175).
                    connecting: tab.dead || tab.connecting,
                    link,
                }
            })
            .collect();

        // App tabs after the session tabs, in §1's order: sessions, then
        // Profiles, then Settings, then the `+`. One list, so the strip,
        // the sidebar's pinned row and the hit map all agree what exists.
        // Profiles is horizontal-only for now: the vertical design pins app
        // tabs above the sidebar footer, which is §11's pinned-rows shape —
        // a chip grouped under a fake host would be worse than none.
        let mut tab_models = tab_models;
        let profiles_chip = self.app_tabs.profiles_open()
            && self.config.tabs.position == zest_config::settings::TabsPosition::Top;
        if profiles_chip {
            tab_models.push(TabModel {
                addr: crate::tabs::profiles_tab_addr(),
                kind: crate::chrome::model::TabKind::Profiles,
                title: "Profiles".into(),
                host: local_label.clone(),
                cwd: String::new(),
                origin: TabOrigin::Local,
                presence: TabPresence::Online,
                accent: 0,
                // Accent index 0 is the theme's own accent: an app tab is a
                // place, not a shell on a host.
                tab_accent: crate::chrome::model::AccentChoice::Profile(0),
                running: false,
                age: String::new(),
                connecting: false,
                link: crate::chrome::model::LinkKind::Loopback,
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
                age: String::new(),
                connecting: false,
                link: crate::chrome::model::LinkKind::Loopback,
            });
        }

        // The sidebar's host grouping, built from the same tab models the
        // strip draws — one pass, one truth. App tabs are places with no
        // host; the vertical layout pins them above the footer instead.
        let sidebar = (self.config.tabs.position == zest_config::settings::TabsPosition::Left)
            .then(|| {
                use zest_mesh::discovery::Presence;
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
                        online: fleet.is_none_or(|h| h.local || h.presence == Presence::Online),
                        tabs: vec![i],
                    });
                }
                let online = fleet_hosts
                    .iter()
                    .filter(|h| h.local || h.presence == Presence::Online)
                    .count()
                    .max(1);
                let asleep = fleet_hosts.len().saturating_sub(online);
                crate::chrome::model::SidebarModel {
                    groups,
                    hosts_online: online,
                    hosts_asleep: asleep,
                }
            });

        self.anim_pulse = tab_models.iter().any(|t| t.running)
            && self.config.tabs.position == zest_config::settings::TabsPosition::Left;

        // Which chip is lit — exactly one (invariant 9). The Profiles pane
        // wins while its screen is up (its chip sits right after the
        // sessions), the Settings tab while it holds the keyboard (its chip
        // is last), else the active session. Computed here rather than via
        // display_active(): that helper predates the Profiles chip and
        // assumes Settings is the only insertion.
        let active = if profiles_chip && self.screen == AppScreen::Profiles {
            self.tabs.len()
        } else if self.tabs.settings_active() {
            tab_models.len() - 1
        } else {
            self.tabs.active_index()
        };

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
            picker: picker_model,
            // The picker wins: it opens *over* the settings tab's content.
            palette: value_picker_model.or(palette_model),
            settings: settings_model,
            launcher: launcher_model,
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
        // Written back only when the pane actually laid out — a covered tab
        // reports 0.0, and writing that would reset a scroll the user set.
        if self.tabs.settings_active() && self.screen == AppScreen::Terminal {
            if let Some(state) = self.settings_ui.as_mut() {
                state.scroll = laid.settings_scroll;
                // One layout consumed the request; the wheel is free again.
                state.scroll_to_selected = false;
            }
        }
        if self.screen == AppScreen::Profiles {
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
            .or_else(|| self.block_hits.hit(x as f32, y as f32))
    }

    /// A pointer action that landed in the chrome.
    fn on_chrome_click(
        &mut self,
        region: HitRegion,
        button: MouseButton,
        state: ElementState,
        el: &ActiveEventLoop,
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
        match (region, button) {
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
                self.close_tab(addr, false, el);
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
            (HitRegion::PalettePill, MouseButton::Left)
            | (HitRegion::SidebarSearch, MouseButton::Left) => {
                self.perform(keymap::Action::ToggleFleetPicker, el);
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
            (HitRegion::Pane(right), MouseButton::Left) => {
                if let Some(tab) = self.tabs.active_mut() {
                    if tab.split.is_some() && tab.focus_right != right {
                        tab.focus_right = right;
                        self.mark_chrome_dirty();
                    }
                }
            }
            (HitRegion::ThemeCard(i), MouseButton::Left) => {
                let id = zest_theme::builtin::IDS.get(i).copied();
                if let Some(id) = id {
                    self.apply_theme_choice(id);
                }
            }
            (HitRegion::BlockFold(id), MouseButton::Left) => {
                if let Some(tab) = self.tabs.active() {
                    let set = self.folded_blocks.entry(tab.focused_addr()).or_default();
                    if !set.remove(&id) {
                        set.insert(id);
                    }
                    // Fold state lives outside the cached layout; the header
                    // pass rebuilds per frame, so a redraw is all it takes.
                    self.chrome_dirty = true;
                    if let Some(w) = self.window.as_ref() {
                        w.request_redraw();
                    }
                }
            }
            // The chips mirror the chords: both act on the most recent block
            // *with output*, which is what the design specifies — at a prompt
            // the cursor's block has printed nothing.
            (HitRegion::BlockCopy(_), MouseButton::Left) => self.copy_block_output(),
            (HitRegion::BlockRerun(_), MouseButton::Left) => self.rerun_last_command(),
            // The band itself swallows: the text it paints over must not be
            // selectable through it.
            (HitRegion::BlockHeader(_), _) => {}
            (HitRegion::PickerRow(i), MouseButton::Left) => {
                if let Some(p) = self.picker.as_mut() {
                    p.selected = i;
                }
                let action = self.picker.as_ref().and_then(|p| p.actions.get(i).cloned());
                if let Some(action) = action {
                    self.run_picker_action(action, el, false);
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
                self.run_palette_selection(el);
            }
            (HitRegion::PaletteScrim, MouseButton::Left) => {
                self.palette_ui = None;
                self.mark_chrome_dirty();
            }
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
                    let row = self.profiles_ui.as_ref().and_then(|ui| ui.menu).map(|(r, _)| r);
                    if let Some(ui) = self.profiles_ui.as_mut() {
                        ui.menu = None;
                    }
                    if let Some(row) = row {
                        self.profiles_apply_variant(row, opt);
                    }
                    return;
                }
                let row = self.settings_ui.as_ref().and_then(|ui| ui.menu).map(|(r, _)| r);
                if let Some(ui) = self.settings_ui.as_mut() {
                    ui.menu = None;
                }
                if let Some(row) = row {
                    self.apply_variant(row, opt);
                }
            }
            (HitRegion::SettingsListRemove(row, item), MouseButton::Left) => {
                self.remove_list_item(row, item);
            }
            (HitRegion::SettingsListAdd(row), MouseButton::Left) => {
                self.begin_list_add(row);
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
                        CaptionButton::Close => self.request_close(el),
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

    /// End the session set and exit, exactly as `CloseRequested` does.
    fn request_close(&mut self, el: &ActiveEventLoop) {
        // Remember the set first: dropping is the detach, and what was open is
        // what the next launch reopens.
        self.persist_tabs();
        self.tabs.clear();
        el.exit();
    }

    /// Remember what this window is showing, so the next launch can pick it
    /// back up instead of guessing (#23's adopt bug, retired).
    fn persist_tabs(&self) {
        if !self.config.tabs.restore {
            return;
        }
        let mut tabs = Vec::new();
        let mut active = 0;
        for (i, tab) in self.tabs.iter().enumerate() {
            // Placeholders (in-process ptys) die with the window and cannot
            // be reattached; dead sessions have nothing to reattach to.
            if crate::tabs::is_placeholder(tab.addr) || tab.dead {
                continue;
            }
            if i == self.tabs.active_index() {
                active = tabs.len();
            }
            let title = tab.source().terminal().lock().title().trim().to_string();
            tabs.push(crate::tabs_state::SavedTab {
                addr: tab.addr,
                local: tab.local,
                dial_hint: tab.dial_hint.clone(),
                title,
            });
        }
        crate::tabs_state::save(&crate::tabs_state::SavedTabs::new(active, tabs));
    }

    /// Toggle the fleet picker (⌘K, and the picker rows' Escape hatch).
    fn toggle_picker(&mut self) {
        self.picker = match self.picker {
            Some(_) => None,
            None => {
                // One modal at a time: opening any overlay closes the
                // others, which is what keeps the modal input blocks
                // order-independent. The settings *tab* is not an overlay
                // and stays put underneath.
                self.palette_ui = None;
                self.launcher = None;
                Some(PickerState {
                    selected: 0,
                    filter: String::new(),
                    scroll: 0.0,
                    scroll_to_selected: false,
                    actions: Vec::new(),
                    pending_profile: None,
                })
            }
        };
        self.mark_chrome_dirty();
    }

    /// Toggle the command palette (⌘/, ⌘⇧P).
    fn toggle_palette(&mut self) {
        self.palette_ui = match self.palette_ui {
            Some(_) => None,
            None => {
                self.picker = None;
                self.launcher = None;
                Some(PaletteState {
                    selected: 0,
                    filter: String::new(),
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
    fn run_palette_selection(&mut self, el: &ActiveEventLoop) {
        let action =
            self.palette_ui.as_ref().and_then(|p| p.actions.get(p.selected).copied()).flatten();
        let Some(action) = action else { return };
        self.palette_ui = None;
        self.mark_chrome_dirty();
        self.perform(action, el);
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
        self.value_picker = None;
        self.launcher = None;
        if self.settings_ui.is_none() {
            // The scan is a real cost, paid once at open — the font rows'
            // fallback tags read the cached roster from then on.
            let installed =
                self.fonts.as_mut().map(Fonts::installed_families).unwrap_or_default();
            self.settings_ui = Some(SettingsUiState {
                selected: 0,
                category: crate::settings_ui::GROUP_ORDER[0].to_string(),
                filter: String::new(),
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
        self.settings_ui = None;
        self.value_picker = None;
        self.tabs.close_settings();
        self.after_activation();
    }

    /// The Settings tab holds the keyboard and the grid area.
    fn settings_tab_active(&self) -> bool {
        self.tabs.settings_active() && self.settings_ui.is_some()
    }

    /// Open the long-list picker on the selected settings row.
    ///
    /// Returns false when the row has nothing to pick from, so the caller can
    /// fall back to whatever it would otherwise have done.
    fn open_value_picker(&mut self) -> bool {
        let Some(idx) = self.selected_settings_field() else { return false };
        let Some(field) = self.settings_ui.as_ref().and_then(|ui| ui.fields.get(idx)) else {
            return false;
        };
        let options = match field.widget {
            zest_config::ui::Widget::FontList => {
                // A real scan of installed families, so it happens here -- once,
                // on the keypress that opens the list -- and never per frame.
                self.fonts.as_mut().map(Fonts::installed_families).unwrap_or_default()
            }
            zest_config::ui::Widget::ThemePicker => {
                zest_theme::builtin::all().into_iter().map(|t| t.id).collect()
            }
            _ => return false,
        };
        if options.is_empty() {
            return false;
        }
        // Start on the value already set, so opening the list and pressing
        // Enter is a no-op rather than a surprise.
        let current = self.settings_value_of(idx).and_then(|v| match &v {
            serde_json::Value::Array(a) => {
                a.first().and_then(|f| f.as_str().map(str::to_string))
            }
            serde_json::Value::String(s) => Some(s.clone()),
            _ => None,
        });
        let selected = current
            .and_then(|c| options.iter().position(|o| *o == c))
            .unwrap_or(0);

        self.value_picker = Some(ValuePickerState {
            field: idx,
            append: false,
            visible: options.clone(),
            options,
            selected,
            filter: String::new(),
            scroll: 0.0,
            scroll_to_selected: true,
        });
        self.mark_chrome_dirty();
        true
    }

    /// Take the picker's selection and write it to the field.
    fn accept_value_picker(&mut self) {
        let Some(state) = self.value_picker.take() else { return };
        self.mark_chrome_dirty();
        let Some(chosen) = state.visible.get(state.selected).cloned() else { return };
        let Some(field) = self.settings_ui.as_ref().and_then(|ui| ui.fields.get(state.field))
        else {
            return;
        };
        let value = if field.widget == zest_config::ui::Widget::FontList {
            if state.append {
                // The add row grows the stack (§11: the dashed row opens
                // this picker); choosing a face already present is a no-op,
                // not a duplicate — the Curlz MT lesson, again.
                let mut arr = self
                    .settings_value_of(state.field)
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
        self.apply_edit(state.field, value);
    }

    /// The selected settings row's field index, when it is a real field.
    fn selected_settings_field(&self) -> Option<usize> {
        let ui = self.settings_ui.as_ref()?;
        match ui.actions.get(ui.selected) {
            Some(crate::settings_ui::RowAction::Field(i)) => Some(*i),
            _ => None,
        }
    }

    /// The live value of a field, read from the resolved settings — the same
    /// serialization the rows display, so an edit steps from what is shown.
    fn settings_value_of(&self, field_idx: usize) -> Option<serde_json::Value> {
        let ui = self.settings_ui.as_ref()?;
        let field = ui.fields.get(field_idx)?;
        let values = serde_json::to_value(&self.settings).ok()?;
        zest_config::ui::value_at(&values, &field.key).cloned()
    }

    /// Arrow-key editing on the selected row: flip, cycle or step, then write.
    fn adjust_selected_setting(&mut self, dir: i32) {
        let Some(idx) = self.selected_settings_field() else { return };
        let Some(current) = self.settings_value_of(idx) else { return };
        // Enumerating installed families is a real scan, so it happens only on
        // the keypress that needs it and only for the field that needs it.
        let installed: Vec<String> = self
            .settings_ui
            .as_ref()
            .and_then(|ui| ui.fields.get(idx))
            .filter(|f| f.widget == zest_config::ui::Widget::FontList)
            .and(self.fonts.as_mut())
            .map(Fonts::installed_families)
            .unwrap_or_default();
        let next = self.settings_ui.as_ref().and_then(|ui| {
            let field = ui.fields.get(idx)?;
            let themes: Vec<String> =
                zest_theme::builtin::all().into_iter().map(|t| t.id).collect();
            crate::settings_ui::adjust(field, &current, dir, &themes, &installed)
        });
        if let Some(value) = next {
            self.apply_edit(idx, value);
        }
    }

    /// Enter on the selected row: act on it the way its widget wants.
    fn activate_selected_setting(&mut self) {
        use zest_config::ui::Widget;
        let Some(idx) = self.selected_settings_field() else { return };
        let Some(widget) = self
            .settings_ui
            .as_ref()
            .and_then(|ui| ui.fields.get(idx))
            .map(|f| f.widget)
        else {
            return;
        };
        match widget {
            // One keypress, one change: instant for the widgets whose next
            // value is unambiguous — a toggle, or a segmented control.
            Widget::Toggle => {
                self.adjust_selected_setting(1);
            }
            Widget::Select => {
                let segmented = self
                    .settings_ui
                    .as_ref()
                    .and_then(|ui| ui.fields.get(idx))
                    .is_some_and(crate::settings_ui::select_is_segmented);
                if segmented {
                    self.adjust_selected_setting(1);
                } else {
                    // The documented/long selects open their menu (§11) —
                    // the doc comments are the reason the menu exists.
                    let row = self.settings_ui.as_ref().map(|ui| ui.selected);
                    if let (Some(ui), Some(row)) = (self.settings_ui.as_mut(), row) {
                        ui.menu = Some((row, 0));
                    }
                }
            }
            // Long lists open a filtered picker instead of cycling. Stepping is
            // fine for a handful of themes and useless for 266 installed font
            // families, which is what this exists for -- the arrows still cycle
            // for anyone who wants them.
            Widget::FontList | Widget::ThemePicker => {
                if !self.open_value_picker() {
                    // Nothing to choose from: fall back to cycling rather than
                    // swallowing the keypress.
                    self.adjust_selected_setting(1);
                }
            }
            // Numbers, text and paths open a buffer: arrows step a number,
            // but "make it 18" should not be nine keypresses, and a string
            // has no other way in.
            Widget::Number | Widget::Slider | Widget::Text | Widget::Path => {
                let seed = self.settings_ui.as_ref().and_then(|ui| {
                    let field = ui.fields.get(idx)?;
                    let values = serde_json::to_value(&self.settings).ok()?;
                    Some(crate::settings_ui::edit_seed(
                        field,
                        zest_config::ui::value_at(&values, &field.key),
                    ))
                });
                if let (Some(ui), Some(buffer)) = (self.settings_ui.as_mut(), seed) {
                    ui.editing = Some(crate::settings_ui::EditBuffer {
                        field_idx: idx,
                        buffer,
                        error: false,
                        append: false,
                    });
                }
            }
            // Enter on a list row means "add one" — the add affordance's
            // keyboard spelling.
            Widget::TagList | Widget::KeyValue => {
                let row = self.settings_ui.as_ref().map(|ui| ui.selected);
                if let Some(row) = row {
                    self.begin_list_add(row);
                }
            }
            // The profile pickers belong to the profiles editor (#130),
            // which this tab does not render — when it does, they open
            // rosters the way ThemePicker does.
            Widget::HostPicker
            | Widget::SchemePicker
            | Widget::AccentPicker
            | Widget::IconPicker => {}
        }
        self.mark_chrome_dirty();
    }

    /// Set a slider row's value from a pointer x, against the track the last
    /// layout actually drew.
    ///
    /// Quantized to the arrow keys' grid and applied only when the quantized
    /// value changes — a drag is then at most twenty writes across the whole
    /// travel, not one per motion event.
    fn apply_slider_at(&mut self, row: usize, x: f32) {
        self.refresh_chrome();
        let Some(track) = self.chrome_layout.as_ref().and_then(|l| {
            l.settings_tracks.iter().find(|(i, _)| *i == row).map(|(_, r)| *r)
        }) else {
            return;
        };
        let frac = f64::from(((x - track[0]) / track[2]).clamp(0.0, 1.0));
        // Whose track: while the Profiles screen is up, the tracks were
        // drawn by it (the Settings model is not built then — the gate in
        // refresh_chrome is what makes this dispatch unambiguous).
        if self.profiles_tab_active() {
            let Some(field_idx) = self.profiles_field_of_row(row) else { return };
            let candidate = self
                .profiles_ui
                .as_ref()
                .and_then(|ui| ui.fields.get(field_idx))
                .and_then(|field| crate::settings_ui::slider_value(field, frac));
            let Some(candidate) = candidate else { return };
            if self.profiles_value_of(field_idx).as_ref() == Some(&candidate) {
                return;
            }
            self.profiles_apply_edit(field_idx, candidate);
            return;
        }
        let Some(field_idx) = self.settings_ui.as_ref().and_then(|ui| {
            match ui.actions.get(row) {
                Some(crate::settings_ui::RowAction::Field(i)) => Some(*i),
                _ => None,
            }
        }) else {
            return;
        };
        let candidate = self
            .settings_ui
            .as_ref()
            .and_then(|ui| ui.fields.get(field_idx))
            .and_then(|field| crate::settings_ui::slider_value(field, frac));
        let Some(candidate) = candidate else { return };
        if self.settings_value_of(field_idx).as_ref() == Some(&candidate) {
            return;
        }
        self.apply_edit(field_idx, candidate);
    }

    /// Write one edited setting through to the user's config file, then apply
    /// it by re-running the cascade synchronously.
    ///
    /// The file stays the single source of truth: the overlay never holds a
    /// value the file does not. The watcher will echo this write ~120ms
    /// later; its reload diffs to `Invalidation::None` and is a no-op — the
    /// synchronous reload here is what makes a toggle feel like a switch
    /// rather than a request.
    fn apply_edit(&mut self, field_idx: usize, new_value: serde_json::Value) {
        let Some((key, value)) = self.settings_ui.as_ref().and_then(|ui| {
            let field = ui.fields.get(field_idx)?;
            Some((field.key.clone(), crate::settings_ui::to_toml(field, &new_value)?))
        }) else {
            return;
        };
        // `config_file()` is None until the file exists — first-ever edit —
        // and in portable mode it points at `zesterm.toml`, which the
        // fallback would get wrong; that is why the file path wins.
        let Some(target) = zest_config::paths::config_file()
            .or_else(|| zest_config::paths::config_dir().map(|d| d.join(zest_config::paths::CONFIG_FILE)))
        else {
            self.settings_error = Some("no config directory on this system".to_string());
            self.mark_chrome_dirty();
            return;
        };
        match zest_config::write_value(&target, &key, value) {
            Ok(()) => {
                self.settings_error = None;
                if zest_config::invalidate::class_of(&key) == zest_config::Invalidation::Restart {
                    self.restart_pending.insert(key);
                }
                self.reload_config();
            }
            Err(e) => {
                tracing::error!(error = %e, key = %key, "could not write the setting");
                self.settings_error = Some(format!("could not save {key}: {e}"));
            }
        }
        self.mark_chrome_dirty();
    }

    /// Where an edit lands: the user's config file, existing or about to.
    fn config_target() -> Option<std::path::PathBuf> {
        zest_config::paths::config_file().or_else(|| {
            zest_config::paths::config_dir().map(|d| d.join(zest_config::paths::CONFIG_FILE))
        })
    }

    /// The field index a settings row stands for, when it is a real field.
    fn settings_field_of_row(&self, row: usize) -> Option<usize> {
        match self.settings_ui.as_ref()?.actions.get(row) {
            Some(crate::settings_ui::RowAction::Field(i)) => Some(*i),
            _ => None,
        }
    }

    /// The rail's visible categories under the live filter — the click
    /// handler resolves `SettingsCategory(i)` against exactly what the model
    /// showed, or filtered-away rows would take clicks for their neighbours.
    fn visible_categories(&self) -> Vec<String> {
        use crate::settings_ui as sui;
        let Some(ui) = self.settings_ui.as_ref() else { return Vec::new() };
        sui::categories(&ui.fields)
            .into_iter()
            .filter(|g| {
                // Only a live filter hides categories (§11) — the unknown
                // category included: clean, unfiltered, it stays in the rail
                // and its page carries the empty-state line.
                ui.filter.is_empty()
                    || sui::category_matches(&ui.fields, g, &ui.filter, &self.unknown_keys)
            })
            .collect()
    }

    /// Select the rail's `i`-th visible category; selection and scroll reset
    /// — a category is a fresh page, not a continuation.
    fn select_settings_category(&mut self, i: usize) {
        let Some(label) = self.visible_categories().get(i).cloned() else { return };
        if let Some(ui) = self.settings_ui.as_mut() {
            if ui.category != label {
                ui.category = label;
                ui.selected = 0;
                ui.scroll = 0.0;
                ui.scroll_to_selected = true;
                ui.editing = None;
                ui.menu = None;
            }
        }
        self.mark_chrome_dirty();
    }

    /// The modified dot's click: delete the key from the file, reload — the
    /// dot is the reset button (§11), and the file stays the single source
    /// of truth. Idempotent because `remove_value` is.
    fn reset_setting_row(&mut self, row: usize) {
        let Some(key) = self
            .settings_field_of_row(row)
            .and_then(|i| self.settings_ui.as_ref()?.fields.get(i).map(|f| f.key.clone()))
        else {
            return;
        };
        let Some(target) = Self::config_target() else {
            self.settings_error = Some("no config directory on this system".to_string());
            self.mark_chrome_dirty();
            return;
        };
        match zest_config::remove_value(&target, &key) {
            Ok(()) => {
                self.settings_error = None;
                self.reload_config();
            }
            Err(e) => {
                tracing::error!(error = %e, key = %key, "could not reset the setting");
                self.settings_error = Some(format!("could not reset {key}: {e}"));
            }
        }
        self.mark_chrome_dirty();
    }

    /// "Edit as TOML": hand the config file to the OS handler.
    fn open_config_externally(&mut self) {
        let Some(target) = Self::config_target() else { return };
        platform::open_path(&target);
    }

    /// Write one of a select field's variants — segmented segments and
    /// dropdown rows both land here, and from here in `apply_edit`.
    fn apply_variant(&mut self, row: usize, opt: usize) {
        let Some((idx, value)) = self.settings_field_of_row(row).and_then(|i| {
            let field = self.settings_ui.as_ref()?.fields.get(i)?;
            let variant = field.variants.get(opt)?;
            Some((i, serde_json::Value::String(variant.value.clone())))
        }) else {
            return;
        };
        self.apply_edit(idx, value);
    }

    /// A list item's ×: fonts and tags lose the item, an env entry loses its
    /// key. The whole new value goes through `apply_edit` — no second path.
    fn remove_list_item(&mut self, row: usize, item: usize) {
        use zest_config::ui::Widget;
        let Some(idx) = self.settings_field_of_row(row) else { return };
        let Some(widget) =
            self.settings_ui.as_ref().and_then(|ui| ui.fields.get(idx)).map(|f| f.widget)
        else {
            return;
        };
        let Some(current) = self.settings_value_of(idx) else { return };
        let next = match widget {
            Widget::FontList | Widget::TagList => {
                let mut arr = current.as_array().cloned().unwrap_or_default();
                if item >= arr.len() {
                    return;
                }
                arr.remove(item);
                serde_json::Value::Array(arr)
            }
            Widget::KeyValue => {
                let Some(map) = current.as_object() else { return };
                let Some(key) = map.keys().nth(item).cloned() else { return };
                let mut map = map.clone();
                map.remove(&key);
                serde_json::Value::Object(map)
            }
            _ => return,
        };
        self.apply_edit(idx, next);
    }

    /// The dashed add affordance: fonts open the existing value picker (in
    /// append mode); tags and env entries open a typed buffer whose Enter
    /// appends.
    fn begin_list_add(&mut self, row: usize) {
        use zest_config::ui::Widget;
        let Some(idx) = self.settings_field_of_row(row) else { return };
        let Some(widget) =
            self.settings_ui.as_ref().and_then(|ui| ui.fields.get(idx)).map(|f| f.widget)
        else {
            return;
        };
        if let Some(ui) = self.settings_ui.as_mut() {
            ui.selected = row;
        }
        match widget {
            Widget::FontList => {
                if self.open_value_picker() {
                    if let Some(p) = self.value_picker.as_mut() {
                        p.append = true;
                    }
                }
            }
            Widget::TagList | Widget::KeyValue => {
                if let Some(ui) = self.settings_ui.as_mut() {
                    ui.editing = Some(crate::settings_ui::EditBuffer {
                        field_idx: idx,
                        buffer: String::new(),
                        error: false,
                        append: true,
                    });
                }
            }
            _ => {}
        }
        self.mark_chrome_dirty();
    }

    /// Commit an append buffer: a tag verbatim (a leading `-` disables and
    /// is kept), or a `KEY=VALUE` env entry — a bare `KEY` gets an empty
    /// value, which *unsets* under the wholesale-replace semantics. Returns
    /// false when the input cannot be an entry (shown as a buffer error).
    fn commit_list_append(&mut self, idx: usize, text: &str) -> bool {
        use zest_config::ui::Widget;
        let Some(widget) =
            self.settings_ui.as_ref().and_then(|ui| ui.fields.get(idx)).map(|f| f.widget)
        else {
            return false;
        };
        let text = text.trim();
        if text.is_empty() {
            // Committing nothing is closing the buffer, not an error.
            return true;
        }
        let Some(current) = self.settings_value_of(idx) else { return false };
        let next = match widget {
            Widget::TagList => {
                let mut arr = current.as_array().cloned().unwrap_or_default();
                arr.push(serde_json::Value::String(text.to_string()));
                serde_json::Value::Array(arr)
            }
            Widget::KeyValue => {
                let (key, value) = match text.split_once('=') {
                    Some((k, v)) => (k.trim(), v.trim()),
                    None => (text, ""),
                };
                if key.is_empty() {
                    return false;
                }
                let mut map = current.as_object().cloned().unwrap_or_default();
                map.insert(key.to_string(), serde_json::Value::String(value.to_string()));
                serde_json::Value::Object(map)
            }
            _ => return false,
        };
        self.apply_edit(idx, next);
        true
    }

    /// Drag-to-reorder on a font row: move `from` to `to` — order is the
    /// setting, so the move writes through the file like every other edit.
    fn reorder_list_item(&mut self, row: usize, from: usize, to: usize) {
        let Some(idx) = self.settings_field_of_row(row) else { return };
        let Some(current) = self.settings_value_of(idx) else { return };
        let Some(arr) = current.as_array() else { return };
        if from >= arr.len() || to >= arr.len() || from == to {
            return;
        }
        let mut arr = arr.clone();
        let moved = arr.remove(from);
        arr.insert(to, moved);
        self.apply_edit(idx, serde_json::Value::Array(arr));
    }

    // ----- The Profiles editor's input path (§12) --------------------------
    //
    // The Settings tab's shape, scoped to `[profiles.<name>]`: every edit is
    // write_profile_value → reload_config → rows rebuilt from the resolved
    // file — one state path; the editor never holds a value the file does
    // not. The reload re-resolves open tabs' identities (#162), which is
    // what makes a scheme edit restyle a running tab live.

    /// The field index a profiles row stands for, when it is a real field.
    fn profiles_field_of_row(&self, row: usize) -> Option<usize> {
        match self.profiles_ui.as_ref()?.actions.get(row) {
            Some(crate::settings_ui::RowAction::Field(i)) => Some(*i),
            _ => None,
        }
    }

    /// The selected profiles row's field index, when it is a real field.
    fn profiles_selected_field(&self) -> Option<usize> {
        self.profiles_field_of_row(self.profiles_ui.as_ref()?.selected)
    }

    /// The value a profiles row currently SHOWS — the profile's resolved
    /// value, or the window's where the profile is silent — so an arrow
    /// press or slider drag steps from what is on screen, exactly like the
    /// Settings tab. Not the typed-edit seed: that is [`Self::profiles_seed_of`].
    fn profiles_value_of(&self, field_idx: usize) -> Option<serde_json::Value> {
        self.profiles_eval(field_idx, crate::profiles_ui::effective_value)
    }

    /// The value a typed edit opens with — the launch strings seed the
    /// profile's own resolved value (empty when unset), never the display
    /// fallback: `effective_value` captions an unset `command` with
    /// `shell_fallback()` (on a remote route, "the host's default shell")
    /// and an unset `host` with a label no fleet entry carries, and seeding
    /// either puts two Enters between the caption and a real
    /// `[profiles.<name>]` value a launch would spawn verbatim.
    fn profiles_seed_of(&self, field_idx: usize) -> Option<serde_json::Value> {
        self.profiles_eval(field_idx, crate::profiles_ui::edit_seed_value)
    }

    fn profiles_eval(
        &self,
        field_idx: usize,
        eval: fn(
            &zest_config::ui::UiField,
            &zest_config::profiles::ProfileResolved,
            &serde_json::Value,
            &crate::profiles_ui::ProfileRowContext,
        ) -> serde_json::Value,
    ) -> Option<serde_json::Value> {
        use crate::profiles_ui as pui;
        let ui = self.profiles_ui.as_ref()?;
        let field = ui.fields.get(field_idx)?;
        let root = crate::launcher::profiles_root(&self.settings);
        let resolved = zest_config::profiles::resolve_profile(&root, &ui.profile);
        let overrides = pui::overrides_json(&resolved);
        let window_values = serde_json::to_value(&self.settings).ok()?;
        let fallback = self.shell_fallback();
        // hosts/schemes are display-only inputs neither evaluator reads
        // (their docs pin that), so the input path skips the snapshot. The
        // placeholder `local_host` is likewise never written: the launch
        // strings it captions refuse arrow-adjust (`adjust_profile` returns
        // `None`) and seed through `edit_seed_value`, which ignores it.
        let ctx = pui::ProfileRowContext {
            window_values: &window_values,
            window_theme: &self.config.theme,
            fallback_command: &fallback,
            local_host: "this machine",
            hosts: &[],
            schemes: &[],
            is_defaults: ui.profile == zest_config::profiles::RESERVED_PROFILE,
        };
        Some(eval(field, &resolved, &overrides, &ctx))
    }

    /// Write one edited value into `[profiles.<name>]`, then reload — never
    /// the root file: the root is the window's, the table is the profile's.
    fn profiles_apply_edit(&mut self, field_idx: usize, new_value: serde_json::Value) {
        let Some((profile, key, value)) = self.profiles_ui.as_ref().and_then(|ui| {
            let field = ui.fields.get(field_idx)?;
            Some((
                ui.profile.clone(),
                field.key.clone(),
                crate::settings_ui::to_toml(field, &new_value)?,
            ))
        }) else {
            return;
        };
        let Some(target) = Self::config_target() else {
            if let Some(ui) = self.profiles_ui.as_mut() {
                ui.error = Some("no config directory on this system".to_string());
            }
            self.mark_chrome_dirty();
            return;
        };
        match zest_config::write_profile_value(&target, &profile, &key, value) {
            Ok(()) => {
                if let Some(ui) = self.profiles_ui.as_mut() {
                    ui.error = None;
                }
                self.reload_config();
            }
            Err(e) => {
                tracing::error!(error = %e, key = %key, profile = %profile, "could not write the profile value");
                if let Some(ui) = self.profiles_ui.as_mut() {
                    ui.error = Some(format!("could not save {key}: {e}"));
                }
            }
        }
        self.mark_chrome_dirty();
    }

    /// The override dot's click: delete the key from `[profiles.<name>]`,
    /// reload — the row falls back through Defaults (§12). Idempotent
    /// because `remove_profile_value` is.
    fn profiles_reset_row(&mut self, row: usize) {
        let Some((profile, key)) = self.profiles_field_of_row(row).and_then(|i| {
            let ui = self.profiles_ui.as_ref()?;
            Some((ui.profile.clone(), ui.fields.get(i)?.key.clone()))
        }) else {
            return;
        };
        let Some(target) = Self::config_target() else { return };
        match zest_config::remove_profile_value(&target, &profile, &key) {
            Ok(()) => {
                if let Some(ui) = self.profiles_ui.as_mut() {
                    ui.error = None;
                }
                self.reload_config();
            }
            Err(e) => {
                tracing::error!(error = %e, key = %key, profile = %profile, "could not clear the override");
                if let Some(ui) = self.profiles_ui.as_mut() {
                    ui.error = Some(format!("could not reset {key}: {e}"));
                }
            }
        }
        self.mark_chrome_dirty();
    }

    /// Arrow-key editing on the selected profiles row: flip, cycle or step,
    /// then write through the profile path.
    fn profiles_adjust(&mut self, dir: i32) {
        let Some(idx) = self.profiles_selected_field() else { return };
        let Some(current) = self.profiles_value_of(idx) else { return };
        let installed: Vec<String> = self
            .profiles_ui
            .as_ref()
            .and_then(|ui| ui.fields.get(idx))
            .filter(|f| f.widget == zest_config::ui::Widget::FontList)
            .and(self.fonts.as_mut())
            .map(Fonts::installed_families)
            .unwrap_or_default();
        let next = self.profiles_ui.as_ref().and_then(|ui| {
            let field = ui.fields.get(idx)?;
            let themes: Vec<String> =
                zest_theme::builtin::all().into_iter().map(|t| t.id).collect();
            crate::profiles_ui::adjust_profile(field, &current, dir, &themes, &installed)
        });
        if let Some(value) = next {
            self.profiles_apply_edit(idx, value);
        }
    }

    /// Enter on the selected profiles row: act the way its widget wants.
    fn profiles_activate_selected(&mut self) {
        use zest_config::ui::Widget;
        let Some(idx) = self.profiles_selected_field() else { return };
        let Some((widget, key, segmented)) =
            self.profiles_ui.as_ref().and_then(|ui| ui.fields.get(idx)).map(|f| {
                (f.widget, f.key.clone(), crate::settings_ui::select_is_segmented(f))
            })
        else {
            return;
        };
        match widget {
            Widget::Toggle => self.profiles_adjust(1),
            // tab_title's third state is typing (the #135 contract: any
            // other string is a custom title) — Enter opens a buffer; the
            // drawn segments stay the two fixed spellings.
            Widget::Select if key == "tab_title" => self.profiles_begin_edit(idx),
            Widget::Select if segmented => self.profiles_adjust(1),
            Widget::Select => {
                let row = self.profiles_ui.as_ref().map(|ui| ui.selected);
                if let (Some(ui), Some(row)) = (self.profiles_ui.as_mut(), row) {
                    ui.menu = Some((row, 0));
                }
            }
            // The fleet-picker-as-chooser belongs to the cross-host launch
            // item (a picker row *launches* today, which is not choosing);
            // until then the host is typed — the same write path, honestly.
            Widget::HostPicker => self.profiles_begin_edit(idx),
            Widget::Number | Widget::Slider | Widget::Text | Widget::Path => {
                self.profiles_begin_edit(idx);
            }
            // The direct-choice rows also answer Enter by stepping, so the
            // keyboard can drive them without a pointer.
            Widget::SchemePicker
            | Widget::AccentPicker
            | Widget::IconPicker
            | Widget::FontList
            | Widget::ThemePicker => self.profiles_adjust(1),
            Widget::TagList | Widget::KeyValue => {}
        }
        self.mark_chrome_dirty();
    }

    /// Open a typed edit on a profiles field, seeded with the profile's own
    /// value — see `profiles_seed_of` for why not with what the row shows.
    fn profiles_begin_edit(&mut self, idx: usize) {
        let current = self.profiles_seed_of(idx);
        let seed = match &current {
            // Strings seed verbatim whatever the widget (host, tab_title,
            // command); numbers go through the settings seeding.
            Some(serde_json::Value::String(s)) => s.clone(),
            other => self
                .profiles_ui
                .as_ref()
                .and_then(|ui| ui.fields.get(idx))
                .map(|f| crate::settings_ui::edit_seed(f, other.as_ref()))
                .unwrap_or_default(),
        };
        if let Some(ui) = self.profiles_ui.as_mut() {
            ui.editing = Some(crate::settings_ui::EditBuffer {
                field_idx: idx,
                buffer: seed,
                error: false,
                append: false,
            });
        }
        self.mark_chrome_dirty();
    }

    /// A direct-choice click (scheme swatch, accent swatch, icon tile).
    fn profiles_choice(&mut self, row: usize, opt: usize) {
        if let Some(ui) = self.profiles_ui.as_mut() {
            ui.selected = row;
        }
        let Some(idx) = self.profiles_field_of_row(row) else { return };
        let value = self.profiles_ui.as_ref().and_then(|ui| {
            let field = ui.fields.get(idx)?;
            crate::profiles_ui::choice_value(field, opt, &crate::profiles_ui::scheme_swatches())
        });
        if let Some(value) = value {
            self.profiles_apply_edit(idx, value);
        } else {
            self.mark_chrome_dirty();
        }
    }

    /// Write one of a select field's variants — profiles-side twin of
    /// `apply_variant`, landing in the profile's table.
    fn profiles_apply_variant(&mut self, row: usize, opt: usize) {
        let Some((idx, value)) = self.profiles_field_of_row(row).and_then(|i| {
            let field = self.profiles_ui.as_ref()?.fields.get(i)?;
            let variant = field.variants.get(opt)?;
            Some((i, serde_json::Value::String(variant.value.clone())))
        }) else {
            return;
        };
        self.profiles_apply_edit(idx, value);
    }

    /// Select the rail's `i`-th profile for editing; selection and scroll
    /// reset — a profile is a fresh page, not a continuation.
    fn profiles_select_rail(&mut self, i: usize) {
        let names = crate::profiles_ui::rail_names(&self.settings);
        let Some(name) = names.get(i).cloned() else { return };
        if let Some(ui) = self.profiles_ui.as_mut() {
            if ui.profile != name {
                ui.profile = name;
                ui.selected = 0;
                ui.scroll = 0.0;
                ui.scroll_to_selected = true;
                ui.editing = None;
                ui.menu = None;
                ui.error = None;
            }
        }
        self.mark_chrome_dirty();
    }

    /// "＋ New profile": create `[profiles.new-profile-N]` (unique), reload,
    /// select it.
    fn profiles_new(&mut self) {
        let Some(target) = Self::config_target() else {
            if let Some(ui) = self.profiles_ui.as_mut() {
                ui.error = Some("no config directory on this system".to_string());
            }
            self.mark_chrome_dirty();
            return;
        };
        let names = crate::profiles_ui::rail_names(&self.settings);
        let name = crate::profiles_ui::new_profile_name(&names);
        match zest_config::create_profile(&target, &name) {
            Ok(()) => {
                self.reload_config();
                if let Some(ui) = self.profiles_ui.as_mut() {
                    ui.profile = name;
                    ui.selected = 0;
                    ui.scroll = 0.0;
                    ui.scroll_to_selected = true;
                    ui.error = None;
                }
            }
            Err(e) => {
                tracing::error!(error = %e, profile = %name, "could not create the profile");
                if let Some(ui) = self.profiles_ui.as_mut() {
                    ui.error = Some(format!("could not create {name}: {e}"));
                }
            }
        }
        self.mark_chrome_dirty();
    }

    /// Duplicate the edited profile under a unique sibling name and select
    /// the copy. Renaming rides this plus Delete (§12 offered either shape;
    /// this one keeps the header name read-only).
    fn profiles_duplicate(&mut self) {
        let Some(from) = self.profiles_ui.as_ref().map(|ui| ui.profile.clone()) else { return };
        let Some(target) = Self::config_target() else { return };
        let names = crate::profiles_ui::rail_names(&self.settings);
        let to = crate::profiles_ui::copy_name(&names, &from);
        // Defaults (or a layer-supplied profile) may have no table in the
        // user's file to copy — an empty duplicate still falls through
        // Defaults, which is exactly what a copy of it means.
        let result = zest_config::copy_profile(&target, &from, &to).or_else(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                zest_config::create_profile(&target, &to)
            } else {
                Err(e)
            }
        });
        match result {
            Ok(()) => {
                self.reload_config();
                if let Some(ui) = self.profiles_ui.as_mut() {
                    ui.profile = to;
                    ui.scroll_to_selected = true;
                    ui.error = None;
                }
            }
            Err(e) => {
                tracing::error!(error = %e, profile = %from, "could not duplicate the profile");
                if let Some(ui) = self.profiles_ui.as_mut() {
                    ui.error = Some(format!("could not duplicate {from}: {e}"));
                }
            }
        }
        self.mark_chrome_dirty();
    }

    /// Delete the edited profile; the editor falls back to Defaults. The
    /// screen never draws Delete for Defaults, and this guards it anyway.
    fn profiles_delete(&mut self) {
        let Some(name) = self.profiles_ui.as_ref().map(|ui| ui.profile.clone()) else { return };
        if name == zest_config::profiles::RESERVED_PROFILE {
            return;
        }
        let Some(target) = Self::config_target() else { return };
        match zest_config::remove_profile(&target, &name) {
            Ok(()) => {
                self.reload_config();
                if let Some(ui) = self.profiles_ui.as_mut() {
                    ui.profile = zest_config::profiles::RESERVED_PROFILE.to_string();
                    ui.selected = 0;
                    ui.scroll = 0.0;
                    ui.error = None;
                }
            }
            Err(e) => {
                tracing::error!(error = %e, profile = %name, "could not delete the profile");
                if let Some(ui) = self.profiles_ui.as_mut() {
                    ui.error = Some(format!("could not delete {name}: {e}"));
                }
            }
        }
        self.mark_chrome_dirty();
    }

    /// The picker's rows and their actions, from the fleet snapshot.
    ///
    /// One pass builds both lists, which is what keeps a drawn row and its
    /// meaning aligned by construction — the hit map's discipline, applied
    /// to indices.
    fn build_picker(&self) -> (Vec<crate::chrome::model::PickerRow>, Vec<PickerAction>) {
        use crate::chrome::model::PickerRow;
        use crate::fleet::SessionsState;

        let mut rows = Vec::new();
        let mut actions = Vec::new();
        let filter = self
            .picker
            .as_ref()
            .map(|p| p.filter.to_lowercase())
            .unwrap_or_default();
        let matches = |text: &str| filter.is_empty() || text.to_lowercase().contains(&filter);

        // Blocks first — the palette is primarily a history of what ran
        // anywhere in the fleet (design screen 6). Gathered from every
        // attached tab; unattached sessions' history has not crossed the
        // wire and pretending otherwise would be a lie with a scrollbar.
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let mut history: Vec<(u64, PickerRow, PickerAction)> = Vec::new();
        for tab in self.tabs.iter() {
            let host = match tab.source().origin() {
                Origin::Daemon { host, local: false } => host,
                _ => String::new(),
            };
            let term = tab.source().terminal();
            let term = term.lock();
            for b in term.blocks().blocks() {
                let command = b.command.trim();
                if command.is_empty() || b.output_line.is_none() || !matches(command) {
                    continue;
                }
                let when = b.ended_ms.or(b.started_ms).unwrap_or(0);
                let ago = match b.ended_ms {
                    _ if b.is_running() => "running".to_string(),
                    Some(e) => {
                        crate::status::age_label(std::time::Duration::from_millis(
                            now_ms.saturating_sub(e),
                        )) + " ago"
                    }
                    None => String::new(),
                };
                let outcome = match b.state {
                    zest_core::BlockState::Finished { exit_code: Some(c) } => format!("exit {c}"),
                    zest_core::BlockState::Finished { exit_code: None } => "done".to_string(),
                    _ => String::new(),
                };
                let provenance = [host.as_str(), ago.as_str(), outcome.as_str()]
                    .iter()
                    .filter(|p| !p.is_empty())
                    .copied()
                    .collect::<Vec<_>>()
                    .join(" \u{b7} ");
                history.push((
                    when,
                    PickerRow::Block {
                        command: command.to_string(),
                        provenance,
                        ok: !b.failed(),
                    },
                    PickerAction::RunBlock { origin: tab.addr, command: command.to_string() },
                ));
            }
        }
        history.sort_by(|a, b| b.0.cmp(&a.0));
        let cap = if filter.is_empty() { 4 } else { 8 };
        if !history.is_empty() {
            rows.push(PickerRow::Group { title: "Blocks".into() });
            actions.push(PickerAction::None);
            for (_, row, action) in history.into_iter().take(cap) {
                rows.push(row);
                actions.push(action);
            }
        }

        // Sessions and hosts, from the fleet.
        let fleet_hosts = self.fleet.as_ref().map(|f| f.snapshot()).unwrap_or_default();
        let mut session_rows = Vec::new();
        let mut host_rows = Vec::new();
        for host in &fleet_hosts {
            let presence = match host.presence {
                zest_mesh::discovery::Presence::Online => TabPresence::Online,
                zest_mesh::discovery::Presence::Away => TabPresence::Away,
                zest_mesh::discovery::Presence::Unseen => TabPresence::Unseen,
                zest_mesh::discovery::Presence::Unreachable => TabPresence::Unreachable,
            };
            // How this host is dialled: the window's own route for the local
            // machine, its advertised endpoint otherwise.
            let route = if host.local {
                self.route.clone()
            } else {
                host.address.clone().map(HostRoute::Tcp)
            };

            if let SessionsState::Fresh(sessions) = &host.sessions {
                for info in sessions {
                    let title = info.title.trim();
                    let title =
                        if title.is_empty() { "shell".to_string() } else { title.to_string() };
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
                // an action that must fail is worse than saying so.
                let action = match route {
                    Some(route) if !matches!(presence, TabPresence::Unreachable) => {
                        PickerAction::Create { host: host.host, route }
                    }
                    _ => PickerAction::None,
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

        (rows, actions)
    }

    /// Act on a picker row. Every action closes the picker: the user chose.
    fn run_picker_action(&mut self, action: PickerAction, el: &ActiveEventLoop, shift: bool) {
        // A pending ask_host launch rides exactly one picker choice: a host
        // row launches the profile there, anything else abandons it (and
        // dismissing the picker abandons it structurally — it lives on the
        // picker's own state).
        let pending_profile = self.picker.as_mut().and_then(|p| p.pending_profile.take());
        match action {
            PickerAction::None => return,
            PickerAction::RunBlock { origin, command } => {
                self.picker = None;
                self.screen = AppScreen::Terminal;
                // ⏎ runs here; ⇧⏎ runs where the command came from — the
                // honest half of "run on host…" until a chooser exists.
                if shift && self.tabs.activate_addr(origin) {
                    self.after_activation();
                }
                if let Some(session) = self.tabs.active_source() {
                    let mut bytes = command.into_bytes();
                    bytes.push(b'\r');
                    session.write(bytes);
                }
            }
            PickerAction::Perform(action) => {
                self.picker = None;
                self.mark_chrome_dirty();
                self.perform(action, el);
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
            }
            PickerAction::Attach { addr, route } => {
                self.picker = None;
                self.screen = AppScreen::Terminal;
                self.spawn_tab_worker(route, Some(addr));
            }
            PickerAction::Create { host, route } => {
                self.picker = None;
                self.screen = AppScreen::Terminal;
                // The ask_host flow (design §12): the picker was opened to
                // choose this profile's host, and this row is the choice.
                if let Some(name) = pending_profile {
                    let meta = crate::launcher::profile_meta(&self.settings, &name);
                    let target = match &route {
                        HostRoute::Tcp(addr) => {
                            crate::launch::HostTarget::Remote { host, addr: addr.clone() }
                        }
                        HostRoute::LocalSocket(_) => crate::launch::HostTarget::Local,
                    };
                    // The picked host's display name, for the provenance
                    // line — the profile itself pinned none (that is what
                    // ask_host means).
                    let label = self
                        .fleet
                        .as_ref()
                        .map(|f| f.snapshot())
                        .unwrap_or_default()
                        .iter()
                        .find(|h| h.host == host)
                        .map(|h| h.label.clone())
                        .unwrap_or_default();
                    self.launch_profile_at(&name, &meta, target, label);
                    return;
                }
                // Pin remote creates to the host the roster named: the
                // address came from an advertisement, which is a claim.
                let expect = (!route.is_local()).then_some(host);
                self.spawn_tab_worker_pinned(route, None, expect, true);
            }
        }
        self.mark_chrome_dirty();
    }

    /// The stored identity for hosts that are not this machine, loaded on
    /// first need — the keychain stays off the startup path, and off every
    /// path for people who never leave loopback. Falls back to a throwaway
    /// key with a loud log, same trade as `--attach`.
    fn remote_identity(&mut self) -> Option<Arc<zest_mesh::identity::ClientIdentity>> {
        if self.remote_identity.is_none() {
            let store = zest_mesh::keystore::OsKeyStore;
            match zest_mesh::identity::ClientIdentity::load_or_create(&store) {
                Ok(i) => self.remote_identity = Some(Arc::new(i)),
                Err(e) => {
                    tracing::warn!(
                        error = %e,
                        "no credential store; using a throwaway key, the far host \
                         will ask for approval every time"
                    );
                    self.remote_identity =
                        zest_mesh::identity::ClientIdentity::generate().ok().map(Arc::new);
                }
            }
        }
        self.remote_identity.clone()
    }

    fn spawn_tab_worker(&mut self, route: HostRoute, attach: Option<zest_proto::SessionAddr>) {
        let expect = attach.and_then(|a| (!route.is_local()).then_some(a.host));
        self.spawn_tab_worker_pinned(route, attach, expect, true);
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

        self.next_placeholder += 1;
        let cell = Arc::new(parking_lot::Mutex::new(crate::tabs::placeholder_addr(
            self.next_placeholder,
        )));
        let wake = wake_for(&self.proxy, Arc::clone(&cell), Arc::clone(&self.activity));
        let pending = Arc::clone(&self.pending_tabs);
        let proxy = self.proxy.clone();
        let palette = self.palette.clone();

        let spawned = std::thread::Builder::new().name("zest-tab-open".into()).spawn(move || {
            let opts = crate::remote::AttachOptions {
                identity: &identity,
                label: "zesterm",
                command: &command,
                cwd: "",
                cols,
                rows,
                scrollback,
                adopt: false,
                local,
                expect_host,
            };
            let result = match attach {
                Some(addr) => {
                    crate::remote::RemoteSession::attach_existing(route.dialer(), addr, &opts, wake)
                }
                None => crate::remote::RemoteSession::create_and_attach(route.dialer(), &opts, wake),
            };
            match result {
                Ok(session) => {
                    *cell.lock() = session.addr();
                    session.terminal().lock().set_palette(palette);
                    let hint = match &route {
                        HostRoute::Tcp(a) => Some(a.clone()),
                        HostRoute::LocalSocket(_) => None,
                    };
                    pending
                        .lock()
                        .push((Tab::daemon(session, local, (cols, rows)).with_dial_hint(hint), focus));
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

    /// What to run in a fresh local shell, per the settings.
    fn build_spec(&self) -> CommandSpec {
        let mut spec = CommandSpec::default_shell();
        if let Some(shell) = &self.config.shell {
            spec.command_line = shell.clone();
        }
        // The in-process path gets the same hook as the daemon's, or
        // `--no-daemon` would silently be a terminal without command blocks.
        if let Some(dir) = zest_config::paths::config_dir() {
            spec.enable_shell_integration(&dir.join("shell-integration"));
        }
        spec
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
        self.open_shell_tab(None, None, None);
    }

    /// The command a command-less launch resolves to — the launcher rows'
    /// caption and the profiles editor's unset `command` row. On a remote
    /// route the far host runs its own default shell, and captioning that
    /// with this machine's `shell.command` would name a command that will
    /// not run.
    fn shell_fallback(&self) -> String {
        match self.route {
            Some(HostRoute::Tcp(_)) => "the host's default shell".to_string(),
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
    fn launch_profile(&mut self, name: &str) {
        let meta = crate::launcher::profile_meta(&self.settings, name);
        if meta.ask_host {
            // Host-agnostic profile: the fleet picker chooses the machine,
            // and its Create action carries this launch there.
            if self.picker.is_none() {
                self.toggle_picker();
            }
            if let Some(p) = self.picker.as_mut() {
                p.pending_profile = Some(name.to_string());
            }
            return;
        }
        let fleet = self.fleet.as_ref().map(|f| f.snapshot()).unwrap_or_default();
        let target = crate::launch::resolve_host(meta.host.as_deref(), &fleet);
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
                self.open_shell_tab(meta.command.clone(), Some(identity), cwd);
            }
            target => {
                let command = crate::launch::launch_command(
                    meta.command.clone(),
                    false,
                    self.config.shell.as_deref(),
                );
                self.spawn_connecting_tab(
                    name,
                    identity,
                    command,
                    cwd.unwrap_or_default(),
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
    fn spawn_connecting_tab(
        &mut self,
        name: &str,
        identity: crate::tabs::ProfileIdentity,
        command: String,
        cwd: String,
        host_label: String,
        target: crate::launch::HostTarget,
    ) {
        let Some(client) = self.remote_identity() else {
            tracing::warn!("no identity to dial with; cannot launch the profile");
            return;
        };

        let (cols, rows) = self.current_dims();
        let seed = self.palette_for(Some(&identity));
        let shown_command = if command.is_empty() { "the host's default shell" } else { &command };
        let provenance = format!("New session \u{b7} {name} on {host_label} \u{b7} {shown_command}");
        let pending = crate::tabs::PendingSession::new(
            cols,
            rows,
            seed.clone(),
            name,
            &provenance,
            &host_label,
        );
        self.next_placeholder += 1;
        let placeholder = crate::tabs::placeholder_addr(self.next_placeholder);
        let hint = match &target {
            crate::launch::HostTarget::Remote { addr, .. } => Some(addr.clone()),
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
        let spawned = std::thread::Builder::new().name("zest-profile-launch".into()).spawn(
            move || {
                let mut failures = 0u32;
                let outcome = loop {
                    let attempt = match &target {
                        crate::launch::HostTarget::Remote { host, addr } => {
                            let route = HostRoute::Tcp(addr.clone());
                            let wake = wake_for(&proxy, Arc::clone(&cell), Arc::clone(&activity));
                            crate::remote::RemoteSession::create_and_attach(
                                route.dialer(),
                                &crate::remote::AttachOptions {
                                    identity: &client,
                                    label: "zesterm",
                                    command: &command,
                                    cwd: &cwd,
                                    cols,
                                    rows,
                                    scrollback,
                                    adopt: false,
                                    local: false,
                                    // The address came from an advertisement,
                                    // which is a claim; pin the identity it
                                    // claimed, like the picker's creates do.
                                    expect_host: Some(*host),
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
    fn open_shell_tab(
        &mut self,
        command: Option<String>,
        identity: Option<crate::tabs::ProfileIdentity>,
        cwd: Option<String>,
    ) {
        let (cols, rows) = self.current_dims();
        // Seeded before the first byte arrives, so the grid never flashes
        // the window's palette under a profile's scheme.
        let seed = self.palette_for(identity.as_ref());

        match (&self.route, &self.client_identity) {
            (Some(route), Some(client)) => {
                self.next_placeholder += 1;
                let cell = Arc::new(parking_lot::Mutex::new(crate::tabs::placeholder_addr(
                    self.next_placeholder,
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
                let session = crate::remote::RemoteSession::create_and_attach(
                    route.dialer(),
                    &crate::remote::AttachOptions {
                        identity: client,
                        label: "zesterm",
                        command: &command,
                        cwd: cwd.as_deref().unwrap_or_default(),
                        cols,
                        rows,
                        scrollback: self.config.scrollback,
                        adopt: false,
                        local: route.is_local(),
                        expect_host: None,
                    },
                    wake,
                );
                match session {
                    Ok(session) => {
                        *cell.lock() = session.addr();
                        session.terminal().lock().set_palette(seed);
                        let local = route.is_local();
                        // A create should never collide, but the daemon owns
                        // session ids — adopt guards every path the same way
                        // (#188); a refused duplicate detaches on drop.
                        let tab = Tab::daemon(session, local, (cols, rows)).with_identity(identity);
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
                self.next_placeholder += 1;
                let addr = crate::tabs::placeholder_addr(self.next_placeholder);
                let cell = Arc::new(parking_lot::Mutex::new(addr));
                let mut spec = self.build_spec();
                if let Some(c) = &command {
                    spec.command_line = c.clone();
                }
                // The profile's starting_directory, resolved by the machine
                // that spawns — here, this one (§12: the daemon path sends
                // it over the wire instead).
                if let Some(dir) = cwd.as_deref().filter(|d| !d.is_empty()) {
                    spec.cwd = Some(dir.into());
                }
                match Session::spawn(
                    &spec,
                    PtySize::new(cols, rows),
                    self.config.scrollback,
                    wake_for(&self.proxy, cell, Arc::clone(&self.activity)),
                ) {
                    Ok(session) => {
                        session.terminal().lock().set_palette(seed);
                        self.tabs
                            .push(Tab::in_process(session, addr, (cols, rows)).with_identity(identity));
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
        self.persist_tabs();
    }

    /// Close one tab: local sessions die, remote ones are only let go of.
    ///
    /// `already_exited` marks the child as gone (a `TabExited` wakeup), where
    /// there is nothing left to kill. The last tab closing closes the window.
    fn close_tab(&mut self, addr: zest_proto::SessionAddr, already_exited: bool, el: &ActiveEventLoop) {
        // The Settings tab has no session to kill or detach; closing it is
        // dropping its state and returning the keyboard (§11).
        if addr == crate::tabs::settings_addr() {
            self.close_settings_tab();
            return;
        }
        let was_active = self.tabs.is_active(addr);
        let Some(tab) = self.tabs.close(addr) else { return };
        if already_exited || tab.dead || !tab.local {
            // Dropping detaches (the destructor sends it); a remote session
            // keeps running on its host, which is the point of the fleet.
            drop(tab);
        } else {
            // A local tab closing means "this shell is done" — the opposite
            // default of a remote one, and what every ordinary terminal does.
            tab.kill();
        }

        self.persist_tabs();
        if self.tabs.is_empty() {
            el.exit();
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

    /// Housekeeping after the active tab changed.
    fn after_activation(&mut self) {
        // Choosing a session is choosing to look at it: any full-pane screen
        // steps aside. Without this, clicking a sidebar row under the fleet
        // view activated the session *invisibly* — and the only way out of
        // the screen was knowing about Esc.
        self.screen = AppScreen::Terminal;
        // …and the chip must be in view: activation from the keyboard or the
        // picker can land on a tab the strip has scrolled past.
        self.strip_ensure_visible = true;
        // A drag cannot span a tab switch, and half a selection drag leaking
        // into another tab's grid would.
        self.mouse.release();
        let dims = self.current_dims();
        if let Some(tab) = self.tabs.active_mut() {
            // Background tabs are resized lazily: this is the moment a stale
            // one catches up (RemoteSession::resize also requests the
            // keyframe that makes the new shape true).
            if tab.sized != dims {
                tab.source().resize(dims.0, dims.1);
                tab.sized = dims;
            }
            tab.source().mark_dirty();
        }
        self.mark_chrome_dirty();
        self.persist_tabs();
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

    /// The active session's blocks as the header pass wants them: which
    /// viewport rows each header covers, plus its state and pre-formatted
    /// labels. One short terminal lock; plain data out.
    fn build_block_views(&self) -> Vec<crate::chrome::blocks::BlockView> {
        // A full-pane screen covers the grid; headers floating above the
        // fleet directory would be chrome over the wrong content. The
        // settings tab covers it the same way.
        if self.screen != AppScreen::Terminal || self.tabs.settings_active() {
            return Vec::new();
        }
        let Some(tab) = self.tabs.active() else { return Vec::new() };
        let pane_dead = match (&tab.split, tab.focus_right) {
            (Some(split), true) => split.dead,
            _ => tab.dead,
        };
        let term = tab.focused_source().terminal();
        let term = term.lock();
        // The alt screen is a separate grid whose ids restart at zero; a
        // primary-grid block would overlay whatever rows happen to collide.
        if term.in_alt_screen() {
            return Vec::new();
        }
        let grid = term.grid();
        let folded = self.folded_blocks.get(&tab.focused_addr());
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
                    zest_core::BlockState::Finished { exit_code: Some(c) } => format!("exit {c}"),
                    _ => String::new(),
                };
                Some(crate::chrome::blocks::BlockView {
                    id: b.id.0,
                    rows,
                    // A block still "running" in a session whose host went
                    // away is not running anywhere; the rail says so.
                    interrupted: running && pane_dead,
                    running: running && !pane_dead,
                    failed: b.failed(),
                    no_output,
                    command: b.command.clone(),
                    cwd: crate::status::shorten_home(&b.cwd),
                    duration,
                    exit_label,
                    running_label,
                    folded: is_folded,
                    foldable: block_actions::fold_range(b).is_some(),
                    folded_lines: b
                        .end_line
                        .map_or(0, |e| (e + 1).saturating_sub(out_line) as usize),
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

    fn redraw(&mut self) {
        let insets = self.insets();
        self.refresh_chrome();
        // Extracted under its own short lock, before the borrows below: the
        // views are plain data, and holding the terminal across atlas work
        // would stall the reader thread.
        let block_views = self.build_block_views();
        self.anim_spin = block_views.iter().any(|v| v.running);
        let anim = self.anim_phase();
        let caret_on = anim.caret_on;
        // Where the headers draw: the focused pane's body when split.
        let block_area = self.focused_view_rect();
        let fold_map: Option<Vec<usize>> = self.tabs.active().and_then(|t| {
            let folds = self.folded_blocks.get(&t.focused_addr()).filter(|s| !s.is_empty())?;
            let term = t.focused_source().terminal();
            let term = term.lock();
            block_actions::fold_row_map(&term, folds)
        });
        // What the window is painted with outside every viewport: the padding,
        // the gaps around the chrome bars, the split gutter. Taken from the
        // app's palette rather than a session's, because those pixels belong to
        // no session -- and computed here, before the borrows below.
        let backdrop = {
            let bg = self.palette.background;
            zest_render_wgpu::LinearRgba::from_srgb(bg.r, bg.g, bg.b, self.config.opacity)
        };
        let (Some(gpu), Some(fonts), Some(session), Some(window)) = (
            self.gpu.as_mut(),
            self.fonts.as_mut(),
            self.tabs.active_source(),
            self.window.as_ref(),
        ) else {
            return;
        };

        let metrics = fonts.cell_metrics();

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
            let area = block_area
                .unwrap_or_else(|| insets.grid_rect(gpu.config.width, gpu.config.height));
            let scale = window.scale_factor() as f32;
            let block_chrome = {
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
                    &block_views,
                    area,
                    metrics.cell_h as f32,
                    scale,
                    &self.chrome_colors,
                    self.chrome_hover,
                    anim.spin,
                    &mut measure,
                )
            };
            chrome.rects.extend_from_slice(&block_chrome.rects);
            for run in &block_chrome.texts {
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
            self.block_hits = block_chrome.hit;
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
            let split = self.tabs.active().and_then(|t| {
                t.split.as_ref().map(|p| (p.source(), t.focus_right))
            });
            match split {
                Some((right_source, focus_right)) => {
                    // Two panes, two grids, one build — the slice the
                    // renderer took from day one finally gets its second
                    // element (CONTRACTS, "cheap now" #3).
                    let scale =
                        self.window.as_ref().map_or(1.0, |w| w.scale_factor() as f32);
                    let (lf, rf) = crate::chrome::layout::pane_frames(area, scale);
                    let (lb, rb) = (
                        crate::chrome::layout::pane_body(lf, scale),
                        crate::chrome::layout::pane_body(rf, scale),
                    );
                    let active_tab =
                        self.tabs.active().expect("split implies an active tab");
                    let left_source = active_tab.source();
                    // Per pane, not per tab: a pane may later carry its own
                    // profile, so each viewport derives its own selection and
                    // opacity — today both read the tab's identity.
                    let left_identity = active_tab.identity.as_ref();
                    let right_identity = active_tab.identity.as_ref();
                    let term_l = left_source.terminal().lock();
                    let term_r = right_source.terminal().lock();
                    let preedit = self.ime.preedit().map(|p| {
                        zest_render_wgpu::Preedit { text: &p.text, cursor: p.cursor }
                    });
                    let left_focused = !focus_right;
                    let viewports = [
                        Viewport {
                            rect: lb,
                            grid: term_l.grid(),
                            palette: term_l.palette(),
                            scroll_px: 0.0,
                            focused: self.focused && left_focused,
                            opacity: pane_opacity(self.config.opacity, left_identity),
                            selection: term_l.selection(),
                            selection_bg: pane_selection_bg(self.selection_bg, left_identity),
                            preedit: if left_focused { preedit } else { None },
                            cursor_on: caret_on,
                            row_map: if left_focused { fold_map.as_deref() } else { None },
                        },
                        Viewport {
                            rect: rb,
                            grid: term_r.grid(),
                            palette: term_r.palette(),
                            scroll_px: 0.0,
                            focused: self.focused && focus_right,
                            opacity: pane_opacity(self.config.opacity, right_identity),
                            selection: term_r.selection(),
                            selection_bg: pane_selection_bg(self.selection_bg, right_identity),
                            preedit: if focus_right { preedit } else { None },
                            cursor_on: caret_on,
                            row_map: if focus_right { fold_map.as_deref() } else { None },
                        },
                    ];
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
                None => {
                    let identity = self.tabs.active().and_then(|t| t.identity.as_ref());
                    let term = session.terminal().lock();
                    self.scene.build(
                        &gpu.device,
                        &gpu.queue,
                        &mut gpu.renderer.atlas,
                        fonts,
                        metrics,
                        backdrop,
                        &[Viewport {
                            rect: area,
                            grid: term.grid(),
                            palette: term.palette(),
                            scroll_px: 0.0,
                            focused: self.focused,
                            opacity: pane_opacity(self.config.opacity, identity),
                            selection: term.selection(),
                            selection_bg: pane_selection_bg(self.selection_bg, identity),
                            preedit: self.ime.preedit().map(|p| {
                                zest_render_wgpu::Preedit { text: &p.text, cursor: p.cursor }
                            }),
                            cursor_on: caret_on,
                            row_map: fold_map.as_deref(),
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
                session.mark_dirty();
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
    }

    /// Re-read the config and apply whatever changed, at its own cost.
    ///
    /// Nothing here reaches for a restart: a class the app cannot yet act on is
    /// logged as such, so "this needs a restart" is a statement rather than an
    /// excuse. The layers are re-read from scratch rather than patched, because
    /// a removed key has to fall back through the cascade, and there is no way
    /// to know what it falls back *to* without redoing the merge.
    fn reload_config(&mut self) {
        let load = zest_config::load(&zest_config::Options {
            profile: self.profile.clone(),
            workspace_dir: std::env::current_dir().ok(),
            cli: Some(self.cli_layer.clone()),
            system_light: false,
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
                if let (Some(gpu), Some(w)) = (self.gpu.as_ref(), self.window.as_ref()) {
                    let size = w.inner_size();
                    let _ = gpu;
                    // The backdrop is a window attribute, not a surface one,
                    // but it shares this class because both change what is
                    // behind the pixels.
                    platform::set_backdrop(w, self.config.backdrop);
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

    /// Re-resolve the theme into the live palette.
    fn apply_theme(&mut self) {
        let theme = zest_theme::builtin::get(&self.config.theme)
            .unwrap_or_else(zest_theme::builtin::obsidian);
        let resolved = zest_theme::resolve(&theme);
        self.chrome_colors = ChromeColors::new(&theme.ui, &theme.effects, self.config.chrome_opacity);
        self.text_tuning = resolve_text_tuning(&self.config);
        self.mark_chrome_dirty();
        self.palette = to_core_palette(&resolved);
        self.selection_bg = zest_core::Rgb::new(
            resolved.selection_bg.r,
            resolved.selection_bg.g,
            resolved.selection_bg.b,
        );
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
            if let Some(split) = &tab.split {
                split.source().terminal().lock().set_palette(seed.clone());
            }
            tab.source().terminal().lock().set_palette(seed);
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
    /// grayscale, regardless of the setting. On Windows this costs almost
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
    fn antialias_for(config: &Config) -> zest_font::TextAntialias {
        if config.opacity < 1.0 {
            return zest_font::TextAntialias::Grayscale;
        }
        zest_font::TextAntialias::Subpixel
    }

    fn resize_surface(&mut self, width: u32, height: u32) {
        let insets = self.insets();
        let Some(gpu) = self.gpu.as_mut() else { return };
        if self.tabs.is_empty() || width == 0 || height == 0 {
            return;
        }

        // Clamp rather than fail. An oversized surface is a validation error
        // that would abort the process mid-drag; a clamped one just draws a
        // little short on an implausibly large window.
        let max = gpu.device.limits().max_texture_dimension_2d;
        let (width, height) = (width.min(max), height.min(max));

        gpu.config.width = width;
        gpu.config.height = height;
        gpu.surface.configure(&gpu.device, &gpu.config);
        gpu.renderer.resize(&gpu.device, width, height);

        // Every rectangle in the chrome depends on the window size — and on
        // macOS a fullscreen transition arrives as a resize, which is also
        // when the traffic-light inset changes.
        self.chrome_layout = None;
        self.chrome_dirty = true;

        if let Some(fonts) = self.fonts.as_ref() {
            let dims = insets.grid_dims(fonts.cell_metrics(), width, height);
            // Only the visible grid follows a drag live; background tabs
            // catch up on activation, so a resize costs one message rather
            // than one per tab per frame. A split tab resizes both panes.
            if self.tabs.active().is_some_and(|t| t.split.is_some()) {
                self.resize_split_panes();
            } else if let Some(tab) = self.tabs.active_mut() {
                tab.source().resize(dims.0, dims.1);
                tab.sized = dims;
            }
        }

        // Draw a frame at the new size, now.
        //
        // Nothing else will. A frame is only drawn when the terminal is dirty,
        // and a resize does not touch the grid -- so on a quiet session the
        // last frame stays on screen while the surface underneath it changes
        // shape, and the compositor stretches it to fit. Dragging an edge then
        // scales the text continuously instead of re-laying it out, which reads
        // as the font resizing with the window.
        //
        // Marking dirty rather than drawing inline keeps the single render path
        // intact: this is the same "something changed" signal the parser sends,
        // and it coalesces the same way when a drag produces a hundred of them.
        if let Some(session) = self.tabs.active_source() {
            session.mark_dirty();
        }
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }
}

impl ApplicationHandler<Wakeup> for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }

        let t0 = std::time::Instant::now();

        // Created HIDDEN, shown only once a real frame has been presented.
        //
        // A visible window shows the OS default background -- white on Windows --
        // for as long as startup takes, and startup is several hundred
        // milliseconds: adapter enumeration, device creation, shader
        // compilation, font resolution, then spawning a shell. Painting nothing
        // into a visible window is what produces the white flash; the fix is to
        // not be visible until there is something to show.
        let (win_w, win_h) = self.screenshot.as_ref().map_or((960.0, 600.0), |s| s.size);
        let attrs = Window::default_attributes()
            .with_title("zesterm")
            .with_transparent(self.config.opacity < 1.0)
            .with_visible(false)
            .with_inner_size(winit::dpi::LogicalSize::new(win_w, win_h));
        // Not borderless (ROADMAP, WS-C2: borderless costs traffic lights,
        // native fullscreen, Sequoia tiling and accessibility). A transparent
        // full-size titlebar keeps all of that, and the tab strip is what
        // fills the space — these are attribute flags, so the startup budget
        // pays nothing.
        #[cfg(target_os = "macos")]
        let attrs = {
            use winit::platform::macos::WindowAttributesExtMacOS;
            attrs
                .with_titlebar_transparent(true)
                .with_title_hidden(true)
                .with_fullsize_content_view(true)
        };
        // Borderless, so the OS caption stops sitting *above* our own tab
        // strip — two titlebars was the state of this window until now.
        //
        // Not a hand-rolled `WM_NCCALCSIZE`, which is what WS-A assumed this
        // would take: winit already returns 0 with the client area covering
        // the frame when decorations are off, and clamps the maximized rect to
        // the monitor work area, which is the thing that keeps the strip on
        // screen. `undecorated_shadow` is what keeps the drop shadow, the snap
        // animation and the rounded corners; it costs one black pixel row
        // along the top, per winit's own comment, and that reads as the
        // window's top border against every theme in the gallery.
        //
        // What it does cost: winit has no `WM_NCHITTEST` handler, so the
        // resize edges vanish with the frame. They come back out of the chrome
        // layout pass as `HitRegion::Resize`.
        #[cfg(windows)]
        let attrs = if self.config.custom_chrome {
            use winit::platform::windows::WindowAttributesExtWindows;
            attrs.with_decorations(false).with_undecorated_shadow(true)
        } else {
            attrs
        };
        let window = Arc::new(el.create_window(attrs).expect("create window"));

        // Show it NOW, painted by the OS in the theme colour.
        //
        // Bringing up the GPU is ~700ms of serial driver init with nothing to
        // overlap it with, so waiting for a presentable frame means the window
        // cannot appear in under three quarters of a second. It does not need
        // the GPU to be the right colour: a class background brush makes Windows
        // erase it on the first paint. The first real frame is the same colour,
        // so the handover is invisible.
        let bg = self.palette.background;
        platform::set_background_color(&window, bg.r, bg.g, bg.b);
        // Before the window is shown, so a backdrop never appears as a second
        // frame after an opaque one. Note the class brush above paints opaque
        // until the first GPU frame lands, so Mica is hidden for that ~700ms
        // and then appears — the same handover the brush exists to make
        // invisible, and not a bug however much it looks like one.
        platform::set_backdrop(&window, self.config.backdrop);
        // Screenshot mode never shows it. The frame goes to a texture we own,
        // so there is nothing to present and no reason to put a window on
        // someone's screen — which is what makes this usable while they are
        // working, and usable at all where there is no screen.
        match self.screenshot.as_ref() {
            Some(shot) => self.screenshot_at = Some(std::time::Instant::now() + shot.delay),
            None => window.set_visible(true),
        }
        let first_paint = t0.elapsed();

        // After the paint, deliberately. Without this no `Ime` event is ever
        // delivered -- and on macOS it is also what makes dead-key sequences
        // combine, so `Option+e` `e` produces `e` rather than `é`. It is off by
        // default in winit because a game does not want it; a terminal always
        // does. Nobody can type before the window exists, so it costs nothing to
        // keep it out of the measured path.
        window.set_ime_allowed(true);
        tracing::debug!(elapsed_ms = first_paint.as_millis(), "window shown");

        // The number the daemon work must not quietly ruin.
        //
        // Attaching to a daemon (ADR-007) puts a find-or-spawn and a socket
        // handshake into startup, and the tempting place to put them is right
        // here — before the window exists, so the session is ready when it does.
        // That would trade a hard-won 50ms for several hundred. This prints on
        // demand so the regression is a failing command rather than a vague
        // sense that it used to feel faster.
        if self.startup_probe {
            println!("first_paint_ms={}", first_paint.as_millis());
            if first_paint > std::time::Duration::from_millis(STARTUP_BUDGET_MS) {
                eprintln!(
                    "FAIL: first paint took {}ms, budget is {STARTUP_BUDGET_MS}ms.\n\
                     Something now runs before the window is shown. The window does \
                     not need a GPU, a font, a shell or a daemon to be the right \
                     colour -- see the comment above this check.",
                    first_paint.as_millis()
                );
                std::process::exit(1);
            }
            el.exit();
            return;
        }

        let scale = window.scale_factor() as f32;
        let typo = Typography { scale_factor: scale, ..self.config.typography };
        let mut fonts = Fonts::new(&self.config.font_families, typo).expect("no usable font");
        fonts.set_builtin_box_drawing(self.config.builtin_box_drawing);
        fonts.set_text_antialias(self.effective_antialias());
        fonts.set_hinting(self.config.text_hinting);
        fonts.set_grid_antialias(self.config.text_antialias);
        let metrics = fonts.cell_metrics();
        tracing::debug!(elapsed_ms = t0.elapsed().as_millis(), "fonts ready");

        // SPAWN THE SHELL BEFORE THE GPU.
        //
        // Bringing up the GPU is ~850ms and starting pwsh is ~400ms, and neither
        // needs the other. Doing them in sequence means the prompt only starts
        // arriving once the GPU is finished, so the first frame is empty and the
        // prompt appears later still. Started here, the shell is running while
        // the driver initializes and its prompt is usually already waiting by
        // the time there is anything to draw it with.
        //
        // The grid size comes from the window and the font metrics, both of
        // which are known now -- it never needed the GPU.
        let size = window.inner_size();
        let insets = self.insets_at(scale);
        let (cols, rows) = insets.grid_dims(metrics, size.width.max(1), size.height.max(1));

        let proxy = self.proxy.clone();
        let spec = self.build_spec();
        // TERM, COLORTERM and the TERM_PROGRAM pair come from
        // `zest_pty::terminal_env`, which `default_shell` already applied --
        // deliberately in one place, because a child that learns the wrong
        // terminal identity produces a monochrome prompt that looks like a
        // renderer bug.

        // Find or spawn this machine's daemon and attach to it, falling back to
        // an in-process pty. This slot -- after the window is visible and the
        // first paint is measured, before GPU init -- is the one ADR-007 names,
        // and nothing above line 649 may move below it.
        // Restore replaces adoption: reopen what this window was showing
        // last time. The synchronous slot fits exactly one attach, and only
        // a local one keeps the startup budget honest — everything else
        // arrives in the background.
        let restore = (!self.new_session
            && !self.no_daemon
            && self.attach_addr.is_none()
            && self.config.tabs.restore)
            .then(crate::tabs_state::load)
            .flatten();
        let (restore_active, restore_rest) = match restore {
            Some(saved) => {
                let mut tabs = saved.tabs;
                let sync = if tabs.get(saved.active).is_some_and(|t| t.local) {
                    Some(saved.active)
                } else {
                    // A remote active tab would put a network dial on the
                    // startup path; restore it in the background and lead
                    // with the first local one instead.
                    tabs.iter().position(|t| t.local)
                };
                match sync {
                    Some(i) => {
                        let lead = tabs.remove(i);
                        (Some(lead.addr), tabs)
                    }
                    None => (None, tabs),
                }
            }
            None => (None, Vec::new()),
        };

        let mut tab: Tab = match self.attach_to_daemon(cols, rows, &proxy, restore_active) {
            Some(tab) => tab,
            None => {
                self.next_placeholder += 1;
                let addr = crate::tabs::placeholder_addr(self.next_placeholder);
                let cell = Arc::new(parking_lot::Mutex::new(addr));
                let session = Session::spawn(
                    &spec,
                    PtySize::new(cols, rows),
                    self.config.scrollback,
                    wake_for(&proxy, cell, Arc::clone(&self.activity)),
                )
                .expect("spawn shell");
                tracing::debug!(
                    elapsed_ms = t0.elapsed().as_millis(),
                    cols,
                    rows,
                    "shell spawned in-process"
                );
                Tab::in_process(session, addr, (cols, rows))
            }
        };
        tab.source().terminal().lock().set_palette(self.palette.clone());

        // The surface is NOT sRGB (the resolve pass encodes), so the clear value
        // is written verbatim -- pass the theme background already in sRGB.
        let bg = self.palette.background;
        let clear = wgpu::Color {
            r: f64::from(bg.r) / 255.0,
            g: f64::from(bg.g) / 255.0,
            b: f64::from(bg.b) / 255.0,
            a: f64::from(self.config.opacity),
        };
        let gpu = pollster::block_on(init_gpu(
            &window,
            self.config.opacity < 1.0,
            clear,
            self.effective_antialias(),
        ));
        tracing::debug!(elapsed_ms = t0.elapsed().as_millis(), "gpu ready");
        // The renderer may have refused subpixel because the device cannot
        // blend per channel. The rasterizer follows it, never the config —
        // see `sync_antialias` for what going the other way costs.
        fonts.set_text_antialias(gpu.renderer.text_antialias());
        fonts.set_hinting(self.config.text_hinting);
        fonts.set_grid_antialias(self.config.text_antialias);

        // The surface may have landed on a slightly different size than the
        // window reported, so reconcile before the first frame.
        let (gpu_cols, gpu_rows) = insets.grid_dims(metrics, gpu.config.width, gpu.config.height);
        if (gpu_cols, gpu_rows) != (cols, rows) {
            tab.source().resize(gpu_cols, gpu_rows);
            tab.sized = (gpu_cols, gpu_rows);
        }

        self.fonts = Some(fonts);
        self.gpu = Some(gpu);
        // Everything downstream of this line works the same whether the shell
        // is in this process or on another machine, which is the property the
        // abstraction exists for.
        self.tabs.push(tab);
        // The rest of the remembered set, off the startup path. Parallel
        // workers, so one sleeping host cannot serialize the others behind
        // its timeout; arrival order may differ from the saved order, which
        // a background tab can afford.
        for saved in restore_rest {
            let route = if saved.local {
                HostRoute::LocalSocket(zest_daemon::default_socket_path())
            } else {
                match saved.dial_hint.clone() {
                    Some(addr) => HostRoute::Tcp(addr),
                    None => {
                        tracing::warn!(addr = %saved.addr, "no way to dial a remembered host; skipping");
                        continue;
                    }
                }
            };
            let expect = (!saved.local).then_some(saved.addr.host);
            self.spawn_tab_worker_pinned(route, Some(saved.addr), expect, false);
        }
        self.window = Some(window);

        // `--screen`: dispatched here — window and session exist, the first
        // real frame has not been built — so the frame a screenshot captures
        // (and the first one a user sees) is already the asked-for surface,
        // never the terminal with a screen flashed over it. Each arm is the
        // exact call the keyboard makes, so the flag can never show a state
        // the user could not have reached.
        match self.start_screen {
            Some(StartScreen::Fleet) => self.show_screen(AppScreen::Fleet),
            Some(StartScreen::Themes) => self.show_screen(AppScreen::Themes),
            Some(StartScreen::Settings) => self.open_settings_tab(),
            Some(StartScreen::Palette) => self.toggle_picker(),
            // Over the default screen, exactly as clicking the + would.
            Some(StartScreen::Launcher) => self.toggle_launcher(),
            Some(StartScreen::Profiles) => self.open_profiles_tab(),
            None => {}
        }

        // The window is already visible and painted with the theme background
        // (see init_gpu). Present the first real frame on top of it.
        //
        // Pre-warming the atlas here matters too: without it the first frame
        // that actually contains text pays for rasterizing the whole prompt,
        // which lands as a visible hitch right after the window appears.
        self.prewarm_atlas();
        self.redraw();

        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }

        tracing::info!(
            cols,
            rows,
            scale,
            startup_ms = t0.elapsed().as_millis(),
            origin = ?self.tabs.active_source().map_or(Origin::InProcess, |s| s.origin()),
            "zesterm ready"
        );

        // Last, and off the measured path: watching costs a thread and an
        // inotify/ReadDirectoryChanges handle, and none of it is needed to show
        // the first frame.
        self.watch_config();

        // The fleet view, also off the measured path. The window's own daemon
        // is synthesized into the listing from its signed Welcome (`addr.host`
        // of the tab it attached) — a default daemon is mDNS-invisible, so
        // discovery alone would omit the one host that certainly exists.
        let local = self.tabs.active().and_then(|tab| {
            if crate::tabs::is_placeholder(tab.addr) {
                return None;
            }
            let label = match tab.source().origin() {
                Origin::Daemon { host, .. } => host,
                Origin::InProcess => return None,
            };
            Some((tab.addr.host, label))
        });
        let fleet = crate::fleet::FleetModel::start(self.proxy.clone(), local);
        if let Some(route) = self.route.clone() {
            // One watching connection to the window's daemon keeps its
            // session list fresh through pushes.
            fleet.watch(move || route.dialer());
        }
        self.fleet = Some(fleet);
    }

    /// A wakeup from the parser thread.
    fn user_event(&mut self, el: &ActiveEventLoop, event: Wakeup) {
        match event {
            Wakeup::Redraw => {
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            Wakeup::Exited => el.exit(),
            // The link died, not the shell. The window stays open showing the
            // last state that was true -- closing it would throw away a session
            // that is still running in a daemon that does not care we went
            // away, which is the property ADR-007 exists to provide.
            Wakeup::Detached => {
                tracing::warn!("the daemon connection dropped; the session is still running");
                // Nothing to schedule here: `RemoteSession` supervises its own
                // link and is already dialling. The window goes on showing the
                // last state that was true, because the session still exists —
                // and the status bar says "reconnecting" until it is.
                self.link_down = true;
                self.mark_chrome_dirty();
                if let Some(s) = self.tabs.active_source() {
                    s.mark_dirty();
                }
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            // The in-place reconnect in `remote.rs` succeeded: same session, same
            // grid, and everything this window had accumulated is still there.
            // Nothing to rebuild — just repaint.
            // A worker finished opening a tab; adopt everything it parked.
            Wakeup::TabsChanged => {
                let mut fresh = self.pending_tabs.lock();
                let tabs: Vec<(Tab, bool)> = fresh.drain(..).collect();
                drop(fresh);
                let pushed = !tabs.is_empty();
                for (tab, focus) in tabs {
                    // Refused duplicates (#188) detach on drop — the shell
                    // stays on its host; the strip already activated the
                    // tab that holds it. This is also what heals a
                    // tabs.json that persisted a duplicate: the restore's
                    // second copy dies here on every launch.
                    if let Some(dup) = self.tabs.adopt(tab, focus) {
                        tracing::info!(addr = %dup.addr, "session already open; activating its tab");
                        drop(dup);
                    }
                }
                // Profile launches settling (issue #175): the connecting tab
                // is already in the strip, so this swaps its session in (or
                // marks it dead carrying the error) rather than pushing.
                let settled: Vec<_> = self.pending_launches.lock().drain(..).collect();
                let dims = self.current_dims();
                for (placeholder, outcome) in settled {
                    match outcome {
                        Ok(session) => match self.tabs.find_mut(placeholder) {
                            Some(tab) => {
                                tab.resolve_live(session, false);
                                // The window may have resized while the dial
                                // was in flight, and the lazy-resize path
                                // only catches *activation* — this tab is
                                // most likely already the active one.
                                if tab.sized != dims {
                                    tab.source().resize(dims.0, dims.1);
                                    tab.sized = dims;
                                }
                                tab.source().mark_dirty();
                            }
                            // Closed while dialling: dropping detaches, the
                            // shell stays on its host for the picker to find.
                            None => drop(session),
                        },
                        Err(error) => {
                            tracing::warn!(%placeholder, error, "profile launch failed");
                            if let Some(tab) = self.tabs.find_mut(placeholder) {
                                tab.resolve_failed(&error);
                            }
                        }
                    }
                }
                if pushed {
                    // A worker-opened tab takes the keyboard, so this is an
                    // activation. A settling launch is not: its tab was
                    // activated when it was pushed, and after_activation()
                    // here would yank a full-pane screen out from under the
                    // user because a background dial finished.
                    self.after_activation();
                    self.relayout_grid();
                } else {
                    self.mark_chrome_dirty();
                    if let Some(w) = self.window.as_ref() {
                        w.request_redraw();
                    }
                }
                self.persist_tabs();
            }
            // The picker's data moved. Consume the latch; the chrome decides
            // whether anything visible depends on it.
            Wakeup::FleetChanged => {
                if self.fleet.as_ref().is_some_and(|f| f.take_changed()) {
                    self.mark_chrome_dirty();
                }
            }
            // One tab's child exited. Close that tab — killing is moot, the
            // child is already gone — and the last tab closing closes the
            // window, which is exactly the old single-session behavior.
            Wakeup::TabExited(addr) => {
                // A split pane's shell ending collapses the pane, never the
                // tab it lived in.
                if let Some(tab) = self.tabs.find_split_owner(addr) {
                    tab.focus_right = false;
                    tab.split = None;
                    self.relayout_grid();
                    self.mark_chrome_dirty();
                    return;
                }
                self.close_tab(addr, true, el);
            }
            // A pinned tab's host answered and its session no longer exists.
            // The supervisor stopped rather than swapping in a fresh shell;
            // the tab stays put, marked ended, until the user closes it (a
            // recreate affordance arrives with the picker).
            Wakeup::SessionGone(addr) => {
                tracing::warn!(%addr, "the session ended on its host");
                if let Some(tab) = self.tabs.find_mut(addr) {
                    tab.dead = true;
                } else if let Some(tab) = self.tabs.find_split_owner(addr) {
                    // The pane stays put showing its last state, like a dead
                    // tab does — vanishing mid-glance is worse.
                    if let Some(split) = tab.split.as_mut() {
                        split.dead = true;
                    }
                }
                self.mark_chrome_dirty();
            }
            Wakeup::Reattached => {
                tracing::info!("the daemon connection is back");
                self.link_down = false;
                self.mark_chrome_dirty();
                if let Some(s) = self.tabs.active_source() {
                    s.mark_dirty();
                }
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }
            Wakeup::ConfigChanged => self.reload_config(),
        }
    }


    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            // Detach, never close. The session keeps running in the daemon and
            // can be picked up from another window or another device -- which
            // is the whole payoff of ADR-007, and is lost the moment closing a
            // window is allowed to mean "end the shell".
            //
            // Dropping the session is what sends the Detach: a destructor
            // covers every way this process can end, including the ones no
            // `CloseRequested` arm would see.
            WindowEvent::CloseRequested => self.request_close(el),

            WindowEvent::Resized(size) => self.resize_surface(size.width, size.height),

            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                // A DPI change invalidates every rasterized glyph, so bump the
                // atlas generation and recompute geometry. Doing this in two
                // steps would render a frame at the wrong size.
                if let Some(fonts) = self.fonts.as_mut() {
                    fonts.set_typography(Typography {
                        scale_factor: scale_factor as f32,
                        ..self.config.typography
                    });
                }
                if let Some(gpu) = self.gpu.as_mut() {
                    gpu.renderer.atlas.clear();
                }
                if let Some(w) = self.window.as_ref() {
                    let size = w.inner_size();
                    self.resize_surface(size.width, size.height);
                }
            }

            WindowEvent::Focused(focused) => {
                self.focused = focused;
                if let Some(s) = self.tabs.active_source() {
                    s.mark_dirty();
                }
                // The active tab's label dims with the window, like any
                // native titlebar.
                self.mark_chrome_dirty();
            }

            WindowEvent::Occluded(false) => {
                // Frames attempted while occluded were skipped with their
                // damage re-armed; this is the moment they were waiting for.
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }

            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),

            WindowEvent::Ime(ime) => self.on_ime(ime),

            WindowEvent::KeyboardInput { event, is_synthetic: false, .. } => {
                if event.state != ElementState::Pressed {
                    // A release exists only for a program that asked for kitty
                    // event types, and it must not walk the rest of this arm:
                    // every chord below fires on press, so a binding that also
                    // matched a release would run twice.
                    self.on_key_release(&event);
                    return;
                }
                // While an input method is composing, the keys are its own:
                // `Enter` picks a candidate rather than running a command, and
                // the arrows move through the candidate list. winit documents
                // that it withholds these events during a preedit, but that is a
                // promise made by four backends rather than a property of this
                // code, and the failure mode -- a half-composed word running as
                // a command -- is bad enough to check twice.
                if self.ime.composing() {
                    return;
                }

                // The open launcher owns the keyboard, like every overlay.
                // Chords resolve through the table first: ⌘1..⌘9 stay
                // ActivateTab (the plain digits below are the launcher's),
                // ⌘K switches to the picker, ⌘T spawns the default — the
                // menu yields to any chord rather than dying against it.
                if self.launcher.is_some() {
                    use winit::keyboard::{Key, NamedKey};
                    if let Some(binding) =
                        keymap::lookup(&event.logical_key, event.physical_key, self.modifiers)
                    {
                        self.launcher = None;
                        self.mark_chrome_dirty();
                        self.perform(binding.action, el);
                        return;
                    }
                    match &event.logical_key {
                        Key::Named(NamedKey::Escape) => {
                            self.launcher = None;
                            self.mark_chrome_dirty();
                        }
                        Key::Named(NamedKey::Enter) => {
                            // ⇧⏎ is the Run-on-another-host chord wherever
                            // the selection sits; plain ⏎ runs the selected
                            // row — the default row, until the user moves.
                            if self.modifiers.shift_key() {
                                self.run_launcher_action(crate::launcher::LauncherAction::RunOnHost);
                            } else {
                                let action = self
                                    .launcher
                                    .as_ref()
                                    .and_then(|l| l.actions.get(l.selected).cloned());
                                if let Some(action) = action {
                                    self.run_launcher_action(action);
                                }
                            }
                        }
                        Key::Named(NamedKey::ArrowDown) => {
                            if let Some(l) = self.launcher.as_mut() {
                                l.selected = crate::launcher::step(&l.actions, l.selected, true);
                            }
                            self.mark_chrome_dirty();
                        }
                        Key::Named(NamedKey::ArrowUp) => {
                            if let Some(l) = self.launcher.as_mut() {
                                l.selected = crate::launcher::step(&l.actions, l.selected, false);
                            }
                            self.mark_chrome_dirty();
                        }
                        Key::Character(c) => {
                            // Plain digits 1–9 run the Nth profile row (§1's
                            // ⌘N hints, minus the modifier the open menu
                            // makes unnecessary). Chords were resolved
                            // above, so anything with a desktop modifier is
                            // already gone.
                            let digit = (!self.modifiers.control_key()
                                && !key::belongs_to_desktop(self.modifiers))
                            .then(|| c.as_str())
                            .and_then(|s| s.parse::<u8>().ok())
                            .filter(|d| (1..=9).contains(d));
                            if let Some(d) = digit {
                                let action = self.launcher.as_ref().and_then(|l| {
                                    let i = crate::launcher::digit_action_index(&l.actions, d)?;
                                    l.actions.get(i).cloned()
                                });
                                if let Some(action) = action {
                                    self.run_launcher_action(action);
                                }
                            }
                        }
                        _ => {}
                    }
                    return;
                }

                // The open picker owns the keyboard entirely: a keystroke
                // meant for a list must never reach a shell.
                if self.picker.is_some() {
                    use winit::keyboard::{Key, NamedKey};
                    // The two chords that switch overlays rather than dying
                    // against the modal wall. Resolved through the binding
                    // table like everything else, because a character
                    // comparison here could only ever match one of the two
                    // spellings: ⌘, arrives as `Character(",")` while
                    // Ctrl+Shift+, arrives as `"<"`.
                    if let Some(binding) =
                        keymap::lookup(&event.logical_key, event.physical_key, self.modifiers)
                    {
                        match binding.action {
                            keymap::Action::ToggleFleetPicker => {
                                self.toggle_picker();
                                return;
                            }
                            keymap::Action::ToggleSettings => {
                                self.open_settings_tab();
                                return;
                            }
                            _ => {}
                        }
                    }
                    match &event.logical_key {
                        Key::Named(NamedKey::Escape) => {
                            self.picker = None;
                            self.mark_chrome_dirty();
                        }
                        Key::Named(NamedKey::ArrowDown) => {
                            if let Some(p) = self.picker.as_mut() {
                                // Skip group labels: the selection lands on
                                // things Enter can do, never on a heading.
                                let mut next = p.selected;
                                while next + 1 < p.actions.len() {
                                    next += 1;
                                    if !matches!(p.actions[next], PickerAction::None) {
                                        p.selected = next;
                                        break;
                                    }
                                }
                                p.scroll_to_selected = true;
                            }
                            self.mark_chrome_dirty();
                        }
                        Key::Named(NamedKey::ArrowUp) => {
                            if let Some(p) = self.picker.as_mut() {
                                let mut next = p.selected;
                                while next > 0 {
                                    next -= 1;
                                    if !matches!(p.actions[next], PickerAction::None) {
                                        p.selected = next;
                                        break;
                                    }
                                }
                                p.scroll_to_selected = true;
                            }
                            self.mark_chrome_dirty();
                        }
                        Key::Named(NamedKey::Enter) => {
                            let action = self
                                .picker
                                .as_ref()
                                .and_then(|p| p.actions.get(p.selected).cloned());
                            if let Some(action) = action {
                                let shift = self.modifiers.shift_key();
                                self.run_picker_action(action, el, shift);
                            }
                        }
                        Key::Named(NamedKey::Backspace) => {
                            if let Some(p) = self.picker.as_mut() {
                                p.filter.pop();
                                p.selected = 0;
                            }
                            self.mark_chrome_dirty();
                        }
                        // winit delivers the spacebar as Named(Space), not
                        // Character(" ") — without this arm no filter can
                        // ever contain a space. Found by typing one.
                        Key::Named(NamedKey::Space) => {
                            if let Some(p) = self.picker.as_mut() {
                                p.filter.push(' ');
                                p.selected = 0;
                            }
                            self.mark_chrome_dirty();
                        }
                        Key::Character(c) => {
                            // Anything carrying a desktop modifier is a chord,
                            // not text. The two that mean something here were
                            // handled above; the rest are swallowed rather
                            // than typed, or ⌘X would put an `x` in the filter.
                            if !key::belongs_to_desktop(self.modifiers)
                                && !self.modifiers.control_key()
                            {
                                if let Some(p) = self.picker.as_mut() {
                                    p.filter.push_str(c.as_str());
                                    p.selected = 0;
                                }
                                self.mark_chrome_dirty();
                            }
                        }
                        _ => {}
                    }
                    return;
                }

                // The value picker sits *over* the settings overlay and takes
                // the keyboard from it, so it is tested before both.
                if self.value_picker.is_some() {
                    use winit::keyboard::{Key, NamedKey};
                    let mut consumed = true;
                    match &event.logical_key {
                        Key::Named(NamedKey::Escape) => {
                            self.value_picker = None;
                            self.mark_chrome_dirty();
                        }
                        Key::Named(NamedKey::Enter) => self.accept_value_picker(),
                        Key::Named(NamedKey::ArrowDown) => {
                            if let Some(p) = self.value_picker.as_mut() {
                                let last = p.visible.len().saturating_sub(1);
                                p.selected = (p.selected + 1).min(last);
                                p.scroll_to_selected = true;
                                self.mark_chrome_dirty();
                            }
                        }
                        Key::Named(NamedKey::ArrowUp) => {
                            if let Some(p) = self.value_picker.as_mut() {
                                p.selected = p.selected.saturating_sub(1);
                                p.scroll_to_selected = true;
                                self.mark_chrome_dirty();
                            }
                        }
                        Key::Named(NamedKey::PageDown) => {
                            if let Some(p) = self.value_picker.as_mut() {
                                p.scroll += 300.0;
                                self.mark_chrome_dirty();
                            }
                        }
                        Key::Named(NamedKey::PageUp) => {
                            if let Some(p) = self.value_picker.as_mut() {
                                p.scroll -= 300.0;
                                self.mark_chrome_dirty();
                            }
                        }
                        Key::Named(NamedKey::Backspace) => {
                            if let Some(p) = self.value_picker.as_mut() {
                                p.filter.pop();
                                p.selected = 0;
                                p.scroll_to_selected = true;
                                self.mark_chrome_dirty();
                            }
                        }
                        // Spacebar arrives as Named(Space), not as a character;
                        // family names are full of them.
                        Key::Named(NamedKey::Space) => {
                            if let Some(p) = self.value_picker.as_mut() {
                                p.filter.push(' ');
                                p.selected = 0;
                                p.scroll_to_selected = true;
                                self.mark_chrome_dirty();
                            }
                        }
                        Key::Character(c) => {
                            if let Some(p) = self.value_picker.as_mut() {
                                p.filter.push_str(c);
                                p.selected = 0;
                                p.scroll_to_selected = true;
                                self.mark_chrome_dirty();
                            }
                        }
                        _ => consumed = false,
                    }
                    if consumed {
                        return;
                    }
                }

                // The open command palette likewise owns the keyboard. It and
                // the picker are mutually exclusive (the toggles enforce it),
                // so the order of these blocks carries no meaning.
                if self.palette_ui.is_some() {
                    use winit::keyboard::{Key, NamedKey};
                    match &event.logical_key {
                        Key::Named(NamedKey::Escape) => {
                            self.palette_ui = None;
                        }
                        Key::Named(NamedKey::Enter) => {
                            self.run_palette_selection(el);
                        }
                        Key::Named(NamedKey::ArrowDown) => {
                            if let Some(p) = self.palette_ui.as_mut() {
                                p.selected = keymap::step_runnable(&p.actions, p.selected, true);
                                p.scroll_to_selected = true;
                            }
                        }
                        Key::Named(NamedKey::ArrowUp) => {
                            if let Some(p) = self.palette_ui.as_mut() {
                                p.selected = keymap::step_runnable(&p.actions, p.selected, false);
                                p.scroll_to_selected = true;
                            }
                        }
                        Key::Named(NamedKey::PageDown) => {
                            if let Some(p) = self.palette_ui.as_mut() {
                                p.scroll += 300.0;
                            }
                        }
                        Key::Named(NamedKey::PageUp) => {
                            if let Some(p) = self.palette_ui.as_mut() {
                                p.scroll -= 300.0;
                            }
                        }
                        Key::Named(NamedKey::Backspace) => {
                            if let Some(p) = self.palette_ui.as_mut() {
                                p.filter.pop();
                                p.selected = 0;
                                p.scroll_to_selected = true;
                            }
                        }
                        // Spacebar arrives as Named(Space); see the picker.
                        Key::Named(NamedKey::Space) => {
                            if let Some(p) = self.palette_ui.as_mut() {
                                p.filter.push(' ');
                                p.selected = 0;
                                p.scroll_to_selected = true;
                            }
                        }
                        Key::Character(c) => {
                            // The opening chord closes too, aliases included —
                            // resolved through the table so ⌘/, ⌘? and ⌘⇧P
                            // agree — and the sibling overlays' chords switch.
                            match keymap::lookup(&event.logical_key, event.physical_key, self.modifiers)
                                .map(|b| b.action)
                            {
                                Some(keymap::Action::TogglePalette) => self.palette_ui = None,
                                Some(keymap::Action::ToggleSettings) => self.open_settings_tab(),
                                Some(keymap::Action::ToggleFleetPicker) => self.toggle_picker(),
                                _ => {
                                    if !self.modifiers.control_key()
                                        && !key::belongs_to_desktop(self.modifiers)
                                    {
                                        if let Some(p) = self.palette_ui.as_mut() {
                                            p.filter.push_str(c.as_str());
                                            p.selected = 0;
                                            p.scroll_to_selected = true;
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                    self.mark_chrome_dirty();
                    return;
                }

                // And the Settings tab, while it holds the grid area — the
                // full-pane screens (Esc returns) are checked below, so a
                // fleet view open over the tab keeps its own keys.
                if self.settings_tab_active() && self.screen == AppScreen::Terminal {
                    use winit::keyboard::{Key, NamedKey};

                    // The open dropdown menu owns the keys before everything.
                    if self.settings_ui.as_ref().is_some_and(|ui| ui.menu.is_some()) {
                        let options = self
                            .settings_ui
                            .as_ref()
                            .and_then(|ui| {
                                let (row, _) = ui.menu?;
                                let i = match ui.actions.get(row) {
                                    Some(crate::settings_ui::RowAction::Field(i)) => *i,
                                    _ => return None,
                                };
                                ui.fields.get(i).map(|f| f.variants.len())
                            })
                            .unwrap_or(0);
                        match &event.logical_key {
                            Key::Named(NamedKey::Escape) => {
                                if let Some(ui) = self.settings_ui.as_mut() {
                                    ui.menu = None;
                                }
                            }
                            Key::Named(NamedKey::Enter) => {
                                let menu = self.settings_ui.as_ref().and_then(|ui| ui.menu);
                                if let Some(ui) = self.settings_ui.as_mut() {
                                    ui.menu = None;
                                }
                                if let Some((row, sel)) = menu {
                                    self.apply_variant(row, sel);
                                }
                            }
                            Key::Named(NamedKey::ArrowDown) => {
                                if let Some((_, sel)) =
                                    self.settings_ui.as_mut().and_then(|ui| ui.menu.as_mut())
                                {
                                    *sel = (*sel + 1).min(options.saturating_sub(1));
                                }
                            }
                            Key::Named(NamedKey::ArrowUp) => {
                                if let Some((_, sel)) =
                                    self.settings_ui.as_mut().and_then(|ui| ui.menu.as_mut())
                                {
                                    *sel = sel.saturating_sub(1);
                                }
                            }
                            _ => {}
                        }
                        self.mark_chrome_dirty();
                        return;
                    }

                    // A typed edit owns the keys before the list does — while
                    // a buffer is open, a digit is a digit, never a filter.
                    if self.settings_ui.as_ref().is_some_and(|ui| ui.editing.is_some()) {
                        match &event.logical_key {
                            Key::Named(NamedKey::Escape) => {
                                if let Some(ui) = self.settings_ui.as_mut() {
                                    ui.editing = None;
                                }
                            }
                            Key::Named(NamedKey::Enter) => {
                                let edit = self.settings_ui.as_ref().and_then(|ui| {
                                    let edit = ui.editing.as_ref()?;
                                    Some((edit.field_idx, edit.buffer.clone(), edit.append))
                                });
                                match edit {
                                    // The add buffers append to the list;
                                    // everything else replaces the value.
                                    Some((idx, buffer, true)) => {
                                        if self.commit_list_append(idx, &buffer) {
                                            if let Some(ui) = self.settings_ui.as_mut() {
                                                ui.editing = None;
                                            }
                                        } else if let Some(edit) = self
                                            .settings_ui
                                            .as_mut()
                                            .and_then(|ui| ui.editing.as_mut())
                                        {
                                            edit.error = true;
                                        }
                                    }
                                    Some((idx, buffer, false)) => {
                                        let parsed = self
                                            .settings_ui
                                            .as_ref()
                                            .and_then(|ui| ui.fields.get(idx))
                                            .and_then(|field| {
                                                crate::settings_ui::parse_input(field, &buffer)
                                            });
                                        match parsed {
                                            Some(value) => {
                                                if let Some(ui) = self.settings_ui.as_mut() {
                                                    ui.editing = None;
                                                }
                                                self.apply_edit(idx, value);
                                            }
                                            // A failed parse keeps the buffer
                                            // and marks it: silently dropping
                                            // typed input reads as a broken
                                            // Enter key.
                                            None => {
                                                if let Some(edit) = self
                                                    .settings_ui
                                                    .as_mut()
                                                    .and_then(|ui| ui.editing.as_mut())
                                                {
                                                    edit.error = true;
                                                }
                                            }
                                        }
                                    }
                                    None => {}
                                }
                            }
                            Key::Named(NamedKey::Backspace) => {
                                if let Some(edit) =
                                    self.settings_ui.as_mut().and_then(|ui| ui.editing.as_mut())
                                {
                                    edit.buffer.pop();
                                    edit.error = false;
                                }
                            }
                            // Spacebar arrives as Named(Space); a path or
                            // command with a space must be typeable.
                            Key::Named(NamedKey::Space) => {
                                if let Some(edit) =
                                    self.settings_ui.as_mut().and_then(|ui| ui.editing.as_mut())
                                {
                                    edit.buffer.push(' ');
                                    edit.error = false;
                                }
                            }
                            Key::Character(c) => {
                                if !self.modifiers.control_key()
                                    && !key::belongs_to_desktop(self.modifiers)
                                {
                                    if let Some(edit) = self
                                        .settings_ui
                                        .as_mut()
                                        .and_then(|ui| ui.editing.as_mut())
                                    {
                                        edit.buffer.push_str(c.as_str());
                                        edit.error = false;
                                    }
                                }
                            }
                            _ => {}
                        }
                        self.mark_chrome_dirty();
                        return;
                    }

                    match &event.logical_key {
                        Key::Named(NamedKey::Enter) => {
                            self.activate_selected_setting();
                        }
                        Key::Named(NamedKey::ArrowRight) => {
                            self.adjust_selected_setting(1);
                        }
                        Key::Named(NamedKey::ArrowLeft) => {
                            self.adjust_selected_setting(-1);
                        }
                        Key::Named(NamedKey::Escape) => {
                            // Layered: edit and menu were handled above, so
                            // here a filter clears first, and a second Esc
                            // CLOSES THE TAB — closing it is closing a tab.
                            let filtered = self
                                .settings_ui
                                .as_ref()
                                .is_some_and(|ui| !ui.filter.is_empty());
                            if filtered {
                                if let Some(ui) = self.settings_ui.as_mut() {
                                    ui.filter.clear();
                                    ui.selected = 0;
                                    ui.scroll_to_selected = true;
                                }
                            } else {
                                self.close_settings_tab();
                            }
                        }
                        Key::Named(NamedKey::ArrowDown) => {
                            if let Some(ui) = self.settings_ui.as_mut() {
                                ui.selected = crate::settings_ui::step_selection(
                                    &ui.actions,
                                    ui.selected,
                                    true,
                                );
                                ui.scroll_to_selected = true;
                            }
                        }
                        Key::Named(NamedKey::ArrowUp) => {
                            if let Some(ui) = self.settings_ui.as_mut() {
                                ui.selected = crate::settings_ui::step_selection(
                                    &ui.actions,
                                    ui.selected,
                                    false,
                                );
                                ui.scroll_to_selected = true;
                            }
                        }
                        Key::Named(NamedKey::Backspace) => {
                            if let Some(ui) = self.settings_ui.as_mut() {
                                ui.filter.pop();
                                ui.selected = 0;
                                ui.scroll_to_selected = true;
                            }
                        }
                        // Spacebar arrives as Named(Space); see the picker.
                        Key::Named(NamedKey::Space) => {
                            if let Some(ui) = self.settings_ui.as_mut() {
                                ui.filter.push(' ');
                                ui.selected = 0;
                                ui.scroll_to_selected = true;
                            }
                        }
                        Key::Character(c) => {
                            match keymap::lookup(&event.logical_key, event.physical_key, self.modifiers)
                                .map(|b| b.action)
                            {
                                // ⌘, on the active settings tab is already
                                // where it goes; swallow rather than reopen.
                                Some(keymap::Action::ToggleSettings) => {}
                                // A tab is not a modal: the tab-management
                                // chords keep working over it — including
                                // ⌘W, which is how this tab closes (§11).
                                Some(
                                    action @ (keymap::Action::ToggleFleetPicker
                                    | keymap::Action::TogglePalette
                                    | keymap::Action::CloseTab
                                    | keymap::Action::NewTab
                                    | keymap::Action::ToggleTabLayout
                                    | keymap::Action::ActivateTab(_)
                                    | keymap::Action::ActivateLastTab
                                    | keymap::Action::PrevTab
                                    | keymap::Action::NextTab),
                                ) => self.perform(action, el),
                                _ => {
                                    // '/' focuses the filter (§11) — which is
                                    // where every other character already
                                    // goes, so focusing is swallowing it.
                                    if c.as_str() != "/"
                                        && !self.modifiers.control_key()
                                        && !key::belongs_to_desktop(self.modifiers)
                                    {
                                        if let Some(ui) = self.settings_ui.as_mut() {
                                            ui.filter.push_str(c.as_str());
                                            ui.selected = 0;
                                            ui.scroll_to_selected = true;
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                    self.mark_chrome_dirty();
                    return;
                }

                // The Profiles editor, while its pane holds the grid area —
                // the Settings tab's keyboard discipline, on §12's surface.
                if self.profiles_tab_active() {
                    use winit::keyboard::{Key, NamedKey};

                    // The open dropdown menu owns the keys before everything.
                    if self.profiles_ui.as_ref().is_some_and(|ui| ui.menu.is_some()) {
                        let options = self
                            .profiles_ui
                            .as_ref()
                            .and_then(|ui| {
                                let (row, _) = ui.menu?;
                                let i = match ui.actions.get(row) {
                                    Some(crate::settings_ui::RowAction::Field(i)) => *i,
                                    _ => return None,
                                };
                                ui.fields.get(i).map(|f| f.variants.len())
                            })
                            .unwrap_or(0);
                        match &event.logical_key {
                            Key::Named(NamedKey::Escape) => {
                                if let Some(ui) = self.profiles_ui.as_mut() {
                                    ui.menu = None;
                                }
                            }
                            Key::Named(NamedKey::Enter) => {
                                let menu = self.profiles_ui.as_ref().and_then(|ui| ui.menu);
                                if let Some(ui) = self.profiles_ui.as_mut() {
                                    ui.menu = None;
                                }
                                if let Some((row, sel)) = menu {
                                    self.profiles_apply_variant(row, sel);
                                }
                            }
                            Key::Named(NamedKey::ArrowDown) => {
                                if let Some((_, sel)) =
                                    self.profiles_ui.as_mut().and_then(|ui| ui.menu.as_mut())
                                {
                                    *sel = (*sel + 1).min(options.saturating_sub(1));
                                }
                            }
                            Key::Named(NamedKey::ArrowUp) => {
                                if let Some((_, sel)) =
                                    self.profiles_ui.as_mut().and_then(|ui| ui.menu.as_mut())
                                {
                                    *sel = sel.saturating_sub(1);
                                }
                            }
                            _ => {}
                        }
                        self.mark_chrome_dirty();
                        return;
                    }

                    // A typed edit owns the keys before the list does.
                    if self.profiles_ui.as_ref().is_some_and(|ui| ui.editing.is_some()) {
                        match &event.logical_key {
                            Key::Named(NamedKey::Escape) => {
                                if let Some(ui) = self.profiles_ui.as_mut() {
                                    ui.editing = None;
                                }
                            }
                            Key::Named(NamedKey::Enter) => {
                                let edit = self.profiles_ui.as_ref().and_then(|ui| {
                                    let edit = ui.editing.as_ref()?;
                                    Some((edit.field_idx, edit.buffer.clone()))
                                });
                                if let Some((idx, buffer)) = edit {
                                    let parsed = self
                                        .profiles_ui
                                        .as_ref()
                                        .and_then(|ui| ui.fields.get(idx))
                                        .and_then(|field| {
                                            crate::settings_ui::parse_input(field, &buffer)
                                        });
                                    match parsed {
                                        Some(value) => {
                                            if let Some(ui) = self.profiles_ui.as_mut() {
                                                ui.editing = None;
                                            }
                                            // An emptied string is the file's
                                            // spelling of "unset" — it parses,
                                            // writes, and resolution falls
                                            // back through Defaults for it
                                            // (profiles.rs's contract).
                                            self.profiles_apply_edit(idx, value);
                                        }
                                        None => {
                                            if let Some(edit) = self
                                                .profiles_ui
                                                .as_mut()
                                                .and_then(|ui| ui.editing.as_mut())
                                            {
                                                edit.error = true;
                                            }
                                        }
                                    }
                                }
                            }
                            Key::Named(NamedKey::Backspace) => {
                                if let Some(edit) =
                                    self.profiles_ui.as_mut().and_then(|ui| ui.editing.as_mut())
                                {
                                    edit.buffer.pop();
                                    edit.error = false;
                                }
                            }
                            Key::Named(NamedKey::Space) => {
                                if let Some(edit) =
                                    self.profiles_ui.as_mut().and_then(|ui| ui.editing.as_mut())
                                {
                                    edit.buffer.push(' ');
                                    edit.error = false;
                                }
                            }
                            Key::Character(c) => {
                                if !self.modifiers.control_key()
                                    && !key::belongs_to_desktop(self.modifiers)
                                {
                                    if let Some(edit) = self
                                        .profiles_ui
                                        .as_mut()
                                        .and_then(|ui| ui.editing.as_mut())
                                    {
                                        edit.buffer.push_str(c.as_str());
                                        edit.error = false;
                                    }
                                }
                            }
                            _ => {}
                        }
                        self.mark_chrome_dirty();
                        return;
                    }

                    match &event.logical_key {
                        Key::Named(NamedKey::Enter) => {
                            self.profiles_activate_selected();
                        }
                        Key::Named(NamedKey::ArrowRight) => {
                            self.profiles_adjust(1);
                        }
                        Key::Named(NamedKey::ArrowLeft) => {
                            self.profiles_adjust(-1);
                        }
                        Key::Named(NamedKey::Escape) => {
                            // Layered: edit and menu were handled above, so a
                            // filter clears first, and a second Esc CLOSES
                            // THE TAB — closing it is closing a tab (§12
                            // takes §11's rule whole).
                            let filtered = self
                                .profiles_ui
                                .as_ref()
                                .is_some_and(|ui| !ui.filter.is_empty());
                            if filtered {
                                if let Some(ui) = self.profiles_ui.as_mut() {
                                    ui.filter.clear();
                                    ui.selected = 0;
                                    ui.scroll_to_selected = true;
                                }
                            } else {
                                self.close_profiles_tab();
                            }
                        }
                        Key::Named(NamedKey::ArrowDown) => {
                            if let Some(ui) = self.profiles_ui.as_mut() {
                                ui.selected = crate::settings_ui::step_selection(
                                    &ui.actions,
                                    ui.selected,
                                    true,
                                );
                                ui.scroll_to_selected = true;
                            }
                        }
                        Key::Named(NamedKey::ArrowUp) => {
                            if let Some(ui) = self.profiles_ui.as_mut() {
                                ui.selected = crate::settings_ui::step_selection(
                                    &ui.actions,
                                    ui.selected,
                                    false,
                                );
                                ui.scroll_to_selected = true;
                            }
                        }
                        Key::Named(NamedKey::Backspace) => {
                            if let Some(ui) = self.profiles_ui.as_mut() {
                                ui.filter.pop();
                                ui.selected = 0;
                                ui.scroll_to_selected = true;
                            }
                        }
                        Key::Named(NamedKey::Space) => {
                            if let Some(ui) = self.profiles_ui.as_mut() {
                                ui.filter.push(' ');
                                ui.selected = 0;
                                ui.scroll_to_selected = true;
                            }
                        }
                        Key::Character(c) => {
                            match keymap::lookup(&event.logical_key, event.physical_key, self.modifiers)
                                .map(|b| b.action)
                            {
                                // ⌘⇧, on the active Profiles tab is already
                                // where it goes; swallow rather than reopen.
                                Some(keymap::Action::OpenProfiles) => {}
                                // A tab is not a modal: the tab-management
                                // chords keep working over it — ⌘W closes it
                                // via perform's Profiles arm.
                                Some(
                                    action @ (keymap::Action::ToggleFleetPicker
                                    | keymap::Action::TogglePalette
                                    | keymap::Action::ToggleSettings
                                    | keymap::Action::CloseTab
                                    | keymap::Action::NewTab
                                    | keymap::Action::ToggleTabLayout
                                    | keymap::Action::ActivateTab(_)
                                    | keymap::Action::ActivateLastTab
                                    | keymap::Action::PrevTab
                                    | keymap::Action::NextTab),
                                ) => self.perform(action, el),
                                _ => {
                                    // A LEADING digit jumps the rail (its
                                    // drawn 1–9 hints); once a filter is
                                    // live, digits filter like any other
                                    // character — the two meanings are
                                    // separated by the filter's emptiness,
                                    // not by guesswork.
                                    let digit = c
                                        .as_str()
                                        .parse::<usize>()
                                        .ok()
                                        .filter(|d| (1..=9).contains(d));
                                    let filter_empty = self
                                        .profiles_ui
                                        .as_ref()
                                        .is_some_and(|ui| ui.filter.is_empty());
                                    if let (Some(d), true) = (digit, filter_empty) {
                                        if !self.modifiers.control_key()
                                            && !key::belongs_to_desktop(self.modifiers)
                                        {
                                            self.profiles_select_rail(d);
                                        }
                                    } else if c.as_str() != "/"
                                        && !self.modifiers.control_key()
                                        && !key::belongs_to_desktop(self.modifiers)
                                    {
                                        // '/' focuses the filter (§11) —
                                        // where every other character
                                        // already goes.
                                        if let Some(ui) = self.profiles_ui.as_mut() {
                                            ui.filter.push_str(c.as_str());
                                            ui.selected = 0;
                                            ui.scroll_to_selected = true;
                                        }
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                    self.mark_chrome_dirty();
                    return;
                }

                // A full-pane screen owns the keyboard the way an overlay
                // does, minus the filter: Esc returns to the grid, chords
                // still work, and nothing falls through to the shell — the
                // user is not looking at it.
                if self.screen != AppScreen::Terminal {
                    use winit::keyboard::{Key, NamedKey};
                    if matches!(&event.logical_key, Key::Named(NamedKey::Escape)) {
                        self.show_screen(AppScreen::Terminal);
                        return;
                    }
                    if let Some(binding) = keymap::lookup(&event.logical_key, event.physical_key, self.modifiers) {
                        self.perform(binding.action, el);
                    }
                    return;
                }

                // Every global chord resolves through the one binding table.
                // Adding a chord as an if-block here instead of a BINDINGS
                // row is the bug the command palette exists to prevent: the
                // palette renders and runs from BINDINGS, so an unlisted
                // chord is an undiscoverable one.
                if let Some(binding) = keymap::lookup(&event.logical_key, event.physical_key, self.modifiers) {
                    let swallow = match binding.when {
                        keymap::When::Always => true,
                        // In the alternate screen the chord falls through to
                        // the encoder rather than being swallowed: `less` and
                        // `vim` page themselves and are owed the bytes.
                        keymap::When::NotAltScreen => !self.tabs.active_source().is_some_and(|s| {
                            s.terminal().lock().modes().contains(zest_core::Modes::ALT_SCREEN)
                        }),
                    };
                    if swallow {
                        self.perform(binding.action, el);
                        return;
                    }
                }

                let Some(session) = self.tabs.active_source() else { return };
                let modes = session.terminal().lock().modes();

                if let Some(bytes) = key::encode(&event, self.modifiers, modes) {
                    // Written synchronously, before anything else. Deferring
                    // input to the next frame adds a whole frame of latency for
                    // nothing.
                    session.write(bytes);
                    let mut term = session.terminal().lock();
                    // Typing scrolls back to the bottom, which is what every
                    // terminal does and what users expect.
                    term.scroll_to_bottom();
                    // ...and clears the selection, which is now stale.
                    term.set_selection(None);
                }
            }

            WindowEvent::CursorMoved { position, .. } => {
                self.pointer_pos = (position.x, position.y);

                // A held slider follows the pointer before anything else —
                // including off the track, which is how every slider works.
                if let Some(row) = self.slider_drag {
                    self.apply_slider_at(row, position.x as f32);
                    return;
                }

                // A held font row reorders as it crosses its siblings: order
                // IS the setting, and each crossing writes through the same
                // path as every other edit (§11).
                if let Some((row, from)) = self.settings_ui.as_ref().and_then(|ui| ui.list_drag)
                {
                    if let Some(HitRegion::SettingsListItem(r, to)) =
                        self.chrome_hit(position.x, position.y)
                    {
                        if r == row && to != from {
                            self.reorder_list_item(row, from, to);
                            if let Some(ui) = self.settings_ui.as_mut() {
                                ui.list_drag = Some((row, to));
                            }
                        }
                    }
                    return;
                }

                // The chrome sees the pointer first — unless a grid drag is in
                // progress, which keeps the grid: a selection that wanders
                // into the strip must not die there.
                if !self.mouse.is_dragging() {
                    let over = self.chrome_hit(position.x, position.y);
                    if over != self.chrome_hover {
                        self.chrome_hover = over;
                        self.mark_chrome_dirty();
                    }
                    // The resize edges are the one hit region with no visible
                    // affordance, so the cursor is the whole of it: without
                    // this the window is resizable and looks like it is not.
                    // Set only on change — this runs per mouse-move, and a
                    // Win32 call per move is not free.
                    let want = match over {
                        Some(HitRegion::Resize(edge)) => edge.into(),
                        _ => winit::window::CursorIcon::Default,
                    };
                    if want != self.cursor {
                        self.cursor = want;
                        if let Some(w) = self.window.as_ref() {
                            w.set_cursor(want);
                        }
                    }
                    if over.is_some() {
                        return;
                    }
                }

                let cell = self.cell_at(position.x, position.y);
                let moved = cell != self.pointer_cell;
                self.pointer_cell = cell;

                // Programs that enabled 1002 or 1003 want movement too -- that
                // is how a tmux pane drag or an htop hover works. Only on a cell
                // change: reporting every pixel would flood the pty.
                if moved && self.forward_motion(cell.0, cell.1) {
                    return;
                }

                if !self.mouse.is_dragging() {
                    return;
                }
                let Some(session) = self.tabs.active_source() else { return };
                let mut term = session.terminal().lock();
                if let (Some(mut sel), Some(pos)) =
                    (term.selection(), self.visual_abs_pos(&term, cell.0, cell.1))
                {
                    // Word mode extends by whole words, so dragging after a
                    // double-click grows the selection a word at a time rather
                    // than reverting to characters.
                    sel.head = if sel.mode == zest_core::SelectionMode::Word {
                        term.word_at(pos).1
                    } else {
                        pos
                    };
                    term.set_selection(Some(sel));
                    drop(term);
                    session.mark_dirty();
                    if let Some(w) = self.window.as_ref() {
                        w.request_redraw();
                    }
                }
            }

            WindowEvent::MouseInput { state, button, .. } => {
                // A slider or list drag ends when any button releases,
                // wherever the pointer wandered to in the meantime.
                if state == ElementState::Released {
                    let slider = self.slider_drag.take().is_some();
                    let list = self
                        .settings_ui
                        .as_mut()
                        .and_then(|ui| ui.list_drag.take())
                        .is_some();
                    if slider || list {
                        return;
                    }
                }
                // Chrome clicks never reach the grid. A drag in progress keeps
                // the grid for symmetry with CursorMoved.
                if !self.mouse.is_dragging() {
                    if let Some(region) = self.chrome_hit(self.pointer_pos.0, self.pointer_pos.1) {
                        self.on_chrome_click(region, button, state, el);
                        return;
                    }
                }

                let Some(session) = self.tabs.active_source() else { return };
                let (row, col) = self.pointer_cell;

                // When the program asked for mouse reporting, the mouse belongs
                // to it -- vim, htop, tmux and every TUI expect their clicks.
                // Selecting instead would make them appear broken.
                //
                // Shift is the escape hatch every terminal implements: hold it
                // to select text over a mouse-aware program anyway.
                if self.forward_mouse(button, state, row, col) {
                    return;
                }

                // The desktop chord plus a click copies *that* block's output,
                // wherever it is in scrollback -- which is the thing a keyboard
                // shortcut cannot express, since it can only mean "the last
                // one". No chrome and no hit map involved: a click in the grid
                // already resolves to a line, and a line already knows its
                // block.
                if button == MouseButton::Left
                    && state == ElementState::Pressed
                    && key::is_clipboard_chord(self.modifiers)
                {
                    self.copy_block_output_at(row);
                    return;
                }

                match (button, state) {
                    (MouseButton::Left, ElementState::Pressed) => {
                        let mode = self.mouse.press(row, col);
                        let mut term = session.terminal().lock();
                        if let Some(pos) = self.visual_abs_pos(&term, row, col) {
                            let sel = select::begin(&term, pos, mode, self.modifiers.alt_key());
                            term.set_selection(Some(sel));
                        }
                        drop(term);
                        session.mark_dirty();
                    }
                    (MouseButton::Left, ElementState::Released) => {
                        self.mouse.release();
                        // Copy-on-select is deliberately NOT the default: it
                        // silently replaces the clipboard, which surprises people
                        // who selected only to read.
                    }
                    // Middle-click pastes the selection, as X11 users expect.
                    (MouseButton::Middle, ElementState::Pressed) => self.paste(),
                    (MouseButton::Right, ElementState::Pressed) => {
                        // Right-click copies when there is a selection and pastes
                        // otherwise -- the PowerShell/conhost convention Windows
                        // users already have in their fingers.
                        //
                        // Everything touching `session` happens first so its
                        // borrow ends before the clipboard calls, which need
                        // `&mut self`.
                        let text = {
                            let mut term = session.terminal().lock();
                            let text = term.selection_text();
                            if text.is_some() {
                                term.set_selection(None);
                            }
                            text
                        };
                        match text {
                            Some(t) => self.set_clipboard(t),
                            None => self.paste(),
                        }
                    }
                    _ => {}
                }

                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }

            WindowEvent::MouseWheel { delta, .. } => {
                // The open launcher swallows the wheel without scrolling
                // anything: a menu that lets the grid scroll beneath it
                // reads as detached from the window it floats over. (It
                // grows its own scroll with the profiles editor, not here.)
                if self.launcher.is_some() {
                    return;
                }
                // An open modal overlay takes the wheel wholesale. The
                // settings tab is below: not modal, so it scrolls only under
                // the pointer, by hit region, like the strip does.
                if self.picker.is_some() || self.palette_ui.is_some() || self.value_picker.is_some()
                {
                    let px = match delta {
                        MouseScrollDelta::LineDelta(_, y) => y * 40.0,
                        MouseScrollDelta::PixelDelta(p) => p.y as f32,
                    };
                    if px != 0.0 {
                        if let Some(p) = self.picker.as_mut() {
                            p.scroll -= px;
                        }
                        if let Some(p) = self.palette_ui.as_mut() {
                            p.scroll -= px;
                        }
                        if let Some(p) = self.value_picker.as_mut() {
                            p.scroll -= px;
                        }
                        self.mark_chrome_dirty();
                    }
                    return;
                }
                // Over ANY of the settings tab's regions, the wheel belongs
                // to settings — a gap in this list sent the scroll to the
                // strip or the session behind the tab. An open dropdown menu
                // swallows it without scrolling: moving the rows would slide
                // the menu's anchor out from under it.
                match self.chrome_hit(self.pointer_pos.0, self.pointer_pos.1) {
                    Some(HitRegion::SettingsMenuRow(_)) => return,
                    Some(
                        HitRegion::SettingsPanel
                        | HitRegion::SettingsRow(_)
                        | HitRegion::SettingsToggle(_)
                        | HitRegion::SettingsSlider(_)
                        | HitRegion::SettingsReset(_)
                        | HitRegion::SettingsCategory(_)
                        | HitRegion::SettingsFilter
                        | HitRegion::SettingsEditToml
                        | HitRegion::SettingsSegment(..)
                        | HitRegion::SettingsStep(..)
                        | HitRegion::SettingsSelect(_)
                        | HitRegion::SettingsListRemove(..)
                        | HitRegion::SettingsListAdd(_)
                        | HitRegion::SettingsListItem(..)
                        | HitRegion::ProfilesChoice(..),
                    ) => {
                        let px = match delta {
                            MouseScrollDelta::LineDelta(_, y) => y * 40.0,
                            MouseScrollDelta::PixelDelta(p) => p.y as f32,
                        };
                        if px != 0.0 {
                            // While the Profiles screen is up, these regions
                            // are its rows pane's (the Settings model is not
                            // built under a covering screen).
                            if self.profiles_tab_active() {
                                if let Some(ui) = self.profiles_ui.as_mut() {
                                    ui.scroll -= px;
                                }
                            } else if let Some(ui) = self.settings_ui.as_mut() {
                                ui.scroll -= px;
                            }
                            self.mark_chrome_dirty();
                        }
                        return;
                    }
                    _ => {}
                }
                // Over the strip, the wheel scrolls the strip.
                if self.chrome_hit(self.pointer_pos.0, self.pointer_pos.1).is_some() {
                    let px = match delta {
                        MouseScrollDelta::LineDelta(x, y) => {
                            let step = if x.abs() > y.abs() { x } else { y };
                            step * 40.0
                        }
                        MouseScrollDelta::PixelDelta(p) => {
                            (if p.x.abs() > p.y.abs() { p.x } else { p.y }) as f32
                        }
                    };
                    if px != 0.0 {
                        // Layout clamps; storing the raw value would let the
                        // scroll wander past the content and take clicks with it.
                        self.strip_scroll -= px;
                        self.mark_chrome_dirty();
                    }
                    return;
                }

                let Some(session) = self.tabs.active_source() else { return };
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y,
                    // Trackpads report pixels. Convert with the cell height so
                    // the feel matches a wheel.
                    MouseScrollDelta::PixelDelta(p) => {
                        let ch = self.fonts.as_ref().map_or(20.0, |f| f.cell_metrics().cell_h as f32);
                        p.y as f32 / ch
                    }
                };
                self.scroll_accum += lines;
                let whole = self.scroll_accum.trunc();
                self.scroll_accum -= whole;
                if whole == 0.0 {
                    return;
                }

                // A mouse-aware program gets the wheel. And in the alternate
                // screen there is no scrollback to move through anyway -- `less`
                // and `man` expect the wheel as arrow keys, so scrolling our own
                // (empty) history would look like the wheel doing nothing.
                let alt = session.terminal().lock().modes().contains(zest_core::Modes::ALT_SCREEN);
                let count = whole.abs() as usize * 3;
                let up = whole > 0.0;

                if self.forward_wheel(up, count) {
                    return;
                }
                if alt && !self.modifiers.shift_key() {
                    let key: &[u8] = if up { b"\x1b[A" } else { b"\x1b[B" };
                    let mut out = Vec::with_capacity(key.len() * count);
                    for _ in 0..count {
                        out.extend_from_slice(key);
                    }
                    session.write(out);
                    return;
                }

                session.terminal().lock().scroll_display(whole as isize * 3);
                session.mark_dirty();
                if let Some(w) = self.window.as_ref() {
                    w.request_redraw();
                }
            }

            WindowEvent::RedrawRequested => {
                // Damage gates the frame entirely. An idle terminal must use 0%
                // GPU -- that is a hard requirement, and it is what separates a
                // real terminal from a demo. The chrome has its own latch,
                // set only by discrete events, so the guarantee survives it.
                let grid_dirty = self.tabs.active_source().is_some_and(|s| s.take_dirty());
                if grid_dirty {
                    // Title changes arrive as ordinary output damage; noticing
                    // them here keeps the tab label and the OS titlebar honest
                    // without inventing a new event for it.
                    let title = self
                        .tabs
                        .active_source()
                        .map(|s| s.terminal().lock().title().trim().to_string())
                        .unwrap_or_default();
                    if title != self.window_title {
                        if let Some(w) = self.window.as_ref() {
                            w.set_title(if title.is_empty() { "zesterm" } else { &title });
                        }
                        self.window_title = title;
                        self.chrome_dirty = true;
                        self.chrome_layout = None;
                    }
                }
                if grid_dirty || self.chrome_dirty {
                    // Applied here rather than in the parser thread: the policy
                    // is about what the user is looking at, and the parser has
                    // no business knowing that. It also means a flood costs one
                    // snap per frame, not one per line.
                    if grid_dirty && self.config.scroll_on_output {
                        if let Some(session) = self.tabs.active_source() {
                            session.terminal().lock().scroll_to_bottom();
                        }
                    }
                    // `redraw` clears the chrome latch only when a frame is
                    // actually presented.
                    self.redraw();
                }
            }

            _ => {}
        }
    }

    fn new_events(&mut self, _el: &ActiveEventLoop, cause: winit::event::StartCause) {
        // The animation clock fired: one repaint, then `about_to_wait`
        // schedules the next tick — or nothing, if the animator's condition
        // cleared in between. That is the settle guarantee in one place.
        if matches!(cause, winit::event::StartCause::ResumeTimeReached { .. }) {
            self.chrome_dirty = true;
            if let Some(session) = self.tabs.active_source() {
                session.mark_dirty();
            }
            if let Some(w) = self.window.as_ref() {
                w.request_redraw();
            }
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        // The PNG is written; leave through the front door so the pty, the
        // clipboard and the tab state all get their `Drop` rather than being
        // cut off by `process::exit`. The code travels back to `main` in the
        // field, which is the whole reason it is a field.
        if self.exit_code.is_some() {
            el.exit();
            return;
        }
        // Wait for something to happen rather than polling — unless something
        // on screen is animating, in which case the clock names the *one*
        // deadline it needs. A resting window schedules nothing (the 0%-idle
        // guarantee); a blinking cursor costs exactly its two frames a
        // second, which is the price of the setting being on.
        // The screenshot deadline is one more thing that wants waking for, and
        // the earlier of the two wins — a blinking cursor must not push the
        // capture past its delay, and the capture must not stop the cursor
        // blinking in the frame it captures.
        let now = std::time::Instant::now();
        let shot = self.screenshot_at.map(|at| at.saturating_duration_since(now));
        match [self.anim_deadline(), shot].into_iter().flatten().min() {
            Some(delay) => el.set_control_flow(ControlFlow::WaitUntil(now + delay)),
            None => el.set_control_flow(ControlFlow::Wait),
        }
    }
}

/// Render `scene` into a texture of our own and write it out as a PNG.
///
/// Returns the process exit code: a screenshot that silently did not happen is
/// the failure mode worth spending an exit code on, because the caller is
/// usually a script that goes on to read the file.
///
/// The texture takes the *surface's* format rather than a convenient one — the
/// render pipelines were built for it, and matching it is what makes this the
/// same frame the window would have shown rather than a re-render under
/// different rules.
fn capture_frame(gpu: &mut Gpu, scene: &zest_render_wgpu::Scene, path: &std::path::Path) -> u8 {
    // Checked here rather than left to `read_rgba`'s assertion, because this is
    // not a programmer error: the surface format is whatever the adapter
    // offered (the first non-sRGB entry in `caps.formats`), and an HDR or
    // 10-bit display can hand back one this cannot encode. The library keeps
    // its invariant; the app owes the user a sentence and an exit code rather
    // than a panic and a backtrace.
    if zest_render_wgpu::capture::channel_swap(gpu.config.format).is_none() {
        eprintln!(
            "[screenshot] this adapter's surface is {:?}, which is not 8-bit RGBA or BGRA \
             and cannot be written as a PNG. Nothing was captured.",
            gpu.config.format
        );
        return 1;
    }

    let (width, height) = (gpu.config.width, gpu.config.height);
    let texture = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("zest screenshot"),
        size: wgpu::Extent3d { width, height, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: gpu.config.format,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&Default::default());

    let mut encoder = gpu.device.create_command_encoder(&Default::default());
    gpu.renderer.render(&gpu.device, &gpu.queue, &mut encoder, &view, scene);
    gpu.queue.submit([encoder.finish()]);

    let pixels = zest_render_wgpu::read_rgba(
        &gpu.device,
        &gpu.queue,
        &texture,
        width,
        height,
        gpu.config.format,
    );
    // PNG explicitly, not inferred from the extension. `save_buffer` picks the
    // encoder from the path, so `--screenshot shot.jpg` would quietly write a
    // JPEG -- lossy, which for a screenshot used to compare exact pixels is a
    // wrong answer rather than a different one -- and an extensionless path
    // would fail outright. The flag says PNG, so it writes PNG.
    match image::save_buffer_with_format(
        path,
        &pixels,
        width,
        height,
        image::ColorType::Rgba8,
        image::ImageFormat::Png,
    ) {
        Ok(()) => {
            println!("[screenshot] {width}x{height} -> {}", path.display());
            0
        }
        Err(e) => {
            eprintln!("[screenshot] could not write {}: {e}", path.display());
            1
        }
    }
}

async fn init_gpu(
    window: &Arc<Window>,
    want_transparency: bool,
    clear_color: wgpu::Color,
    antialias: zest_font::TextAntialias,
) -> Gpu {
    let t = std::time::Instant::now();

    // One backend at a time, preferred first.
    //
    // Probing several costs real startup latency -- initializing a Vulkan *and*
    // a DX12 instance, then enumerating adapters on both, was ~670ms of the
    // ~1.9s launch. `Backends::all()` is worse still, since it also spins up an
    // OpenGL stack we will never use.
    //
    // Vulkan leads on Windows because it is the only backend that reports
    // `PreMultiplied` alpha there (ADR-003); DX12 reports `Opaque` on every
    // adapter, so preferring it would silently cost transparency.
    let preferred: &[wgpu::Backends] = if cfg!(target_os = "macos") {
        &[wgpu::Backends::METAL]
    } else if cfg!(windows) {
        &[wgpu::Backends::VULKAN, wgpu::Backends::DX12]
    } else {
        &[wgpu::Backends::VULKAN, wgpu::Backends::GL]
    };

    // `ZESTERM_BACKEND=dx12|vulkan|gl` forces one, for measuring.
    let forced = std::env::var("ZESTERM_BACKEND").ok().and_then(|s| {
        match s.to_ascii_lowercase().as_str() {
            "dx12" => Some(wgpu::Backends::DX12),
            "vulkan" => Some(wgpu::Backends::VULKAN),
            "gl" => Some(wgpu::Backends::GL),
            _ => None,
        }
    });
    let forced_list = forced.map(|b| vec![b]);
    let preferred: &[wgpu::Backends] = forced_list.as_deref().unwrap_or(preferred);

    let mut found = None;
    for &backends in preferred {
        let t_inst = std::time::Instant::now();
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor {
            backends,
            ..wgpu::InstanceDescriptor::new_without_display_handle()
        });
        tracing::debug!(?backends, ms = t_inst.elapsed().as_millis(), "instance created");
        let Ok(surface) = instance.create_surface(Arc::clone(window)) else { continue };
        tracing::debug!(ms = t_inst.elapsed().as_millis(), "surface created");
        if let Ok(adapter) = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                compatible_surface: Some(&surface),
                ..Default::default()
            })
            .await
        {
            found = Some((surface, adapter));
            break;
        }
        tracing::debug!(?backends, "no adapter; trying the next backend");
    }

    let (surface, adapter) = found.expect("no suitable GPU adapter on any backend");
    tracing::debug!(elapsed_ms = t.elapsed().as_millis(), "adapter");
    tracing::info!(adapter = %adapter.get_info().name, backend = ?adapter.get_info().backend, "gpu");

    // Conservative limits everywhere except texture size.
    //
    // `downlevel_defaults` caps 2D textures at 2048, which is smaller than an
    // ordinary window on a modern display -- configuring the surface then fails
    // validation outright. Raise only the dimension limits to what the adapter
    // actually offers, and keep the conservative values for everything else so
    // the renderer stays runnable on weak hardware.
    // Pipeline caching removes most of the ~450ms spent creating pipelines on a
    // cold start. Requested only when the adapter offers it, so a machine
    // without it simply pays the old cost.
    let want_cache = adapter.features().contains(wgpu::Features::PIPELINE_CACHE);
    let mut features = wgpu::Features::empty();
    if want_cache {
        features |= wgpu::Features::PIPELINE_CACHE;
    }
    // Subpixel text blends three coverages against the destination, which needs
    // a per-channel destination factor. DX12 (including WARP), Vulkan where the
    // adapter reports dualSrcBlend, and Metal all have it; asked for only when
    // offered, so a device without it starts normally and the renderer falls
    // back to grayscale.
    if adapter.features().contains(wgpu::Features::DUAL_SOURCE_BLENDING) {
        features |= wgpu::Features::DUAL_SOURCE_BLENDING;
    }

    let adapter_limits = adapter.limits();
    let limits = wgpu::Limits {
        max_texture_dimension_1d: adapter_limits.max_texture_dimension_1d,
        max_texture_dimension_2d: adapter_limits.max_texture_dimension_2d,
        max_texture_dimension_3d: adapter_limits.max_texture_dimension_3d,
        ..wgpu::Limits::downlevel_defaults()
    };
    let max_dim = limits.max_texture_dimension_2d;

    let (device, queue) = adapter
        .request_device(&wgpu::DeviceDescriptor {
            label: Some("zesterm"),
            required_features: features,
            required_limits: limits,
            ..Default::default()
        })
        .await
        .expect("request device");
    tracing::debug!(elapsed_ms = t.elapsed().as_millis(), "device");

    let caps = surface.get_capabilities(&adapter);

    // A NON-sRGB format, deliberately. The resolve pass performs the sRGB
    // encode itself so that premultiplication happens in encoded space; an sRGB
    // surface would encode a second time and wash everything out. -> ADR-003.
    let format = caps
        .formats
        .iter()
        .copied()
        .find(|f| !f.is_srgb())
        .unwrap_or_else(|| {
            tracing::warn!("no non-sRGB surface format; colours will be over-bright");
            caps.formats[0]
        });

    // Transparency is adapter-dependent on Windows: DX12 reports Opaque on every
    // adapter, and Vulkan only on some. Never silently ignore the setting.
    let alpha_mode = if want_transparency
        && caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::PreMultiplied)
    {
        wgpu::CompositeAlphaMode::PreMultiplied
    } else {
        if want_transparency {
            tracing::warn!(
                available = ?caps.alpha_modes,
                "this adapter cannot composite per-pixel alpha; window opacity ignored"
            );
        }
        wgpu::CompositeAlphaMode::Opaque
    };

    let size = window.inner_size();
    let config = wgpu::SurfaceConfiguration {
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        format,
        color_space: wgpu::SurfaceColorSpace::Auto,
        width: size.width.clamp(1, max_dim),
        height: size.height.clamp(1, max_dim),
        // Mailbox where available: no tearing, lower latency than Fifo because
        // it replaces the queued frame rather than queueing behind it.
        present_mode: if caps.present_modes.contains(&wgpu::PresentMode::Mailbox) {
            wgpu::PresentMode::Mailbox
        } else {
            wgpu::PresentMode::Fifo
        },
        desired_maximum_frame_latency: 2,
        alpha_mode,
        view_formats: vec![],
    };
    surface.configure(&device, &config);
    tracing::debug!(elapsed_ms = t.elapsed().as_millis(), "surface configured");

    // Paint the surface the theme colour before the pipelines exist, so the
    // handover from the OS-painted background is seamless. A clear needs no
    // pipeline, so this costs nothing and avoids a flicker at the moment the
    // swapchain starts covering the window.
    clear_to(&device, &queue, &surface, clear_color);
    tracing::debug!(elapsed_ms = t.elapsed().as_millis(), "first gpu paint");

    let info = adapter.get_info();
    let cached = pipeline_cache::load(&info);
    let previous_len = cached.as_ref().map_or(0, Vec::len);
    let cache = pipeline_cache::create(&device, want_cache, cached.as_deref());

    let mut renderer =
        Renderer::with_cache(&device, format, cache.as_ref(), antialias);
    renderer.resize(&device, config.width, config.height);
    tracing::debug!(
        elapsed_ms = t.elapsed().as_millis(),
        cached = cache.is_some(),
        "pipelines"
    );

    // Saved after the pipelines exist, so the blob contains what was just
    // compiled. Only writes when something new was added.
    if let Some(cache) = cache.as_ref() {
        pipeline_cache::save(cache, &info, previous_len);
    }

    Gpu { surface, device, queue, config, renderer }
}

/// Paint the surface a solid colour. Needs no pipeline, only a clear.
fn clear_to(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    surface: &wgpu::Surface<'static>,
    color: wgpu::Color,
) {
    let frame = match surface.get_current_texture() {
        wgpu::CurrentSurfaceTexture::Success(t) | wgpu::CurrentSurfaceTexture::Suboptimal(t) => t,
        _ => return,
    };
    let view = frame.texture.create_view(&Default::default());
    let mut encoder = device.create_command_encoder(&Default::default());
    drop(encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("zest first paint"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view: &view,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(color),
                store: wgpu::StoreOp::Store,
            },
            depth_slice: None,
        })],
        ..Default::default()
    }));
    queue.submit([encoder.finish()]);
    queue.present(frame);
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

/// The cell-background opacity for one viewport: the identity's override when
/// it set one, the window's otherwise. Rides the viewport only — whether the
/// *surface* can show the desktop through stays a window-level decision made
/// at surface creation.
fn pane_opacity(window: f32, identity: Option<&crate::tabs::ProfileIdentity>) -> f32 {
    identity.and_then(|i| i.opacity).map_or(window, |o| o.clamp(0.0, 1.0))
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
mod settings_ime_tests {
    use super::SettingsUiState;

    fn state() -> SettingsUiState {
        SettingsUiState {
            selected: 3,
            category: "Appearance".into(),
            filter: String::new(),
            scroll: 0.0,
            scroll_to_selected: false,
            actions: Vec::new(),
            fields: Vec::new(),
            editing: None,
            installed: Vec::new(),
            menu: None,
            list_drag: None,
        }
    }

    #[test]
    fn composed_text_lands_in_the_filter_like_a_keystroke() {
        // The review finding this seam exists for: `on_ime` wrote the commit
        // into `tabs.active_source()` — the concealed session — while the
        // Settings tab held the keyboard, so an IME user typing into the
        // filter typed into the hidden shell instead.
        let mut ui = state();
        ui.commit_text("設定");
        assert_eq!(ui.filter, "設定", "no edit open: the filter is where characters go");
        assert_eq!(ui.selected, 0, "a filter edit resets the selection, exactly like typing");
        assert!(ui.scroll_to_selected, "and brings it into view");
    }

    #[test]
    fn composed_text_feeds_an_open_edit_buffer_before_the_filter() {
        let mut ui = state();
        ui.editing = Some(crate::settings_ui::EditBuffer {
            field_idx: 0,
            buffer: "nu ".into(),
            error: true,
            append: false,
        });
        ui.commit_text("シェル");
        let edit = ui.editing.as_ref().expect("still editing");
        assert_eq!(edit.buffer, "nu シェル", "a typed edit owns the characters, as the key path says");
        assert!(!edit.error, "new input clears a stale parse error, as typing does");
        assert!(ui.filter.is_empty(), "nothing leaks into the filter");
    }

    #[test]
    fn an_open_dropdown_swallows_composed_text() {
        // The key path's dropdown arm ignores `Key::Character`; the IME
        // route must agree, or a commit would edit a filter the user cannot
        // see behind the menu.
        let mut ui = state();
        ui.menu = Some((1, 0));
        ui.commit_text("あ");
        assert!(ui.filter.is_empty(), "the menu owns the keys");
        assert!(ui.editing.is_none());
    }
}

#[cfg(test)]
mod value_picker_tests {
    use super::ValuePickerState;

    fn picker(options: &[&str], filter: &str) -> ValuePickerState {
        ValuePickerState {
            field: 0,
            append: false,
            options: options.iter().map(|s| (*s).to_string()).collect(),
            visible: Vec::new(),
            selected: 0,
            filter: filter.to_string(),
            scroll: 0.0,
            scroll_to_selected: true,
        }
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

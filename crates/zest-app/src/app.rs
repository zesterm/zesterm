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
                letter_spacing: s.typography.letter_spacing.clamp(-5.0, 20.0),
                ..Default::default()
            },
            builtin_box_drawing: s.typography.builtin_box_drawing,
            // Left as `Option` rather than resolved here: the fallback is the
            // *theme's* suggestion, and the theme is not in scope yet.
            text_gamma: s.appearance.text_gamma,
            text_contrast: s.appearance.text_contrast,
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

/// The settings overlay's transient state while it is open.
struct SettingsUiState {
    selected: usize,
    filter: String,
    scroll: f32,
    /// Bring the selection into view on the next layout — set by keyboard
    /// navigation, never by the wheel, so free scrolling does not snap back.
    scroll_to_selected: bool,
    /// Parallel to the drawn rows, same-pass built (the picker discipline).
    actions: Vec<crate::settings_ui::RowAction>,
    /// The schema walk, cached at open: the schema cannot change while the
    /// overlay is up, and re-walking it per hover would be pure waste.
    fields: Vec<zest_config::ui::UiField>,
    /// A typed edit in progress; while `Some`, characters belong to it.
    editing: Option<crate::settings_ui::EditBuffer>,
}

/// Which full-pane screen the window shows in place of the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AppScreen {
    Terminal,
    Fleet,
    Themes,
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
    /// Where each non-default setting came from, kept from the last resolve —
    /// the settings overlay's "set by profile `k8s`" chips read it.
    provenance: std::collections::BTreeMap<String, zest_config::Source>,
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

impl App {
    pub fn new(
        resolved: zest_config::Resolved,
        cli_layer: toml::Table,
        profile: Option<String>,
        proxy: EventLoopProxy<Wakeup>,
    ) -> Self {
        // Taken whole rather than as bare settings: provenance is the part
        // of a resolve that is easy to drop and expensive to add back — the
        // settings overlay's "set by ..." chips are built from it.
        let zest_config::Resolved { settings, provenance, .. } = resolved;
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
            settings_ui: None,
            provenance,
            restart_pending: std::collections::BTreeSet::new(),
            settings_error: None,
            slider_drag: None,
            pending_tabs: Arc::new(parking_lot::Mutex::new(Vec::new())),
            remote_identity: None,
            fonts: None,
            palette,
            chrome_colors,
            chrome_layout: None,
            chrome_dirty: true,
            chrome_hover: None,
            cursor: winit::window::CursorIcon::Default,
            strip_scroll: 0.0,
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

    /// Find or start this machine's daemon and attach a session to it.
    ///
    /// `None` means fall back to an in-process pty. **Never an error the caller
    /// has to handle**: a terminal that refuses to open because a helper binary
    /// is missing has failed at the only job it has, and both paths already
    /// exist behind `SessionSource`.
    fn attach_to_daemon(
        &mut self,
        spec: &CommandSpec,
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
            command: &spec.command_line,
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
            Action::ToggleSettings => self.toggle_settings(),
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
                        session.terminal().lock().set_palette(self.palette.clone());
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
                        session.terminal().lock().set_palette(self.palette.clone());
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
    fn show_screen(&mut self, screen: AppScreen) {
        self.screen = screen;
        self.picker = None;
        self.palette_ui = None;
        self.settings_ui = None;
        self.mark_chrome_dirty();
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

    /// The status bar's model: the active session's facts, plus how its host
    /// is reached. Never blocks — everything here is memory or one small
    /// file read (the git HEAD).
    fn build_status(
        &self,
        fleet_hosts: &[crate::fleet::FleetHost],
    ) -> Option<crate::chrome::model::StatusModel> {
        use crate::chrome::model::{LinkKind, StatusModel};
        let tab = self.tabs.active()?;
        let (cwd_raw, blocks) = {
            let term = tab.source().terminal();
            let term = term.lock();
            let cwd = if term.cwd().is_empty() {
                term.blocks().last().map(|b| b.cwd.clone()).unwrap_or_default()
            } else {
                term.cwd().to_string()
            };
            (cwd, term.blocks().blocks().len())
        };

        let origin = tab.source().origin();
        let (link, latency_ms, cwd, branch) = match &origin {
            Origin::Daemon { host, local: false } => {
                let fleet = fleet_hosts.iter().find(|h| &h.label == host);
                let link = match fleet.and_then(|h| h.reachability) {
                    Some(zest_mesh::Reachability::Cloud) => LinkKind::Tunnel,
                    Some(zest_mesh::Reachability::Loopback) => LinkKind::Loopback,
                    _ => LinkKind::Lan,
                };
                // Another machine's paths: no home shortening, no git probe —
                // both would be guesses about a filesystem we cannot see.
                (link, fleet.and_then(|h| h.rtt_ms), cwd_raw, None)
            }
            _ => {
                let branch = (!cwd_raw.is_empty())
                    .then(|| crate::status::git_branch(std::path::Path::new(&cwd_raw)))
                    .flatten();
                (LinkKind::Loopback, None, crate::status::shorten_home(&cwd_raw), branch)
            }
        };

        // A dropped link outranks whatever path the host normally takes.
        let link = if self.link_down { LinkKind::Reconnecting } else { link };
        Some(StatusModel {
            cwd,
            branch,
            blocks,
            theme: if zest_theme::builtin::get(&self.config.theme).is_some() {
                self.config.theme.clone()
            } else {
                zest_theme::builtin::DEFAULT_DARK.to_string()
            },
            link,
            latency_ms,
        })
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
                if let Some(session) = self.tabs.active_source() {
                    if !text.is_empty() {
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
                    // The slim title bar over the main column: the vertical
                    // layout's counterpart of the strip, and it was once
                    // forgotten here — the grid painted its first two rows
                    // straight over the session name.
                    insets.top += crate::chrome::layout::SLIM_BAR_H * scale;
                }
            }
            // The status bar comes with the chrome: same latch, same layout
            // pass, so the grid and the bar cannot disagree about the edge.
            insets.bottom += crate::chrome::layout::STATUS_H * scale;
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
            && self.settings_ui.is_none()
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

        // Inputs gathered before the &mut borrow of the overlay state below;
        // the clone is a handful of provenance entries, on an event-driven
        // rebuild, not a frame path.
        let settings_inputs = self.settings_ui.is_some().then(|| {
            (
                serde_json::to_value(&self.settings).unwrap_or(serde_json::Value::Null),
                self.provenance.clone(),
                self.restart_pending.clone(),
                self.settings_error.clone(),
            )
        });
        let settings_model = self.settings_ui.as_mut().zip(settings_inputs).map(
            |(ui, (values, provenance, restart_pending, error))| {
                let (rows, actions) = crate::settings_ui::build_rows(
                    &ui.fields,
                    &values,
                    &provenance,
                    &ui.filter,
                    ui.editing.as_ref(),
                    &restart_pending,
                    error.as_deref(),
                );
                ui.actions = actions;
                // A filter edit can strand the selection on a header or past
                // the end; land it on the nearest real row instead.
                ui.selected = crate::settings_ui::nearest_field(&ui.actions, ui.selected);
                crate::chrome::model::SettingsModel {
                    rows,
                    selected: ui.selected,
                    filter: ui.filter.clone(),
                    scroll: ui.scroll,
                    ensure_visible: ui.scroll_to_selected,
                }
            },
        );

        // Built before the font borrow below: the status reads tabs, fleet
        // and the filesystem, never the fonts.
        let fleet_hosts = self.fleet.as_ref().map(|f| f.snapshot()).unwrap_or_default();
        // Only when the strip shows: `insets_at` reserves the bar's edge under
        // the same condition, and a bar the grid does not know about would
        // paint over the last row.
        let status = if self.strip_shown() { self.build_status(&fleet_hosts) } else { None };
        let screen_model = self.build_screen_model(&fleet_hosts);
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
                TabModel {
                    addr: tab.addr,
                    title: if tab.dead { format!("{title} · ended") } else { title },
                    host: host_label,
                    cwd,
                    origin,
                    // Presence joins in with the fleet model; until then a
                    // reachable tab is simply online.
                    presence: TabPresence::Online,
                    accent,
                    running,
                    age,
                    // Dead tabs borrow the connecting style (faint text): not
                    // live, not interactive, still present.
                    connecting: tab.dead,
                }
            })
            .collect();

        // The sidebar's host grouping, built from the same tab models the
        // strip draws — one pass, one truth.
        let sidebar = (self.config.tabs.position == zest_config::settings::TabsPosition::Left)
            .then(|| {
                use zest_mesh::discovery::Presence;
                let mut groups: Vec<crate::chrome::model::HostGroup> = Vec::new();
                for (i, tm) in tab_models.iter().enumerate() {
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
        let model = ChromeModel {
            tabs: tab_models,
            active: self.tabs.active_index(),
            position: self.config.tabs.position,
            strip_scroll: self.strip_scroll,
            hover: self.chrome_hover,
            controls,
            focused: self.focused,
            status,
            sidebar,
            screen: screen_model,
            panes,
            grid_area,
            anim,
            toggle_chord: keymap::chord_for(keymap::Action::ToggleTabLayout),
            palette_chord: keymap::chord_for(keymap::Action::ToggleFleetPicker),
            picker: picker_model,
            palette: palette_model,
            settings: settings_model,
        };

        let colors = self.chrome_colors;
        let mut measure = |s: &str, px: f32, bold: bool, tracking: f32| {
            zest_render_wgpu::measure_ui_run(fonts, s, zest_font::Style::new(bold, false), px, tracking)
        };
        let laid = crate::chrome::layout::layout(&model, &colors, &metrics, &mut measure);
        self.strip_scroll = laid.strip_scroll;
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
        if let Some(state) = self.settings_ui.as_mut() {
            state.scroll = laid.settings_scroll;
            // One layout consumed the request; the wheel is free again.
            state.scroll_to_selected = false;
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
        match (region, button) {
            (HitRegion::Tab(addr), MouseButton::Left) => {
                // Even when it is already the active one: clicking a session
                // means "show me this", which no screen may overrule.
                self.leave_screen();
                if self.tabs.activate_addr(addr) {
                    self.after_activation();
                }
            }
            (HitRegion::TabClose(addr), MouseButton::Left)
            | (HitRegion::Tab(addr), MouseButton::Middle) => {
                self.close_tab(addr, false, el);
            }
            (HitRegion::NewTab, MouseButton::Left) => {
                self.new_tab();
            }
            (HitRegion::LayoutPill, MouseButton::Left) => {
                self.perform(keymap::Action::ToggleTabLayout, el);
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
            // The status bar swallows clicks like the strip does; nothing on
            // it is a control yet.
            (HitRegion::Status, _) => {}
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
            (HitRegion::SettingsRow(i), MouseButton::Left) => {
                if let Some(ui) = self.settings_ui.as_mut() {
                    ui.selected = i;
                }
                self.mark_chrome_dirty();
            }
            (HitRegion::SettingsToggle(i), MouseButton::Left) => {
                // Select first, then flip through the same path the keyboard
                // uses — one code path per change, however it arrives.
                if let Some(ui) = self.settings_ui.as_mut() {
                    ui.selected = i;
                }
                self.adjust_selected_setting(1);
            }
            (HitRegion::SettingsSlider(i), MouseButton::Left) => {
                if let Some(ui) = self.settings_ui.as_mut() {
                    ui.selected = i;
                }
                self.slider_drag = Some(i);
                self.apply_slider_at(i, self.pointer_pos.0 as f32);
            }
            (HitRegion::SettingsScrim, MouseButton::Left) => {
                self.settings_ui = None;
                self.mark_chrome_dirty();
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
                // order-independent.
                self.palette_ui = None;
                self.settings_ui = None;
                Some(PickerState {
                    selected: 0,
                    filter: String::new(),
                    scroll: 0.0,
                    scroll_to_selected: false,
                    actions: Vec::new(),
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
                self.settings_ui = None;
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

    /// Toggle the settings overlay (⌘,).
    fn toggle_settings(&mut self) {
        self.settings_ui = match self.settings_ui {
            Some(_) => None,
            None => {
                self.picker = None;
                self.palette_ui = None;
                Some(SettingsUiState {
                    selected: 0,
                    filter: String::new(),
                    scroll: 0.0,
                    scroll_to_selected: true,
                    actions: Vec::new(),
                    fields: zest_config::ui::fields(),
                    editing: None,
                })
            }
        };
        self.mark_chrome_dirty();
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
        let next = self.settings_ui.as_ref().and_then(|ui| {
            let field = ui.fields.get(idx)?;
            let themes: Vec<String> =
                zest_theme::builtin::all().into_iter().map(|t| t.id).collect();
            crate::settings_ui::adjust(field, &current, dir, &themes)
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
            // value is unambiguous.
            Widget::Toggle | Widget::Select | Widget::ThemePicker => {
                self.adjust_selected_setting(1);
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
                    });
                }
            }
            // The list widgets have no inline editor; their rows say where
            // the edit happens instead.
            Widget::FontList | Widget::TagList | Widget::KeyValue => {}
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

    /// Open a new tab on the current tab's host (⌘T, the + button).
    ///
    /// One daemon per window today, so "the current tab's host" is the
    /// window's route; the fleet model makes it genuinely per-tab. Runs
    /// inline: creating on an already-proven route is sub-millisecond on
    /// loopback and a few on the LAN — the picker's cold dials are the ones
    /// that must not block, and they arrive with the fleet model.
    fn new_tab(&mut self) {
        let (cols, rows) = self.current_dims();

        match (&self.route, &self.client_identity) {
            (Some(route), Some(identity)) => {
                self.next_placeholder += 1;
                let cell = Arc::new(parking_lot::Mutex::new(crate::tabs::placeholder_addr(
                    self.next_placeholder,
                )));
                let wake = wake_for(&self.proxy, Arc::clone(&cell), Arc::clone(&self.activity));
                // Empty means the host's default shell — for a remote host,
                // its shell, never this machine's command line.
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
                        session.terminal().lock().set_palette(self.palette.clone());
                        let local = route.is_local();
                        self.tabs.push(Tab::daemon(session, local, (cols, rows)));
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
                match Session::spawn(
                    &self.build_spec(),
                    PtySize::new(cols, rows),
                    self.config.scrollback,
                    wake_for(&self.proxy, cell, Arc::clone(&self.activity)),
                ) {
                    Ok(session) => {
                        session.terminal().lock().set_palette(self.palette.clone());
                        self.tabs.push(Tab::in_process(session, addr, (cols, rows)));
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
        self.after_activation();
        self.relayout_grid();
    }

    /// Housekeeping after the active tab changed.
    fn after_activation(&mut self) {
        // Choosing a session is choosing to look at it: any full-pane screen
        // steps aside. Without this, clicking a sidebar row under the fleet
        // view activated the session *invisibly* — and the only way out of
        // the screen was knowing about Esc.
        self.screen = AppScreen::Terminal;
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
        // fleet directory would be chrome over the wrong content.
        if self.screen != AppScreen::Terminal {
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
                let mut first = None;
                let mut last = None;
                for (r, line) in row_lines.iter().enumerate() {
                    if line.is_some_and(|l| l >= header.0 && l < header.1) {
                        first.get_or_insert(r);
                        last = Some(r + 1);
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
                    let left_source = self
                        .tabs
                        .active()
                        .expect("split implies an active tab")
                        .source();
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
                            opacity: self.config.opacity,
                            selection: term_l.selection(),
                            selection_bg: self.selection_bg,
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
                            opacity: self.config.opacity,
                            selection: term_r.selection(),
                            selection_bg: self.selection_bg,
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
                            opacity: self.config.opacity,
                            selection: term.selection(),
                            selection_bg: self.selection_bg,
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
            return;
        }
        tracing::info!(?class, keys = ?changed, "config changed");

        self.settings = new.clone();
        self.config = Config::from(new);
        self.provenance = load.resolved.provenance;
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
        // nobody can reproduce on demand.
        for tab in self.tabs.iter() {
            tab.source().terminal().lock().set_palette(self.palette.clone());
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
        match Fonts::new(&self.config.font_families, typo) {
            Ok(mut fonts) => {
                fonts.set_builtin_box_drawing(self.config.builtin_box_drawing);
                self.fonts = Some(fonts);
            }
            Err(e) => {
                // Keeping the old fonts is the only safe answer: there is no
                // such thing as a terminal with no font.
                tracing::error!(error = %e, "new font stack unusable; keeping the previous one");
                return;
            }
        }
        if let (Some(gpu), Some(w)) = (self.gpu.as_mut(), self.window.as_ref()) {
            gpu.renderer.clear_atlas();
            let size = w.inner_size();
            self.resize_surface(size.width, size.height);
        }
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

        let mut tab: Tab = match self.attach_to_daemon(&spec, cols, rows, &proxy, restore_active) {
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
        let gpu = pollster::block_on(init_gpu(&window, self.config.opacity < 1.0, clear));
        tracing::debug!(elapsed_ms = t0.elapsed().as_millis(), "gpu ready");

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
                for (tab, focus) in tabs {
                    if focus {
                        self.tabs.push(tab);
                    } else {
                        self.tabs.push_background(tab);
                    }
                }
                self.after_activation();
                self.relayout_grid();
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
                                self.toggle_settings();
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
                                Some(keymap::Action::ToggleSettings) => self.toggle_settings(),
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

                // And the open settings overlay, the same way.
                if self.settings_ui.is_some() {
                    use winit::keyboard::{Key, NamedKey};

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
                                let parsed = self.settings_ui.as_ref().and_then(|ui| {
                                    let edit = ui.editing.as_ref()?;
                                    let field = ui.fields.get(edit.field_idx)?;
                                    Some((
                                        edit.field_idx,
                                        crate::settings_ui::parse_input(field, &edit.buffer),
                                    ))
                                });
                                match parsed {
                                    Some((idx, Some(value))) => {
                                        if let Some(ui) = self.settings_ui.as_mut() {
                                            ui.editing = None;
                                        }
                                        self.apply_edit(idx, value);
                                    }
                                    // A failed parse keeps the buffer and
                                    // marks it: silently dropping typed input
                                    // reads as a broken Enter key.
                                    Some((_, None)) => {
                                        if let Some(edit) = self
                                            .settings_ui
                                            .as_mut()
                                            .and_then(|ui| ui.editing.as_mut())
                                        {
                                            edit.error = true;
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
                            // Layered: a search in progress clears first, a
                            // second Escape closes. A settings filter is a
                            // navigation the user built, not the picker's
                            // throwaway two letters.
                            if let Some(ui) = self.settings_ui.as_mut() {
                                if ui.filter.is_empty() {
                                    self.settings_ui = None;
                                } else {
                                    ui.filter.clear();
                                    ui.selected = 0;
                                    ui.scroll_to_selected = true;
                                }
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
                                Some(keymap::Action::ToggleSettings) => self.toggle_settings(),
                                Some(keymap::Action::ToggleFleetPicker) => self.toggle_picker(),
                                Some(keymap::Action::TogglePalette) => self.toggle_palette(),
                                _ => {
                                    if !self.modifiers.control_key()
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
                // A slider drag ends when any button releases, wherever the
                // pointer wandered to in the meantime.
                if state == ElementState::Released && self.slider_drag.take().is_some() {
                    return;
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
                // An open modal overlay takes the wheel wholesale.
                if self.picker.is_some() || self.palette_ui.is_some() || self.settings_ui.is_some() {
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
                        if let Some(ui) = self.settings_ui.as_mut() {
                            ui.scroll -= px;
                        }
                        self.mark_chrome_dirty();
                    }
                    return;
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
    let features = if want_cache {
        wgpu::Features::PIPELINE_CACHE
    } else {
        wgpu::Features::empty()
    };

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

    let mut renderer = Renderer::with_cache(&device, format, cache.as_ref());
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
        gamma: config.text_gamma.clamp(0.5, 2.5),
        contrast: config.text_contrast.clamp(0.0, 1.0),
    }
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

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
use crate::chrome::hit::HitRegion;
use crate::chrome::layout::ChromeLayout;
use crate::chrome::model::{ChromeMetrics, ChromeModel, TabModel, TabOrigin, TabPresence};
use crate::chrome::theme::ChromeColors;
use crate::chrome::Insets;
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
    pub theme: String,
    pub scrollback: usize,
    pub opacity: f32,
    /// Space between the window edge and the grid, in logical pixels.
    pub padding: u32,
    /// The strip's own alpha, independent of the grid's (ADR-003).
    pub chrome_opacity: f32,
    /// The tab strip's knobs, taken whole — the chrome reads all of them.
    pub tabs: zest_config::settings::Tabs,
    pub shell: Option<String>,
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
            theme: s.appearance.theme.clone(),
            scrollback: s.scrolling.scrollback,
            opacity: s.window.opacity.clamp(0.0, 1.0),
            padding: s.window.padding.min(64),
            chrome_opacity: s.window.chrome_opacity.clamp(0.0, 1.0),
            tabs: s.tabs.clone(),
            shell: (!s.shell.command.is_empty()).then(|| s.shell.command.clone()),
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

/// A tab's wake callback: forward to the event loop, translating the
/// window-scoped `Exited` into a tab-scoped `TabExited`.
///
/// The address lives in a shared cell because it is not always known when the
/// callback is built — a created session learns its address from the daemon —
/// and because a supervisor rebind moves it.
fn wake_for(
    proxy: &EventLoopProxy<Wakeup>,
    addr: Arc<parking_lot::Mutex<zest_proto::SessionAddr>>,
) -> impl Fn(Wakeup) + Send + 'static {
    let proxy = proxy.clone();
    move |w| {
        let w = match w {
            Wakeup::Exited => Wakeup::TabExited(*addr.lock()),
            other => other,
        };
        let _ = proxy.send_event(w);
    }
}

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
    /// How to reach the daemon this window's tabs live on, for ⌘T and the
    /// redials it implies. One daemon today; the fleet model generalizes it.
    route: Option<HostRoute>,
    /// The identity every daemon tab proves. One per window, minted or loaded
    /// once — N tabs are one client holding N sessions, not N clients.
    client_identity: Option<Arc<zest_mesh::identity::ClientIdentity>>,
    /// Distinct placeholder addresses for sessions with no real one.
    next_placeholder: u64,
    fonts: Option<Fonts>,
    palette: zest_core::PaletteSnapshot,

    /// The chrome's resolved palette; rebuilt with the theme.
    chrome_colors: ChromeColors,
    /// Last laid-out chrome, shared by redraw and the input path so a click
    /// is tested against exactly what is on screen.
    chrome_layout: Option<ChromeLayout>,
    /// The chrome's own damage latch, beside the session's. Set only by
    /// discrete events (hover, focus, title, config), so 0%-idle holds.
    chrome_dirty: bool,
    /// What the pointer was last over, for hover fills.
    chrome_hover: Option<HitRegion>,
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
}


impl App {
    pub fn new(
        settings: zest_config::Settings,
        cli_layer: toml::Table,
        profile: Option<String>,
        proxy: EventLoopProxy<Wakeup>,
    ) -> Self {
        let config = Config::from(&settings);
        let theme = zest_theme::builtin::get(&config.theme)
            .unwrap_or_else(zest_theme::builtin::obsidian);
        let resolved = zest_theme::resolve(&theme);
        let palette = to_core_palette(&resolved);
        let chrome_colors = ChromeColors::new(&theme.ui, &theme.effects, config.chrome_opacity);
        let selection_bg = zest_core::Rgb::new(
            resolved.selection_bg.r,
            resolved.selection_bg.g,
            resolved.selection_bg.b,
        );

        Self {
            config,
            proxy,
            window: None,
            gpu: None,
            tabs: TabStrip::default(),
            route: None,
            client_identity: None,
            next_placeholder: 0,
            fonts: None,
            palette,
            chrome_colors,
            chrome_layout: None,
            chrome_dirty: true,
            chrome_hover: None,
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
        }
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
        let wake = wake_for(proxy, Arc::clone(&addr_cell));
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

        let session = crate::remote::RemoteSession::attach(
            dial,
            &crate::remote::AttachOptions {
                identity: &identity,
                label: "zesterm",
                command: &spec.command_line,
                cols,
                rows,
                scrollback: self.config.scrollback,
                // Adopt an unattached session if there is one. Creating every
                // time meant a session could never be picked up again -- the
                // whole point of the daemon owning it -- and leaked one shell
                // per launch, because closing a window only detaches.
                adopt: !self.new_session,
                local: true,
                // Loopback: the socket already answered "is this my machine",
                // and there is no advertisement to have been misled by.
                expect_host: None,
            },
            wake,
        );

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
                    // Dropped explicitly, because `process::exit` below runs no
                    // destructors: without this the Detach is never written,
                    // the daemon goes on believing a dead client is attached,
                    // and the next probe cannot adopt the session it left.
                    drop(session);
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
        let wake = wake_for(proxy, Arc::clone(&addr_cell));
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
        let Some(path) = zest_config::paths::config_dir().map(|d| d.join("config.toml")) else {
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
            let block = block_actions::at_row(&term, row)?;
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
                }
            }
        }
        insets
    }

    /// Whether the strip is drawn at all.
    fn strip_shown(&self) -> bool {
        self.config.tabs.show_single_tab || self.tabs.len() > 1
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
        if !self.strip_shown() {
            self.chrome_layout = None;
            return;
        }
        // The layout cache and the frame-pending latch are separate on
        // purpose: the input path refreshes the layout between frames, and
        // must not eat the redraw the change asked for.
        if self.chrome_layout.is_some() {
            return;
        }
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
        };
        // In fullscreen the traffic lights auto-hide, so the strip reclaims
        // their reserve; everywhere else the answer comes from AppKit fresh,
        // because the inset is not a constant.
        let traffic = if window.fullscreen().is_some() {
            None
        } else {
            platform::traffic_light_inset(window)
                .map(|(x, y)| [x as f32 * scale, y as f32 * scale])
        };

        let tab_models: Vec<TabModel> = self
            .tabs
            .iter()
            .map(|tab| {
                // A brief lock per tab per chrome rebuild — rebuilds are
                // event-driven, so this is microseconds, not a frame cost.
                let title = tab.source().terminal().lock().title().trim().to_string();
                let title = if title.is_empty() { "shell".to_string() } else { title };
                let origin = match tab.source().origin() {
                    Origin::Daemon { host, local: false } => {
                        TabOrigin::Remote { host_label: host }
                    }
                    _ => TabOrigin::Local,
                };
                TabModel {
                    addr: tab.addr,
                    title: if tab.dead { format!("{title} · ended") } else { title },
                    origin,
                    // Presence joins in with the fleet model; until then a
                    // reachable tab is simply online.
                    presence: TabPresence::Online,
                    // Dead tabs borrow the connecting style (faint text): not
                    // live, not interactive, still present.
                    connecting: tab.dead,
                }
            })
            .collect();

        let model = ChromeModel {
            tabs: tab_models,
            active: self.tabs.active_index(),
            position: self.config.tabs.position,
            strip_scroll: self.strip_scroll,
            hover: self.chrome_hover,
            traffic_inset: traffic,
            focused: self.focused,
        };

        let colors = self.chrome_colors;
        let mut measure =
            |s: &str| zest_render_wgpu::measure_ui_run(fonts, s, zest_font::Style::default());
        let laid = crate::chrome::layout::layout(&model, &colors, &metrics, &mut measure);
        self.strip_scroll = laid.strip_scroll;
        self.chrome_layout = Some(laid);
    }

    /// What the pointer is over in the chrome, using the current layout.
    fn chrome_hit(&mut self, x: f64, y: f64) -> Option<HitRegion> {
        self.refresh_chrome();
        self.chrome_layout.as_ref().and_then(|l| l.hit.hit(x as f32, y as f32))
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
            _ => {}
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
                let wake = wake_for(&self.proxy, Arc::clone(&cell));
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
                    wake_for(&self.proxy, cell),
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

        if self.tabs.is_empty() {
            el.exit();
            return;
        }
        self.after_activation();
        self.relayout_grid();
    }

    /// Housekeeping after the active tab changed.
    fn after_activation(&mut self) {
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

    /// Pointer pixels to a grid cell, clamped into the viewport.
    fn cell_at(&self, x: f64, y: f64) -> (usize, usize) {
        let Some(fonts) = self.fonts.as_ref() else { return (0, 0) };
        let m = fonts.cell_metrics();
        let insets = self.insets();
        let col = ((x - f64::from(insets.left)).max(0.0) / f64::from(m.cell_w)) as usize;
        let row = ((y - f64::from(insets.top)).max(0.0) / f64::from(m.cell_h)) as usize;

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
        let mut chrome = Chrome::default();
        if let Some(layout) = self.chrome_layout.as_ref() {
            chrome.rects.extend_from_slice(&layout.rects);
            for run in &layout.texts {
                zest_render_wgpu::emit_ui_run(
                    &gpu.device,
                    &gpu.queue,
                    &mut gpu.renderer.atlas,
                    fonts,
                    &run.text,
                    zest_font::Style::default(),
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
            let term = session.terminal().lock();
            self.scene.build(
                &gpu.device,
                &gpu.queue,
                &mut gpu.renderer.atlas,
                fonts,
                metrics,
                &[Viewport {
                    rect: insets.grid_rect(gpu.config.width, gpu.config.height),
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
                }],
                &chrome,
            );
        } // lock released before any GPU work

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
            Ok(fonts) => self.fonts = Some(fonts),
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
            // than one per tab per frame.
            if let Some(tab) = self.tabs.active_mut() {
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
        let attrs = Window::default_attributes()
            .with_title("zesterm")
            .with_transparent(self.config.opacity < 1.0)
            .with_visible(false)
            .with_inner_size(winit::dpi::LogicalSize::new(960.0, 600.0));
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
        window.set_visible(true);
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
        let fonts = Fonts::new(&self.config.font_families, typo).expect("no usable font");
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
        let mut tab: Tab = match self.attach_to_daemon(&spec, cols, rows, &proxy) {
            Some(tab) => tab,
            None => {
                self.next_placeholder += 1;
                let addr = crate::tabs::placeholder_addr(self.next_placeholder);
                let cell = Arc::new(parking_lot::Mutex::new(addr));
                let session = Session::spawn(
                    &spec,
                    PtySize::new(cols, rows),
                    self.config.scrollback,
                    wake_for(&proxy, cell),
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
                // last state that was true, because the session still exists.
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
            // One tab's child exited. Close that tab — killing is moot, the
            // child is already gone — and the last tab closing closes the
            // window, which is exactly the old single-session behavior.
            Wakeup::TabExited(addr) => {
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
                }
                self.mark_chrome_dirty();
            }
            Wakeup::Reattached => {
                tracing::info!("the daemon connection is back");
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
            WindowEvent::CloseRequested => {
                self.tabs.clear();
                el.exit();
            }

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

                // Tab chords, on the desktop's own modifier (⌘ here). Before
                // the clipboard block because on macOS both tests are true
                // for ⌘ — these letters are simply not c/v/o/r.
                if key::belongs_to_desktop(self.modifiers) {
                    if let winit::keyboard::Key::Character(c) = &event.logical_key {
                        match c.as_str() {
                            // Lowercase only: ⌘⇧T is reserved (reopen-closed,
                            // one day), and shift produces the uppercase.
                            "t" => {
                                self.new_tab();
                                return;
                            }
                            "w" => {
                                if let Some(tab) = self.tabs.active() {
                                    let addr = tab.addr;
                                    self.close_tab(addr, false, el);
                                }
                                return;
                            }
                            // ⌘1..⌘8 pick a tab; ⌘9 is "last", following the
                            // browsers everyone's fingers learned it from.
                            "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" => {
                                let n = c.as_bytes()[0] - b'1';
                                if self.tabs.activate(usize::from(n)) {
                                    self.after_activation();
                                }
                                return;
                            }
                            "9" => {
                                let last = self.tabs.len().saturating_sub(1);
                                if self.tabs.activate(last) {
                                    self.after_activation();
                                }
                                return;
                            }
                            // ⌘⇧[ and ⌘⇧] arrive as { and }.
                            "{" => {
                                if self.tabs.activate_prev() {
                                    self.after_activation();
                                }
                                return;
                            }
                            "}" => {
                                if self.tabs.activate_next() {
                                    self.after_activation();
                                }
                                return;
                            }
                            _ => {}
                        }
                    }
                }
                // Ctrl+Tab / Ctrl+Shift+Tab cycle, as in every tabbed app.
                if self.modifiers.control_key()
                    && event.logical_key == winit::keyboard::Key::Named(winit::keyboard::NamedKey::Tab)
                {
                    let switched = if self.modifiers.shift_key() {
                        self.tabs.activate_prev()
                    } else {
                        self.tabs.activate_next()
                    };
                    if switched {
                        self.after_activation();
                    }
                    return;
                }

                let Some(session) = self.tabs.active_source() else { return };

                // Copy and paste, on whichever chord this desktop uses.
                // See `key::is_clipboard_chord`.
                if key::is_clipboard_chord(self.modifiers) {
                    if let winit::keyboard::Key::Character(c) = &event.logical_key {
                        match c.to_ascii_lowercase().as_str() {
                            "c" => {
                                self.copy_selection();
                                return;
                            }
                            "v" => {
                                self.paste();
                                if let Some(w) = self.window.as_ref() {
                                    w.request_redraw();
                                }
                                return;
                            }
                            // Command blocks. On the same chord as copy and
                            // paste because they are the same kind of thing --
                            // the desktop acting on the terminal rather than
                            // the terminal receiving a keystroke -- and because
                            // that chord is already the one the encoder refuses
                            // to pass through to the shell.
                            "o" => {
                                self.copy_block_output();
                                return;
                            }
                            "r" => {
                                self.rerun_last_command();
                                return;
                            }
                            _ => {}
                        }
                    }
                }

                let modes = session.terminal().lock().modes();

                // Shift+PgUp/PgDn pages the scrollback. The shift is what makes
                // this unambiguous: bare PgUp still belongs to the program, and
                // in the alternate screen there is no scrollback to page through
                // at all -- `less` and `vim` do their own paging and would be
                // left showing a stale screen.
                if self.modifiers.shift_key() && !modes.contains(zest_core::Modes::ALT_SCREEN) {
                    let page = match &event.logical_key {
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::PageUp) => Some(1isize),
                        winit::keyboard::Key::Named(winit::keyboard::NamedKey::PageDown) => Some(-1),
                        _ => None,
                    };
                    if let Some(dir) = page {
                        // A page is one screen less a line of overlap, which is
                        // what makes paged reading continuous rather than
                        // guessing where the seam was.
                        let rows = session.terminal().lock().grid().rows();
                        let lines = dir * (rows.saturating_sub(1).max(1)) as isize;
                        session.terminal().lock().scroll_display(lines);
                        session.mark_dirty();
                        if let Some(w) = self.window.as_ref() {
                            w.request_redraw();
                        }
                        return;
                    }
                }

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

                // The chrome sees the pointer first — unless a grid drag is in
                // progress, which keeps the grid: a selection that wanders
                // into the strip must not die there.
                if !self.mouse.is_dragging() {
                    let over = self.chrome_hit(position.x, position.y);
                    if over != self.chrome_hover {
                        self.chrome_hover = over;
                        self.mark_chrome_dirty();
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
                    (term.selection(), term.abs_pos(cell.0, cell.1))
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
                        if let Some(pos) = term.abs_pos(row, col) {
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

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        // Wait for something to happen rather than polling. With no animations
        // yet there is nothing to schedule; the cursor blink will add a
        // `WaitUntil` here. Reconnecting does not: `RemoteSession` supervises
        // its own link on its own thread and wakes this loop when it is back.
        el.set_control_flow(ControlFlow::Wait);
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


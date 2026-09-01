//! One process, many windows: [`App`] is the window, [`Process`] is the
//! process (ADR-018).
//!
//! `App` was the whole application for as long as there was one window, and
//! nearly every field on it is genuinely the window's — its surface, its
//! strip, its overlays, its pointer. Multi-window did not move those out;
//! it moved the *few* things that are one per process **in**, here, and put
//! a thin owner over the windows that routes each event to the one it is
//! for. The rule for what lives on [`Shared`]: something two windows would
//! break by each having their own. `next_placeholder` is the clearest case —
//! placeholder addresses key wakeup routing, and two windows minting the
//! same one would deliver a tab's exit to the wrong window.
//!
//! Windows never call `el.exit()` and never open windows. They record what
//! they want in [`WindowRequests`] and the process drains that after every
//! dispatch — so "close this window" and "quit" are different things, and the
//! last window closing is the only way the second happens.

use std::cell::{Cell, OnceCell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use winit::application::ApplicationHandler;
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::window::WindowId;

use crate::app::{App, ApprovalCell, NextWake, Screenshot, StartScreen};
use crate::instance::{self, OpenRequest, OpenTarget, PendingOpens, Reply};
use crate::session::Wakeup;
use crate::tabs_state::SavedTab;
use crate::windows_state::{self, Geometry, Rect, SavedWindow, SavedWindows};

/// What is one per process, reached by every window through an `Rc`.
///
/// Each member is interior-mutable on its own rather than the whole struct
/// sitting behind one `RefCell`: a window's methods are long, and one borrow
/// spanning a method that calls another window-facing method is a panic at
/// runtime rather than an error at compile time.
pub struct Shared {
    /// Hosts, presence, and session lists — the picker's data source. One
    /// mDNS browser and one prober per process, started once the first
    /// window has painted.
    pub fleet: OnceCell<crate::fleet::FleetModel>,
    /// A device waiting for THIS machine's approval — the modal's state,
    /// written by the fleet watcher. Every window shows it and any window
    /// may answer it: the question is about the machine, not a window.
    pub approval: ApprovalCell,
    /// Distinct placeholder addresses for sessions with no real one yet.
    next_placeholder: Cell<u64>,
    /// One OS clipboard connection. On X11 the text a copy set lives in the
    /// instance that set it; a window closing must not take it along.
    pub clipboard: RefCell<Option<arboard::Clipboard>>,
    /// The persisted identity used for hosts that are not this machine,
    /// loaded lazily so the keychain stays off the startup path — and
    /// loaded once, so a second window is not a second keychain prompt.
    pub remote_identity: RefCell<Option<Arc<zest_mesh::identity::ClientIdentity>>>,
    /// The context engine for in-process sessions (#434), built on first
    /// need; the daemon's is the same type, and one serves every window.
    pub local_context: std::sync::OnceLock<zest_daemon::context::ContextEngine>,
    /// Restart-class keys edited this run. A restart is owed by the process,
    /// whichever window's settings tab did the editing.
    pub restart_pending: RefCell<std::collections::BTreeSet<String>>,
}

impl Shared {
    fn new() -> Self {
        Self {
            fleet: OnceCell::new(),
            approval: Arc::new(parking_lot::Mutex::new(Vec::new())),
            next_placeholder: Cell::new(0),
            // Created once: constructing a Clipboard opens an OS connection,
            // and doing that per copy is both slow and flaky under contention.
            clipboard: RefCell::new(
                arboard::Clipboard::new()
                    .map_err(|e| tracing::warn!(error = %e, "clipboard unavailable"))
                    .ok(),
            ),
            remote_identity: RefCell::new(None),
            local_context: std::sync::OnceLock::new(),
            restart_pending: RefCell::new(std::collections::BTreeSet::new()),
        }
    }

    /// The next placeholder number; never repeats within the process.
    pub fn mint_placeholder(&self) -> u64 {
        let n = self.next_placeholder.get() + 1;
        self.next_placeholder.set(n);
        n
    }
}

/// What every window of this process is built from.
pub struct WindowTemplate {
    pub resolved: zest_config::Resolved,
    /// Flags, replayed on every reload so a `--size` is not lost to a save.
    pub cli_layer: toml::Table,
    pub profile: Option<String>,
    /// Own the pty in-process instead of attaching to a daemon. Every
    /// window: a second window under `--no-daemon` must also stay
    /// in-process, or the flag would mean "the first window only".
    pub no_daemon: bool,
    /// Attach to another machine's daemon at `host:port`.
    pub attach_addr: Option<String>,
    /// Start a fresh session rather than picking up an idle one.
    pub new_session: bool,
}

/// What only the first window gets: the probes and `--screenshot` measure
/// or photograph *a* window, and a second one would measure nothing.
#[derive(Default)]
pub struct FirstOnly {
    pub startup_probe: bool,
    pub attach_probe: bool,
    pub screenshot: Option<Screenshot>,
    pub start_screen: Option<StartScreen>,
}

/// What a window asks the process for. Drained after every dispatch.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct WindowRequests {
    /// Close this window; the process quits when it was the last.
    pub close: bool,
    /// Open another window beside this one, on the same host.
    pub new_window: bool,
    /// The tab set changed; remember every window.
    pub persist: bool,
}

/// What a tab opened on an existing route holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabOpen {
    /// A shell — the host's default, or `command` — in `cwd` when given.
    Shell { command: Option<String>, cwd: Option<String> },
    /// A named launch profile, through the same path the + menu takes.
    Profile(String),
    /// The Settings tab.
    Settings,
    /// The Profiles tab.
    Profiles,
}

impl TabOpen {
    /// What a launch wants in a *tab*: the profile if it named one, else
    /// the tab-shaped screens, else a shell.
    fn for_tab(req: &OpenRequest) -> Self {
        match (&req.profile, req.screen) {
            (Some(name), _) => Self::Profile(name.clone()),
            (None, Some(StartScreen::Settings)) => Self::Settings,
            (None, Some(StartScreen::Profiles)) => Self::Profiles,
            _ => Self::Shell { command: req.command.clone(), cwd: req.cwd.clone() },
        }
    }

    /// What a launch wants in a *window*: a screen is dispatched by the
    /// window itself (`start_screen`), over the shell or profile.
    fn for_window(req: &OpenRequest) -> Self {
        match &req.profile {
            Some(name) => Self::Profile(name.clone()),
            None => Self::Shell { command: req.command.clone(), cwd: req.cwd.clone() },
        }
    }
}

/// How a window's first tab comes to be.
pub enum FirstTab {
    /// Find or spawn this machine's daemon (or `--attach`'s), reattaching
    /// `restore` inline when there is one, and the `rest` in the background.
    /// The launch path, and what a restored window does.
    Attach { restore: Option<zest_proto::SessionAddr>, rest: Vec<SavedTab> },
    /// A window opened *from* another: the same route and the same proven
    /// identity, so a relay or `--attach` window's far host does not ask
    /// for approval again, then `open` on it.
    Inherit {
        route: zest_fleet::HostRoute,
        identity: Option<Arc<zest_mesh::identity::ClientIdentity>>,
        open: TabOpen,
    },
}

pub struct WindowSpec {
    pub geometry: Geometry,
    pub first_tab: FirstTab,
    /// Wayland's activation token from the launcher, so the compositor lets
    /// the new window take focus under the launcher's right to it.
    pub activation_token: Option<String>,
}

impl WindowSpec {
    /// The launch window when nothing is remembered.
    fn fresh() -> Self {
        Self {
            geometry: Geometry::default(),
            first_tab: FirstTab::Attach { restore: None, rest: Vec::new() },
            activation_token: None,
        }
    }

    fn restored(saved: SavedWindow, monitors: &[Rect]) -> Self {
        let (restore, rest) = windows_state::split_lead(saved.tabs, saved.active);
        Self {
            geometry: windows_state::place(saved.geometry, monitors),
            first_tab: FirstTab::Attach { restore, rest },
            activation_token: None,
        }
    }

    /// A window opened from `from` — by ⌘N, or by a second launch —
    /// cascaded beside it, on its host, holding `open`. A window with no
    /// route (the in-process fallback) opens a launch-shaped window instead,
    /// which is the honest degraded answer.
    fn cascade_from(from: &App, monitors: &[Rect], open: TabOpen) -> Self {
        let first_tab = match from.route() {
            Some(route) => {
                FirstTab::Inherit { route: route.clone(), identity: from.client_identity(), open }
            }
            None => FirstTab::Attach { restore: None, rest: Vec::new() },
        };
        Self {
            geometry: windows_state::cascade(from.current_geometry(), monitors),
            first_tab,
            activation_token: None,
        }
    }
}

/// Where a wakeup goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Route {
    /// Every window whose strip holds this session (a tab or a pane) —
    /// possibly more than one, since adoption dedupes per strip.
    Owner(zest_proto::SessionAddr),
    /// Every window. Each arm is already a no-op for a window the event does
    /// not concern: a `Redraw` reaches a frame that finds nothing dirty and
    /// skips, `TabsChanged` drains a per-window queue, `AccountChanged`
    /// drains per-window cells.
    Broadcast,
    /// The fleet model's change latch. **Not a broadcast**: `take_changed`
    /// clears one flag, so the first window to look would consume it and
    /// every other would see nothing. The process consumes it once and
    /// tells every window.
    Fleet,
    /// The process ends.
    Exit,
    /// A second launch's request is parked; the process opens it.
    Open,
}

/// The routing rule, exhaustive on purpose: a new `Wakeup` variant is a
/// compile error here rather than a wakeup that quietly reaches the wrong
/// window.
#[must_use]
pub fn route(event: &Wakeup) -> Route {
    match event {
        Wakeup::TabExited(addr) | Wakeup::SessionGone(addr) | Wakeup::Attention(addr, _) => {
            Route::Owner(*addr)
        }
        Wakeup::FleetChanged => Route::Fleet,
        Wakeup::Exited => Route::Exit,
        Wakeup::OpenRequested => Route::Open,
        Wakeup::Redraw
        | Wakeup::Detached
        | Wakeup::Reattached
        | Wakeup::ConfigChanged
        | Wakeup::AccountChanged
        | Wakeup::TabsChanged
        | Wakeup::PairingChanged
        | Wakeup::DirListingReady
        | Wakeup::FileContentsReady
        | Wakeup::SignalChanged => Route::Broadcast,
    }
}

/// The earlier of two windows' next wake-ups. A capture that is due outranks
/// everything — it is not a wait at all — and an idle window contributes
/// nothing, so a process of idle windows stays at 0% idle.
#[must_use]
pub fn merge_wakes(a: NextWake, b: NextWake) -> NextWake {
    match (a, b) {
        (NextWake::CaptureNow, _) | (_, NextWake::CaptureNow) => NextWake::CaptureNow,
        (NextWake::After(x), NextWake::After(y)) => NextWake::After(x.min(y)),
        (NextWake::After(x), NextWake::Idle) | (NextWake::Idle, NextWake::After(x)) => {
            NextWake::After(x)
        }
        (NextWake::Idle, NextWake::Idle) => NextWake::Idle,
    }
}

/// What the file remembers after a window closes.
///
/// Closing one window of several is a decision about that window: it is
/// forgotten and the survivors are what the next launch reopens. Closing
/// the *last* window is quitting, and what it showed is exactly what the
/// next launch should show — the same memory the single window always had.
#[must_use]
pub fn snapshot_after_close(
    remaining: Vec<SavedWindow>,
    closing: Option<SavedWindow>,
) -> Vec<SavedWindow> {
    if remaining.is_empty() {
        closing.into_iter().collect()
    } else {
        remaining
    }
}

pub struct Process {
    template: WindowTemplate,
    first: Option<FirstOnly>,
    /// Screenshot and probe runs never touch the remembered set: they
    /// photograph or measure a window that was never the user's.
    persist_allowed: bool,
    shared: Rc<Shared>,
    proxy: EventLoopProxy<Wakeup>,
    /// In creation order, which is also the order the file remembers them.
    /// A `Vec`: N is a handful, and a linear find by id costs nothing.
    windows: Vec<App>,
    /// The window that last took focus — where a request that names no
    /// window lands.
    focused: Option<WindowId>,
    /// Dropping this stops watching the config file.
    config_watcher: Option<zest_config::Watcher>,
    /// Carried out of a closing window (a screenshot that could not be
    /// written), returned by `main`.
    exit_code: Option<u8>,
    /// The `zesterm-app` endpoint, claimed by `main` and served once the
    /// first window has painted (#497).
    instance_claim: Option<instance::Claim>,
    instance: Option<instance::InstanceServer>,
    /// Launches parked by the endpoint's threads for this loop to open.
    pending_opens: PendingOpens,
}

impl Process {
    pub fn new(
        template: WindowTemplate,
        first: FirstOnly,
        proxy: EventLoopProxy<Wakeup>,
        instance_claim: Option<instance::Claim>,
    ) -> Self {
        let persist_allowed = first.screenshot.is_none() && !first.startup_probe && !first.attach_probe;
        Self {
            template,
            first: Some(first),
            persist_allowed,
            shared: Rc::new(Shared::new()),
            proxy,
            windows: Vec::new(),
            focused: None,
            config_watcher: None,
            exit_code: None,
            instance_claim,
            instance: None,
            pending_opens: Arc::default(),
        }
    }

    /// The window a request that names none lands in: the focused one, else
    /// the last opened.
    fn focused_index(&self) -> Option<usize> {
        self.focused
            .and_then(|id| self.index_of(id))
            .or_else(|| self.windows.len().checked_sub(1))
    }

    /// Open what a second launch asked for, and answer it — after the window
    /// exists, never before, so a launcher that reads `Ok` has a window.
    fn open_requested(&mut self, el: &ActiveEventLoop) {
        let parked: Vec<_> = self.pending_opens.lock().drain(..).collect();
        for pending in parked {
            let req = pending.request;
            let target = match (req.target, self.focused_index()) {
                (OpenTarget::Tab, Some(i)) => {
                    self.windows[i].open_tab(&TabOpen::for_tab(&req));
                    i
                }
                _ => {
                    let monitors = Self::monitors(el);
                    let mut spec = match (req.attach.is_some(), self.focused_index()) {
                        (false, Some(i)) => {
                            WindowSpec::cascade_from(&self.windows[i], &monitors, TabOpen::for_window(&req))
                        }
                        _ => WindowSpec::fresh(),
                    };
                    spec.activation_token = req.activation_token.clone();
                    let mut app = self.new_app();
                    if let Some(addr) = req.attach.clone() {
                        app = app.with_attach_addr(addr);
                    }
                    if let Some(screen) = req.screen {
                        app = app.with_start_screen(screen);
                    }
                    app.open_window(el, &spec);
                    self.windows.push(app);
                    self.windows.len() - 1
                }
            };
            self.windows[target].focus();
            let _ = pending.reply.send(Reply::Ok);
        }
        self.persist_all();
    }

    /// What the process should exit with, once the event loop has returned.
    #[must_use]
    pub fn exit_code(&self) -> u8 {
        self.exit_code.unwrap_or(0)
    }

    fn new_app(&mut self) -> App {
        let t = &self.template;
        let mut app = App::new(
            t.resolved.clone(),
            t.cli_layer.clone(),
            t.profile.clone(),
            self.proxy.clone(),
            Rc::clone(&self.shared),
        );
        if t.no_daemon {
            app = app.with_no_daemon();
        }
        if t.new_session {
            app = app.with_new_session();
        }
        if let Some(addr) = t.attach_addr.clone() {
            app = app.with_attach_addr(addr);
        }
        if let Some(first) = self.first.take() {
            if first.startup_probe {
                app = app.with_startup_probe();
            }
            if first.attach_probe {
                app = app.with_attach_probe();
            }
            if let Some(shot) = first.screenshot {
                app = app.with_screenshot(shot);
            }
            if let Some(screen) = first.start_screen {
                app = app.with_start_screen(screen);
            }
        }
        app
    }

    fn monitors(el: &ActiveEventLoop) -> Vec<Rect> {
        el.available_monitors()
            .map(|m| {
                let p = m.position();
                let s = m.size();
                Rect { x: p.x, y: p.y, w: s.width, h: s.height }
            })
            .collect()
    }

    fn index_of(&self, id: WindowId) -> Option<usize> {
        self.windows.iter().position(|w| w.window_id() == Some(id))
    }

    /// Restore replaces adoption: reopen what this process was showing last
    /// time. Only on a plain launch — a probe, a screenshot, `--new-session`
    /// or `--attach` each mean a specific window, not the remembered set.
    fn restore_enabled(&self) -> bool {
        let t = &self.template;
        self.persist_allowed
            && !t.new_session
            && !t.no_daemon
            && t.attach_addr.is_none()
            && t.resolved.settings.tabs.restore
    }

    fn open_window(&mut self, el: &ActiveEventLoop, spec: &WindowSpec) {
        let mut app = self.new_app();
        app.open_window(el, spec);
        if let Some(id) = app.window_id() {
            self.focused.get_or_insert(id);
        }
        self.windows.push(app);
    }

    /// Remember every live window. Gated on the setting through the windows
    /// themselves, because a reload lands per window.
    fn persist_all(&self) {
        if !self.persist_allowed || !self.windows.iter().any(App::restore_enabled) {
            return;
        }
        let windows = self.windows.iter().filter_map(App::saved_window).collect();
        windows_state::save(&SavedWindows::new(windows));
    }

    fn close_window(&mut self, el: &ActiveEventLoop, i: usize) {
        let app = self.windows.remove(i);
        if let Some(code) = app.exit_code_raw() {
            self.exit_code = Some(code);
        }
        if self.persist_allowed && (app.restore_enabled() || self.windows.iter().any(App::restore_enabled)) {
            let remaining = self.windows.iter().filter_map(App::saved_window).collect();
            let snapshot = snapshot_after_close(remaining, app.saved_window());
            windows_state::save(&SavedWindows::new(snapshot));
        }
        if self.focused == app.window_id() {
            self.focused = self.windows.last().and_then(App::window_id);
        }
        // Dropping is the detach: every tab's session lets go of its daemon
        // handle here, and the in-process ones end with their pty.
        drop(app);
        if self.windows.is_empty() {
            el.exit();
        }
    }

    /// Act on what the windows asked for during the last dispatch.
    fn drain_requests(&mut self, el: &ActiveEventLoop) {
        let mut persist = false;
        let mut to_open = Vec::new();
        let mut to_close = Vec::new();
        for (i, w) in self.windows.iter_mut().enumerate() {
            let req = w.take_requests();
            persist |= req.persist;
            if req.new_window {
                to_open.push(i);
            }
            if req.close {
                to_close.push(i);
            }
        }
        if persist {
            self.persist_all();
        }
        if !to_open.is_empty() {
            let monitors = Self::monitors(el);
            // The spec is built before the new window is pushed, so the
            // borrow of its parent never overlaps the push.
            let specs: Vec<_> = to_open
                .iter()
                .map(|&i| {
                    let open = TabOpen::Shell { command: None, cwd: None };
                    WindowSpec::cascade_from(&self.windows[i], &monitors, open)
                })
                .collect();
            for spec in specs {
                self.open_window(el, &spec);
            }
        }
        // Highest first, so each index still names the window it did.
        for i in to_close.into_iter().rev() {
            self.close_window(el, i);
        }
    }
}

impl ApplicationHandler<Wakeup> for Process {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if !self.windows.is_empty() {
            return;
        }

        let specs: Vec<WindowSpec> = match self.restore_enabled().then(windows_state::load).flatten() {
            Some(saved) => {
                let monitors = Self::monitors(el);
                saved.windows.into_iter().map(|w| WindowSpec::restored(w, &monitors)).collect()
            }
            None => vec![WindowSpec::fresh()],
        };
        for spec in &specs {
            self.open_window(el, spec);
            // The probes measure the first window and leave from inside it.
            if el.exiting() {
                return;
            }
        }

        // The fleet view, off the measured path. The first window's own
        // daemon is synthesized into the listing from its signed Welcome —
        // a default daemon is mDNS-invisible, so discovery alone would omit
        // the one host that certainly exists.
        let first = &self.windows[0];
        let fleet = crate::fleet::FleetModel::start(self.proxy.clone(), first.local_host_label());
        if let Some(route) = first.route().cloned() {
            crate::app::watch_pairings(&fleet, route, Arc::clone(&self.shared.approval), &self.proxy);
        }
        let _ = self.shared.fleet.set(fleet);
        // `--screen fleet` showed the screen before the fleet model existed,
        // so its account watch found nothing to start; catch up now.
        for w in &mut self.windows {
            w.after_fleet_started();
        }

        // Last, and off the measured path: watching costs a thread and an
        // inotify/ReadDirectoryChanges handle, and none of it is needed to
        // show the first frame.
        self.watch_config();
        // Likewise the endpoint: claimed in `main`, served only now that the
        // first window has painted.
        if let Some(claim) = self.instance_claim.take() {
            self.instance = Some(instance::InstanceServer::start(
                claim,
                self.proxy.clone(),
                Arc::clone(&self.pending_opens),
            ));
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, event: Wakeup) {
        match route(&event) {
            // Every owner, not the first: adoption refuses a duplicate within
            // one strip, so the same session can be open in two windows, and
            // a tab's exit must mark it in both.
            Route::Owner(addr) => {
                let mut delivered = false;
                for w in self.windows.iter_mut().filter(|w| w.owns(addr)) {
                    w.handle_wakeup(event);
                    delivered = true;
                }
                if !delivered {
                    // Closed between the send and now; nothing to tell.
                    tracing::debug!(%addr, ?event, "wakeup for a session no window holds");
                }
            }
            Route::Broadcast => {
                for w in &mut self.windows {
                    w.handle_wakeup(event);
                }
            }
            Route::Fleet => {
                if self.shared.fleet.get().is_some_and(|f| f.take_changed()) {
                    for w in &mut self.windows {
                        w.mark_chrome_dirty();
                    }
                }
            }
            Route::Exit => el.exit(),
            Route::Open => self.open_requested(el),
        }
        self.drain_requests(el);
    }

    fn window_event(&mut self, el: &ActiveEventLoop, id: WindowId, event: WindowEvent) {
        if matches!(event, WindowEvent::Focused(true)) {
            self.focused = Some(id);
        }
        let Some(i) = self.index_of(id) else { return };
        self.windows[i].handle_window_event(event);
        self.drain_requests(el);
    }

    fn new_events(&mut self, _el: &ActiveEventLoop, cause: winit::event::StartCause) {
        for w in &mut self.windows {
            w.on_new_events(cause);
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let now = std::time::Instant::now();
        let mut wake = NextWake::Idle;
        for w in &mut self.windows {
            match w.next_wake(now) {
                // Drawn from here rather than asked for through the window,
                // because in screenshot mode there is no window to ask: it
                // is never made visible, so the OS never sends it a paint
                // (#255).
                NextWake::CaptureNow => w.redraw(),
                other => wake = merge_wakes(wake, other),
            }
        }
        match wake {
            NextWake::After(delay) => el.set_control_flow(ControlFlow::WaitUntil(now + delay)),
            NextWake::Idle | NextWake::CaptureNow => el.set_control_flow(ControlFlow::Wait),
        }
        // The PNG is written; that window leaves through the front door so
        // the pty, the clipboard and the tab state all get their `Drop`
        // rather than being cut off by `process::exit`. Checked *after* the
        // capture above, which is what sets it.
        for w in &mut self.windows {
            if w.exit_code_raw().is_some() {
                w.request_close();
            }
        }
        self.drain_requests(el);
    }

    /// The loop is ending with windows still open — ⌘Q on macOS, which never
    /// delivers a `CloseRequested`. Remember them exactly as closing the last
    /// one would have.
    fn exiting(&mut self, _el: &ActiveEventLoop) {
        if !self.windows.is_empty() {
            self.persist_all();
        }
    }
}

impl Process {
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use zest_proto::{HostId, SessionId};

    fn addr(n: u8) -> zest_proto::SessionAddr {
        zest_proto::SessionAddr { host: HostId::from_bytes([n; 32]), session: SessionId(u64::from(n)) }
    }

    #[test]
    fn a_wakeup_that_names_a_session_goes_to_the_window_holding_it() {
        for w in [
            Wakeup::TabExited(addr(1)),
            Wakeup::SessionGone(addr(1)),
            Wakeup::Attention(addr(1), zest_proto::AttentionCause::Bell),
        ] {
            assert_eq!(
                route(&w),
                Route::Owner(addr(1)),
                "{w:?} is about one session; delivering it to every window would close or mark a tab in each"
            );
        }
    }

    #[test]
    fn the_fleet_latch_is_consumed_once_and_the_exit_is_the_process_s() {
        assert_eq!(route(&Wakeup::FleetChanged), Route::Fleet, "take_changed clears a single flag; a broadcast would starve every window but the first");
        assert_eq!(route(&Wakeup::Exited), Route::Exit);
        assert_eq!(route(&Wakeup::OpenRequested), Route::Open, "a launch is the process's to place");
    }

    #[test]
    fn everything_else_reaches_every_window() {
        for w in [
            Wakeup::Redraw,
            Wakeup::Detached,
            Wakeup::Reattached,
            Wakeup::ConfigChanged,
            Wakeup::AccountChanged,
            Wakeup::TabsChanged,
            Wakeup::PairingChanged,
            Wakeup::DirListingReady,
            Wakeup::FileContentsReady,
            Wakeup::SignalChanged,
        ] {
            assert_eq!(route(&w), Route::Broadcast, "{w:?} carries no address; each window's own arm decides whether it matters");
        }
    }

    #[test]
    fn the_earliest_wake_wins_and_idle_windows_add_nothing() {
        let a = NextWake::After(Duration::from_millis(500));
        let b = NextWake::After(Duration::from_millis(16));
        assert_eq!(merge_wakes(a, b), b, "a blinking cursor in one window must not wait on a slower animation in another");
        assert_eq!(merge_wakes(NextWake::Idle, a), a);
        assert_eq!(merge_wakes(a, NextWake::Idle), a);
        assert_eq!(merge_wakes(NextWake::Idle, NextWake::Idle), NextWake::Idle, "a process of resting windows schedules nothing — the 0%-idle guarantee");
        assert_eq!(merge_wakes(NextWake::CaptureNow, a), NextWake::CaptureNow, "a due capture is not a wait");
        assert_eq!(merge_wakes(NextWake::Idle, NextWake::CaptureNow), NextWake::CaptureNow);
    }

    fn saved(n: u8) -> SavedWindow {
        SavedWindow {
            active: 0,
            tabs: vec![SavedTab { addr: addr(n), local: true, dial_hint: None, title: String::new() }],
            geometry: Geometry::default(),
        }
    }

    #[test]
    fn closing_one_window_of_several_forgets_it() {
        let after = snapshot_after_close(vec![saved(1), saved(2)], Some(saved(3)));
        assert_eq!(after.iter().map(|w| w.tabs[0].addr).collect::<Vec<_>>(), vec![addr(1), addr(2)], "the user closed it; reopening it next launch would undo that");
    }

    #[test]
    fn closing_the_last_window_remembers_it() {
        let after = snapshot_after_close(vec![], Some(saved(3)));
        assert_eq!(after.len(), 1, "closing the last window is quitting, and what it showed is what comes back");
        assert_eq!(after[0].tabs[0].addr, addr(3));
        assert!(snapshot_after_close(vec![], None).is_empty());
    }

    #[test]
    fn placeholders_never_repeat_across_windows() {
        // Placeholder addresses key wakeup routing: two windows minting the
        // same one would deliver a tab's exit to the wrong window. One
        // counter for the process, not one per window.
        let shared = Shared::new();
        let a: Vec<u64> = (0..3).map(|_| shared.mint_placeholder()).collect();
        let b: Vec<u64> = (0..3).map(|_| shared.mint_placeholder()).collect();
        assert!(a.iter().all(|x| !b.contains(x)), "{a:?} and {b:?} share a number");
        assert!(!a.contains(&0), "0 is the app tabs' sentinel and is never minted");
    }
}

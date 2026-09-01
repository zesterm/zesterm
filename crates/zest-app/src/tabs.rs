//! Tabs: the sessions this window has attached.
//!
//! A tab is a *name for one session*, keyed by `SessionAddr` from birth —
//! fleet-shaped even while every session is on this machine, because
//! retrofitting the key means retouching every hit region and every wakeup
//! (#23). The strip deliberately does not list the fleet; the picker does.
//! What lives here is only what this window holds open.
//!
//! Sits beside the frozen `SessionSource`/`Origin` contract rather than
//! changing it: `Origin` cannot give a tab its key back (it carries a display
//! string, not a `HostId`), so the tab owns the key and the trait stays as
//! the renderer's read surface.

use zest_config::{ColorFrom, TabTitle};
use zest_proto::{HostId, SessionAddr, SessionId};

use crate::remote::RemoteSession;
use crate::session::Session;
use crate::source::SessionSource;

/// The appearance half of the profile a tab was launched from, resolved.
///
/// Carried on the tab rather than looked up per frame because a profile can
/// be renamed or deleted while its sessions live on — the tab keeps the look
/// it launched with until a config reload re-resolves it by name. Per §12's
/// chrome-vs-grid rule the scheme applies to the grid only; the chrome's one
/// concession is the accent and glyph.
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileIdentity {
    /// The profile's name — the key a config reload re-resolves by.
    pub name: String,
    /// Colour scheme id: the ANSI half of a theme, for this tab's grid.
    pub scheme: Option<String>,
    /// The scheme's selection wash, resolved *here* rather than per frame.
    /// `None` means no scheme (or one that no longer exists): follow the
    /// window's. The redraw path reads this field per pane per frame, so
    /// resolving there made a deleted scheme warn on every caret blink —
    /// unbounded log growth for a non-event — and charged a full theme
    /// resolve + allocation per pane per frame for a valid one.
    pub selection_bg: Option<zest_core::Rgb>,
    /// Index into the theme's accents for the chip's rule and glyph tile.
    pub tab_color: Option<u8>,
    /// Glyph for the tab's icon tile.
    pub icon: Option<String>,
    /// Whether the accent is the profile's own or its host's.
    pub color_from: Option<ColorFrom>,
    /// `window.opacity` override, riding this tab's viewport only.
    pub opacity: Option<f32>,
    /// `window.background_image` override: this profile's own picture.
    ///
    /// The three background keys are separate `Option`s rather than one bundle
    /// because the cascade resolves them separately — a profile that sets only
    /// `background_dim` dims the window's picture, which is what "overrides
    /// Defaults" means on a per-row editor.
    pub background_image: Option<String>,
    pub background_fit: Option<zest_config::settings::BackgroundFit>,
    pub background_dim: Option<f32>,
    /// Where the tab's title comes from.
    pub title: TabTitle,
}

impl ProfileIdentity {
    /// Resolve a profile's appearance identity through Defaults.
    ///
    /// Goes through `resolve_profile` so inheritance cannot drift from what
    /// the profiles editor shows; a name that no longer exists resolves as
    /// empty-over-Defaults rather than failing (the never-crash rule).
    #[must_use]
    pub fn resolve(settings: &zest_config::Settings, name: &str) -> Self {
        let mut profiles = toml::Table::new();
        for (key, table) in &settings.profiles {
            profiles.insert(key.clone(), toml::Value::Table(table.clone()));
        }
        let mut root = toml::Table::new();
        root.insert("profiles".into(), toml::Value::Table(profiles));
        let resolved = zest_config::profiles::resolve_profile(&root, name);

        // Lenient like the rest of the profile keys: a wrong-typed opacity is
        // ignored, an integer one (`opacity = 1`) is accepted — TOML has no
        // float coercion, and `1` is how a hand edit spells "opaque".
        let window_key = |key: &str| {
            resolved
                .overrides
                .get("window")
                .and_then(toml::Value::as_table)
                .and_then(|w| w.get(key))
                .cloned()
        };
        // TOML admits `nan`, and clamp preserves it — a non-finite value must
        // degrade to None (the window's own), not ride NaN into the render
        // path, where it is a quad at infinity rather than a wrong pixel.
        let unit = |key: &str| {
            window_key(key)
                .and_then(|v| match v {
                    toml::Value::Float(f) => Some(f as f32),
                    toml::Value::Integer(i) => Some(i as f32),
                    _ => None,
                })
                .filter(|o: &f32| o.is_finite())
                .map(|o| o.clamp(0.0, 1.0))
        };

        let opacity = unit("opacity");
        let background_dim = unit("background_dim");
        // An empty string is how the settings form spells "no picture", so a
        // profile may use it to *turn off* one Defaults set — which is why it
        // stays `Some("")` rather than collapsing to `None`.
        let background_image =
            window_key("background_image").and_then(|v| v.as_str().map(str::to_string));
        let background_fit = window_key("background_fit")
            .and_then(|v| v.as_str().map(str::to_string))
            .and_then(|name| match name.as_str() {
                "fill" => Some(zest_config::settings::BackgroundFit::Fill),
                "fit" => Some(zest_config::settings::BackgroundFit::Fit),
                "watermark" => Some(zest_config::settings::BackgroundFit::Watermark),
                _ => None,
            });

        let scheme = resolved.meta.color_scheme;
        Self {
            name: name.to_string(),
            selection_bg: scheme.as_deref().and_then(scheme_selection_wash),
            scheme,
            tab_color: resolved.meta.tab_color,
            icon: resolved.meta.icon,
            color_from: resolved.meta.color_from,
            opacity,
            background_image,
            background_fit,
            background_dim,
            title: resolved.meta.tab_title,
        }
    }

    /// The identity of a profile *another machine* published (#268).
    ///
    /// Built from what came over the wire rather than resolved against this
    /// machine's config, and that is the whole point: the far host has already
    /// folded its own `profiles.defaults` in, and re-resolving the name here
    /// would apply *our* `defaults` to *their* profile — a `nightly` on the
    /// build box silently inheriting this laptop's command.
    ///
    /// Two fields have no wire form and take their local meaning: `opacity`
    /// and `title` are window-side decisions (§12 keeps window size and
    /// padding off profiles for the same reason), so a published profile
    /// follows this window's.
    #[must_use]
    pub fn from_published(profile: &zest_proto::HostProfile) -> Self {
        let scheme = (!profile.color_scheme.is_empty()).then(|| profile.color_scheme.clone());
        Self {
            name: profile.name.clone(),
            selection_bg: scheme.as_deref().and_then(scheme_selection_wash),
            scheme,
            tab_color: profile.tab_color,
            icon: (!profile.icon.is_empty()).then(|| profile.icon.clone()),
            // Never `Host`: that choice is `profiles.defaults.color_from` on
            // *this* machine, a preference about how the fleet reads to the
            // person looking at it, not something the far host gets a vote on.
            color_from: None,
            opacity: None,
            // Window-side too, and for the same reason: the file named by a far
            // host's `background_image` is a path on *that* machine, which this
            // one cannot read (#20's trap, one layer up).
            background_image: None,
            background_fit: None,
            background_dim: None,
            title: TabTitle::default(),
        }
    }
}

/// A colour scheme id resolved to its palette — `None` for a name that does
/// not exist. Unknown warns and falls back rather than failing (the
/// never-crash rule): a deleted scheme must not take a running session's
/// window down, its tab just follows the window again. Called at identity
/// (re-)resolve and terminal (re-)seed time only — never per frame, so the
/// warn fires once per transition instead of once per redraw.
pub(crate) fn resolve_scheme(scheme: &str) -> Option<zest_theme::ResolvedPalette> {
    match crate::themes::get(scheme) {
        Some(theme) => Some(zest_theme::resolve(&theme)),
        None => {
            tracing::warn!(scheme, "unknown colour scheme; the tab follows the window palette");
            None
        }
    }
}

/// The scheme's selection wash — the one colour the per-frame render path
/// needs, extracted here so it can be cached on the identity.
pub(crate) fn scheme_selection_wash(scheme: &str) -> Option<zest_core::Rgb> {
    let r = resolve_scheme(scheme)?;
    Some(zest_core::Rgb::new(r.selection_bg.r, r.selection_bg.g, r.selection_bg.b))
}

/// Where a tab's terminal actually lives.
///
/// An enum rather than `Box<dyn SessionSource>` because closing needs the
/// concrete type: `RemoteSession::kill` consumes self to guarantee the
/// CloseSession frame is flushed, and no object-safe trait method can do
/// that.
pub enum TabSession {
    Daemon(RemoteSession),
    InProcess(Session),
    /// A launch worker is still dialling the host (design §12): the tab is
    /// already in the strip, showing a provenance line, so a cold host costs
    /// the user a placeholder rather than a frozen event loop or a silent
    /// `warn!` in a log nobody reads (issue #175).
    Pending(PendingSession),
}

/// The pane behind a connecting tab: a real local [`Terminal`] holding one
/// provenance line, so the renderer needs no special case. Keystrokes are
/// dropped — there is nothing to type into yet, and buffering them would
/// replay half-considered input into a shell that arrives seconds later
/// (the same reasoning as the reconnect path's resize-only replay).
pub struct PendingSession {
    terminal: std::sync::Arc<crate::fair_mutex::FairMutex<zest_core::Terminal>>,
    dirty: std::sync::atomic::AtomicBool,
    origin: crate::source::Origin,
}

impl PendingSession {
    /// An empty placeholder, for tests about a tab's fields rather than its
    /// pane.
    #[cfg(test)]
    pub(crate) fn blank() -> Self {
        Self {
            terminal: std::sync::Arc::new(crate::fair_mutex::FairMutex::new(
                zest_core::Terminal::new(80, 24, 0),
            )),
            dirty: std::sync::atomic::AtomicBool::new(false),
            origin: crate::source::Origin::InProcess,
        }
    }

    /// Build the placeholder pane: profile palette seeded first (the grid
    /// must never flash the window's scheme under a profile's), the profile
    /// name as the title, and the provenance line in the scheme's dim colour
    /// (SGR 2 — the palette decides what "dim" looks like).
    #[must_use]
    pub fn new(
        cols: u16,
        rows: u16,
        seed: zest_core::PaletteSnapshot,
        title: &str,
        provenance: &str,
        host_label: &str,
    ) -> Self {
        let mut term = zest_core::Terminal::new(usize::from(cols), usize::from(rows), 0);
        term.set_palette(seed);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\x1b]2;");
        bytes.extend_from_slice(sanitize(title).as_bytes());
        bytes.extend_from_slice(b"\x07\x1b[2m");
        bytes.extend_from_slice(sanitize(provenance).as_bytes());
        bytes.extend_from_slice(b"\x1b[0m\r\n");
        term.advance(&bytes);
        Self {
            terminal: std::sync::Arc::new(crate::fair_mutex::FairMutex::new(term)),
            dirty: std::sync::atomic::AtomicBool::new(true),
            origin: crate::source::Origin::Daemon { host: host_label.to_string(), local: false },
        }
    }

    /// The worker gave up: the error joins the provenance line, in danger
    /// ink, so the dead tab *carries* its reason instead of pointing at a
    /// log.
    fn show_error(&self, error: &str) {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"\x1b[31mcould not open the session: ");
        bytes.extend_from_slice(sanitize(error).as_bytes());
        bytes.extend_from_slice(b"\x1b[0m\r\n");
        self.terminal.lock().advance(&bytes);
        self.dirty.store(true, std::sync::atomic::Ordering::Release);
    }
}

/// Strip control bytes before feeding externally-sourced text to the pane's
/// terminal — an error message carrying a stray escape would repaint or
/// retitle the very pane trying to report it (the raw-VT trap from the test
/// guide, applied to the UI).
fn sanitize(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

impl crate::source::SessionSource for PendingSession {
    fn terminal(&self) -> &std::sync::Arc<crate::fair_mutex::FairMutex<zest_core::Terminal>> {
        &self.terminal
    }

    fn write(&self, _bytes: Vec<u8>) {
        // Dropped on purpose; see the struct doc.
    }

    fn resize(&self, cols: u16, rows: u16) {
        self.terminal.lock().resize(usize::from(cols), usize::from(rows));
        self.dirty.store(true, std::sync::atomic::Ordering::Release);
    }

    fn take_dirty(&self) -> bool {
        self.dirty.swap(false, std::sync::atomic::Ordering::AcqRel)
    }

    fn mark_dirty(&self) {
        self.dirty.store(true, std::sync::atomic::Ordering::Release);
    }

    fn origin(&self) -> crate::source::Origin {
        self.origin.clone()
    }
}

pub struct Tab {
    pub addr: SessionAddr,
    session: TabSession,
    /// The panes split right of the primary, left to right (design screen
    /// 5, generalised — #436). Any number, on any host: a pane is a session
    /// like the tab's own, and a session is addressed `(HostId, SessionId)`
    /// wherever it runs. An unsplit tab — every tab, most of the time —
    /// pays an empty `Vec`, which allocates nothing.
    pub panes: Vec<SplitPane>,
    /// Which pane owns the keyboard: `0` is the primary, `i` is `panes[i-1]`.
    pub focus: usize,
    /// The session runs on this machine (loopback daemon or in-process).
    /// Decides close semantics: closing a local tab kills; a remote one
    /// detaches.
    pub local: bool,
    /// `Wakeup::SessionGone` arrived: the host answered and the session no
    /// longer exists. The tab stays put saying so rather than vanishing —
    /// or worse, silently becoming a different shell.
    pub dead: bool,
    /// A launch worker is still dialling this tab's host. The chip borrows
    /// the connecting style and the pane shows the provenance line; resolved
    /// by [`Tab::resolve_live`] or [`Tab::resolve_failed`], never both.
    pub connecting: bool,
    /// The last `(cols, rows)` this tab's pty was told. Background tabs are
    /// resized lazily on activation, so a window drag costs one resize for
    /// the visible grid instead of N network messages per frame.
    pub sized: (u16, u16),
    /// How this tab's host was dialled when it was not the local socket —
    /// persisted so a restore can reach the host even before discovery has
    /// found it again.
    pub dial_hint: Option<String>,
    /// The profile this tab was launched from, when it was launched from one.
    /// `None` is every plain tab: it follows the window's palette and accent.
    pub identity: Option<ProfileIdentity>,
    /// Bytes waiting for this tab's session to exist (#324) — see
    /// [`Tab::with_pending_input`].
    pending_input: Option<Vec<u8>>,
}

/// What a split pane holds (#464).
///
/// Only a *split* pane may be a non-session: `Tab.session` keeps its type, so
/// pane 0 is always a shell and a tab is still named by one. That is what
/// keeps `close_focused_pane`'s promotion representable and stops a tab
/// existing with no terminal in it at all.
pub enum PaneContent {
    Session(TabSession),
    /// A file, open for reading. It has no terminal, so every path that
    /// reaches for one has to ask first — which is the whole cost of this
    /// enum, and is paid by the compiler rather than at runtime.
    Editor(Box<crate::editor::EditorPane>),
}

/// One extra pane of a split tab: a session *or a file*, with the little state
/// a pane needs and none of a tab's (a pane is not a hit target in the strip,
/// does not persist, and cannot be dragged).
pub struct SplitPane {
    pub addr: SessionAddr,
    content: PaneContent,
    pub local: bool,
    pub dead: bool,
    /// A worker is still dialling this pane's host — the same treatment a
    /// connecting tab gets (#175), because a pane on a cold host must cost a
    /// placeholder and never a frozen event loop.
    pub connecting: bool,
    pub sized: (u16, u16),
}

impl SplitPane {
    pub fn daemon(remote: RemoteSession, local: bool, sized: (u16, u16)) -> Self {
        Self {
            addr: remote.addr(),
            content: PaneContent::Session(TabSession::Daemon(remote)),
            local,
            dead: false,
            connecting: false,
            sized,
        }
    }

    pub fn in_process(session: Session, addr: SessionAddr, sized: (u16, u16)) -> Self {
        Self {
            addr,
            content: PaneContent::Session(TabSession::InProcess(session)),
            local: true,
            dead: false,
            connecting: false,
            sized,
        }
    }

    /// A pane whose session a worker is still opening, under a placeholder
    /// address; settled by [`SplitPane::resolve_live`] /
    /// [`SplitPane::resolve_failed`].
    pub fn connecting(addr: SessionAddr, pending: PendingSession, sized: (u16, u16)) -> Self {
        Self {
            addr,
            content: PaneContent::Session(TabSession::Pending(pending)),
            // Remote until proven otherwise: closing a connecting pane must
            // never kill anything, because there is nothing of ours to kill.
            local: false,
            dead: false,
            connecting: true,
            sized,
        }
    }

    /// The worker's session arrived: swap it in under the same frame.
    pub fn resolve_live(&mut self, remote: RemoteSession, local: bool) {
        self.addr = remote.addr();
        self.content = PaneContent::Session(TabSession::Daemon(remote));
        self.local = local;
        self.connecting = false;
    }

    /// The worker gave up: the pane stays, dead, carrying the error.
    pub fn resolve_failed(&mut self, error: &str) {
        self.connecting = false;
        self.dead = true;
        if let PaneContent::Session(TabSession::Pending(p)) = &self.content {
            p.show_error(error);
        }
    }

    #[cfg(test)]
    pub(crate) fn pending_for_test(addr: SessionAddr) -> Self {
        Self::connecting(addr, PendingSession::blank(), (80, 24))
    }

    /// A pane holding a file (#464), under a placeholder address so it is a
    /// distinct hit region and persistence skips it.
    pub fn editor(editor: crate::editor::EditorPane) -> Self {
        Self {
            addr: editor.addr,
            content: PaneContent::Editor(Box::new(editor)),
            // Not local, so `Tab::kill` and `close_focused_pane` do not try to
            // end something that was never started. A file has nothing to kill.
            local: false,
            dead: false,
            connecting: false,
            sized: (0, 0),
        }
    }

    /// This pane's terminal, or `None` when it holds a file.
    pub fn session(&self) -> Option<&dyn SessionSource> {
        match &self.content {
            PaneContent::Session(TabSession::Daemon(r)) => Some(r),
            PaneContent::Session(TabSession::InProcess(s)) => Some(s),
            PaneContent::Session(TabSession::Pending(p)) => Some(p),
            PaneContent::Editor(_) => None,
        }
    }

    #[must_use]
    pub fn is_session(&self) -> bool {
        matches!(self.content, PaneContent::Session(_))
    }

    #[must_use]
    pub fn editor_ref(&self) -> Option<&crate::editor::EditorPane> {
        match &self.content {
            PaneContent::Editor(e) => Some(e),
            PaneContent::Session(_) => None,
        }
    }

    pub fn editor_mut(&mut self) -> Option<&mut crate::editor::EditorPane> {
        match &mut self.content {
            PaneContent::Editor(e) => Some(e),
            PaneContent::Session(_) => None,
        }
    }

    /// The session this pane holds, taken — for the promotion in
    /// [`Tab::close_focused_pane`], which can only put a session in a tab.
    fn into_session(self) -> Option<TabSession> {
        match self.content {
            PaneContent::Session(s) => Some(s),
            PaneContent::Editor(_) => None,
        }
    }

    /// End the pane's session for good (local close); dropping detaches.
    pub fn kill(self) {
        match self.content {
            PaneContent::Session(TabSession::Daemon(r)) => r.kill(),
            PaneContent::Session(TabSession::InProcess(s)) => drop(s),
            // Nothing exists to kill; a session the worker later delivers
            // for a closed pane is dropped by the resolution path.
            PaneContent::Session(TabSession::Pending(p)) => drop(p),
            PaneContent::Editor(e) => drop(e),
        }
    }
}

impl Tab {
    pub fn daemon(remote: RemoteSession, local: bool, sized: (u16, u16)) -> Self {
        Self {
            addr: remote.addr(),
            session: TabSession::Daemon(remote),
            panes: Vec::new(),
            focus: 0,
            local,
            dead: false,
            connecting: false,
            sized,
            dial_hint: None,
            pending_input: None,
            identity: None,
        }
    }

    /// A tab whose session is still being opened by a worker: pushed
    /// immediately under a placeholder address so the launch is visible from
    /// its first frame, then settled by [`Tab::resolve_live`] /
    /// [`Tab::resolve_failed`] when the dial finishes (issue #175).
    pub fn connecting(addr: SessionAddr, pending: PendingSession, sized: (u16, u16)) -> Self {
        Self {
            addr,
            session: TabSession::Pending(pending),
            panes: Vec::new(),
            focus: 0,
            // Remote until proven otherwise: closing a connecting tab must
            // never kill anything, because there is nothing of ours to kill.
            local: false,
            dead: false,
            connecting: true,
            sized,
            dial_hint: None,
            pending_input: None,
            identity: None,
        }
    }

    /// The worker's session arrived: swap it in under the same strip slot.
    /// The tab's address becomes real here — every hit region and wakeup
    /// keyed on the placeholder is re-keyed by the caller reading `addr`.
    pub fn resolve_live(&mut self, remote: RemoteSession, local: bool) {
        self.addr = remote.addr();
        self.session = TabSession::Daemon(remote);
        self.local = local;
        self.connecting = false;
    }

    /// The worker gave up: the existing dead-tab treatment, carrying the
    /// error in the pane rather than in a log (issue #175's whole point).
    pub fn resolve_failed(&mut self, error: &str) {
        self.connecting = false;
        self.dead = true;
        if let TabSession::Pending(p) = &self.session {
            p.show_error(error);
        }
    }

    #[must_use]
    pub fn with_dial_hint(mut self, hint: Option<String>) -> Self {
        self.dial_hint = hint;
        self
    }

    /// Bytes to write the moment this tab joins the strip (#324).
    ///
    /// "Run this command on that machine" cannot happen at click time: the
    /// session does not exist yet and the dial is on a worker. So the command
    /// rides the tab the worker builds and is written when the event loop
    /// adopts it — keyed to *this* tab, never to whatever happens to be active
    /// when the dial lands.
    #[must_use]
    pub fn with_pending_input(mut self, input: Option<Vec<u8>>) -> Self {
        self.pending_input = input;
        self
    }

    /// Take the armed bytes, if any. Taking is the point: a command runs once.
    pub fn take_pending_input(&mut self) -> Option<Vec<u8>> {
        self.pending_input.take()
    }

    /// A tab with no session behind it, for tests that only exercise the
    /// fields — `connecting` needs a `PendingSession`, which needs a palette
    /// snapshot and a rendered pane nothing here is about.
    #[cfg(test)]
    pub(crate) fn pending_for_test(addr: SessionAddr) -> Self {
        Self {
            addr,
            session: TabSession::Pending(PendingSession::blank()),
            panes: Vec::new(),
            focus: 0,
            local: false,
            dead: false,
            connecting: true,
            sized: (80, 24),
            dial_hint: None,
            identity: None,
            pending_input: None,
        }
    }

    #[must_use]
    pub fn with_identity(mut self, identity: Option<ProfileIdentity>) -> Self {
        self.identity = identity;
        self
    }

    pub fn in_process(session: Session, addr: SessionAddr, sized: (u16, u16)) -> Self {
        Self {
            addr,
            session: TabSession::InProcess(session),
            panes: Vec::new(),
            focus: 0,
            local: true,
            dead: false,
            connecting: false,
            sized,
            dial_hint: None,
            pending_input: None,
            identity: None,
        }
    }

    pub fn source(&self) -> &dyn SessionSource {
        match &self.session {
            TabSession::Daemon(r) => r,
            TabSession::InProcess(s) => s,
            TabSession::Pending(p) => p,
        }
    }

    /// The pane the keyboard belongs to — what input, selection, IME and the
    /// status bar all act on. The primary pane unless a split holds focus.
    ///
    /// `None` when that pane holds a file rather than a shell (#464). Every
    /// caller is a session question — a block, a selection, a cwd probe — so
    /// the honest answer for a file pane is "there is nothing to ask".
    pub fn focused_session(&self) -> Option<&dyn SessionSource> {
        self.pane_session(self.focus)
    }

    /// The focused pane's session address.
    #[must_use]
    pub fn focused_addr(&self) -> SessionAddr {
        self.pane_addr(self.focus)
    }

    /// More than one pane.
    #[must_use]
    pub fn is_split(&self) -> bool {
        !self.panes.is_empty()
    }

    /// Panes, the primary included.
    #[must_use]
    pub fn pane_count(&self) -> usize {
        1 + self.panes.len()
    }

    /// Pane `i`'s terminal; `0` is the primary. Out of range clamps to the
    /// primary rather than panicking — a stale hit region or a focus index
    /// a frame behind a pane close must draw something, never crash.
    ///
    /// `None` only for a pane holding a file: index 0 is `Tab.session`, which
    /// is a session by construction.
    pub fn pane_session(&self, i: usize) -> Option<&dyn SessionSource> {
        match i.checked_sub(1).and_then(|j| self.panes.get(j)) {
            Some(pane) => pane.session(),
            None => Some(self.source()),
        }
    }

    /// Pane `i`'s open file, if it holds one.
    #[must_use]
    pub fn pane_editor(&self, i: usize) -> Option<&crate::editor::EditorPane> {
        i.checked_sub(1).and_then(|j| self.panes.get(j)).and_then(SplitPane::editor_ref)
    }

    pub fn pane_editor_mut(&mut self, i: usize) -> Option<&mut crate::editor::EditorPane> {
        i.checked_sub(1).and_then(|j| self.panes.get_mut(j)).and_then(SplitPane::editor_mut)
    }

    /// Every open file in this tab, with the pane index each sits in.
    pub fn editors_mut(&mut self) -> impl Iterator<Item = (usize, &mut crate::editor::EditorPane)> {
        self.panes
            .iter_mut()
            .enumerate()
            .filter_map(|(j, p)| p.editor_mut().map(|e| (j + 1, e)))
    }

    /// Any pane of this tab holds something that is not a shell — the question
    /// `refresh_chrome` asks before deciding it has no chrome to build.
    #[must_use]
    pub fn has_editor_pane(&self) -> bool {
        self.panes.iter().any(|p| !p.is_session())
    }

    #[must_use]
    pub fn pane_addr(&self, i: usize) -> SessionAddr {
        i.checked_sub(1).and_then(|j| self.panes.get(j)).map_or(self.addr, |p| p.addr)
    }

    /// Whether pane `i`'s shell has ended. A pane holding a file is never
    /// dead: nothing of its was running.
    #[must_use]
    pub fn pane_dead(&self, i: usize) -> bool {
        i.checked_sub(1).and_then(|j| self.panes.get(j)).map_or(self.dead, |p| p.dead)
    }

    /// Move the keyboard to pane `i`; `false` when nothing changed.
    pub fn focus_pane(&mut self, i: usize) -> bool {
        if i >= self.pane_count() || i == self.focus {
            return false;
        }
        self.focus = i;
        true
    }

    /// Move the keyboard one pane right (`+1`) or left (`-1`), wrapping —
    /// so the chord stays useful however many panes there are.
    pub fn cycle_focus(&mut self, delta: isize) -> bool {
        let n = self.pane_count();
        if n < 2 {
            return false;
        }
        let next = (self.focus as isize + delta).rem_euclid(n as isize) as usize;
        self.focus_pane(next)
    }

    /// Close the focused pane of a split tab; `false` when there is no
    /// split, in which case closing means the whole tab.
    ///
    /// Closing the *primary* promotes the next pane into the tab, so the
    /// tab keeps its identity in the strip while the surviving shells keep
    /// running — the alternative (the tab vanishing while a pane lives) is
    /// how sessions get orphaned. Otherwise the keyboard lands on the pane
    /// to the left of the one that closed, which is where the eye already is.
    pub fn close_focused_pane(&mut self) -> bool {
        if self.panes.is_empty() {
            return false;
        }
        if self.focus == 0 {
            // The promotion has to find a pane that *is* a session: a tab is
            // named by its shell and an editor cannot become one (#464). With
            // only files left there is nothing to promote, so this answers
            // `false` and the caller closes the whole tab — which takes the
            // files with it, and is what closing the last shell should mean.
            let Some(j) = self.panes.iter().position(SplitPane::is_session) else {
                return false;
            };
            let next = self.panes.remove(j);
            let next_addr = next.addr;
            let (next_local, next_dead, next_connecting, next_sized) =
                (next.local, next.dead, next.connecting, next.sized);
            let Some(session) = next.into_session() else {
                unreachable!("the pane was chosen by `is_session`")
            };
            let was_local = self.local;
            let old = core::mem::replace(&mut self.session, session);
            self.addr = next_addr;
            self.local = next_local;
            self.dead = next_dead;
            self.connecting = next_connecting;
            self.sized = next_sized;
            // Removing pane `j` shifts everything after it down one; focus is
            // 0 either way, so only a `j` past the focus could matter and
            // there is none.
            if was_local {
                match old {
                    TabSession::Daemon(r) => r.kill(),
                    TabSession::InProcess(s) => drop(s),
                    TabSession::Pending(p) => drop(p),
                }
            }
        } else {
            let pane = self.panes.remove(self.focus - 1);
            self.focus -= 1;
            if pane.local {
                pane.kill();
            }
            // A remote pane's drop detaches, exactly like a remote tab.
        }
        true
    }

    /// Drop the pane holding `addr` because its shell has ended — nothing
    /// to kill; the keyboard stays with its pane, or moves one left when
    /// that pane was the one that went. `false` when no split pane holds it
    /// (the primary's exit is the tab's, not a pane's).
    pub fn remove_gone_pane(&mut self, addr: SessionAddr) -> bool {
        let Some(j) = self.panes.iter().position(|p| p.addr == addr) else { return false };
        drop(self.panes.remove(j));
        if self.focus > j {
            self.focus -= 1;
        }
        true
    }

    /// End this tab's session for good.
    ///
    /// Only meaningful for daemon sessions — an in-process pty dies with its
    /// `Session` drop regardless, which is also what a dead tab needs. A
    /// split pane follows its tab's fate: local panes die, remote detach.
    pub fn kill(self) {
        for pane in self.panes {
            if pane.local {
                pane.kill();
            }
        }
        match self.session {
            TabSession::Daemon(r) => r.kill(),
            TabSession::InProcess(s) => drop(s),
            // Nothing exists to kill; a session the worker later delivers
            // for a closed tab is dropped by the resolution path (drop
            // detaches, the daemon keeps the shell).
            TabSession::Pending(p) => drop(p),
        }
    }
}

/// A placeholder address for sessions that have no real one — the in-process
/// fallback, and the instant between asking a daemon for a session and
/// learning its address. All-zero `HostId`, which no real host can have (it
/// is a key fingerprint), plus a locally unique counter so two placeholder
/// tabs stay distinct hit regions.
#[must_use]
pub fn placeholder_addr(n: u64) -> SessionAddr {
    // The top two ids on the all-zero host are the app tabs' sentinels
    // ([`settings_addr`], [`profiles_tab_addr`]); the counter cannot reach
    // them in any real session, so a collision here is a bug in whatever
    // minted `n`, not bad luck — and a release build that hit one would
    // silently activate or close the wrong tab (every hit region keys on the
    // address), which is why this is not a `debug_assert`. Zero is legal but
    // reserved: it is the "no id yet" address a wakeup carries until
    // `wake_for` stamps the real one, so the ids minted for placeholder tabs
    // count up from 1.
    assert!(
        n < u64::MAX - 1,
        "placeholder ids must never reach the app-tab sentinels"
    );
    SessionAddr::new(HostId::from_bytes([0; 32]), SessionId(n))
}

/// Whether an address is a placeholder rather than a session in the fleet.
#[must_use]
#[allow(dead_code, reason = "the picker and persistence skip placeholder tabs, next in #23")]
pub fn is_placeholder(addr: SessionAddr) -> bool {
    is_placeholder_host(addr.host)
}

/// The host half of the same fact, for callers holding a `TabOrigin` rather
/// than an address: an all-zero id is "no id yet", never a machine.
#[must_use]
pub fn is_placeholder_host(host: HostId) -> bool {
    host == HostId::from_bytes([0; 32])
}

/// The Settings tab's address (design §11): an app tab is a place, not a
/// shell, but the strip's hit machinery is keyed by `SessionAddr` and staying
/// inside that key is what keeps this change from retouching every hit
/// region (#23's lesson). All-zero host — no real host, it is a key
/// fingerprint — plus a `SessionId` the placeholder counter can never
/// count up to.
#[must_use]
pub fn settings_addr() -> SessionAddr {
    SessionAddr::new(HostId::from_bytes([0; 32]), SessionId(u64::MAX))
}

/// The Profiles chip's address (design §12), one below Settings' — the top
/// two ids on the all-zero host are the app tabs', reserved as a pair so the
/// two chips can never collide, and real placeholder ids count up from 1 so
/// neither is reachable by opening tabs.
#[must_use]
pub fn profiles_tab_addr() -> SessionAddr {
    SessionAddr::new(HostId::from_bytes([0; 32]), SessionId(u64::MAX - 1))
}

/// The window's Profiles tab (design §12): a place, not a shell, as a
/// placeholder pane until its work item lands. Settings has its own strip
/// machinery (§11, landed with #172); Profiles keeps this thinner shape
/// until the editor replaces the placeholder.
///
/// At most one — the singleton rule (`⌘⇧,` on an already-open Profiles tab
/// activates it rather than opening a second; the web client's
/// `openSingleton` pins the same rule). A `bool` makes duplication
/// unrepresentable rather than merely checked.
#[derive(Default)]
pub struct AppTabs {
    profiles: bool,
}

impl AppTabs {
    /// Open the Profiles tab. `false` means it already existed — the reopen
    /// is then an activation, which the caller performs by showing it.
    pub fn open_profiles(&mut self) -> bool {
        !core::mem::replace(&mut self.profiles, true)
    }

    #[must_use]
    pub fn profiles_open(&self) -> bool {
        self.profiles
    }

    pub fn close_profiles(&mut self) {
        self.profiles = false;
    }
}

/// The window's open tabs, and which one the keyboard belongs to.
///
/// Beside the session tabs the strip can hold one *app tab* — Settings
/// (design §11) — which is a place rather than a shell: it has no session,
/// so it lives as two flags instead of a `Tab`. `active` always names a
/// session tab; while `settings_active` the keyboard belongs to the
/// Settings tab and the session keeps its slot to return to on close.
#[derive(Default)]
pub struct TabStrip {
    tabs: Vec<Tab>,
    active: usize,
    /// The Settings tab exists (at most one, per §11).
    settings_open: bool,
    /// ...and holds the keyboard/display.
    settings_active: bool,
}

impl TabStrip {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.tabs.is_empty()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.tabs.len()
    }

    #[must_use]
    pub fn active_index(&self) -> usize {
        self.active
    }

    #[must_use]
    pub fn active(&self) -> Option<&Tab> {
        self.tabs.get(self.active)
    }

    pub fn active_mut(&mut self) -> Option<&mut Tab> {
        self.tabs.get_mut(self.active)
    }

    /// The active tab's *focused* terminal, which is what nearly every
    /// caller wants: input, rendering, selection and IME all act on the pane
    /// the keyboard is in — which is what makes a split tab route by
    /// changing one function instead of twenty call sites.
    #[must_use]
    /// `None` when there is no tab, and also when the focused pane holds a
    /// file rather than a shell (#464). The signature did not have to change
    /// for that second case, which is what kept the refactor to one function:
    /// every one of its callers already handled the empty-strip `None`.
    pub fn active_source(&self) -> Option<&dyn SessionSource> {
        self.active().and_then(Tab::focused_session)
    }

    /// Whether `addr` names the tab holding the keyboard. `close_tab` asks
    /// this *before* closing to decide if the close is also an activation:
    /// only then may `after_activation` run, because its ensure-visible flag
    /// would otherwise snap a wheel-scrolled strip back to the active chip on
    /// every background ×-click, pulling the next close target out from under
    /// the pointer.
    #[must_use]
    pub fn is_active(&self, addr: SessionAddr) -> bool {
        if addr == settings_addr() {
            return self.settings_active;
        }
        !self.settings_active && self.active().is_some_and(|t| t.addr == addr)
    }

    /// The Settings tab exists in the strip.
    #[must_use]
    pub fn settings_open(&self) -> bool {
        self.settings_open
    }

    /// The Settings tab holds the keyboard and the grid area.
    #[must_use]
    pub fn settings_active(&self) -> bool {
        self.settings_active
    }

    /// Open the Settings tab, or activate the one that exists — ⌘, never
    /// opens a second (design §11: "at most one exists at a time").
    pub fn open_settings(&mut self) {
        self.settings_open = true;
        self.settings_active = true;
    }

    /// Close the Settings tab; the keyboard returns to the session tab that
    /// held it before.
    pub fn close_settings(&mut self) {
        self.settings_open = false;
        self.settings_active = false;
    }

    /// Index into the drawn tab list (sessions, then Settings when open) of
    /// the tab that is lit — the chrome model's `active`.
    #[must_use]
    pub fn display_active(&self) -> usize {
        if self.settings_active {
            self.tabs.len()
        } else {
            self.active
        }
    }

    /// Whether `addr` names a tab in this strip, or a pane inside one.
    ///
    /// The routing question with several windows: a wakeup stamped with a
    /// session address goes to the window whose strip holds it, and a pane's
    /// address counts as held — a split pane's shell exiting must reach the
    /// window that shows the pane, not be dropped as nobody's.
    #[must_use]
    pub fn owns(&self, addr: SessionAddr) -> bool {
        self.tabs.iter().any(|t| t.addr == addr || t.panes.iter().any(|p| p.addr == addr))
    }

    /// The tab holding `addr` as one of its *split* panes, if any.
    pub fn find_pane_owner(&mut self, addr: SessionAddr) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.panes.iter().any(|p| p.addr == addr))
    }

    pub fn iter(&self) -> impl Iterator<Item = &Tab> {
        self.tabs.iter()
    }

    pub fn find_mut(&mut self, addr: SessionAddr) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.addr == addr)
    }

    /// The tabs worth remembering, and the active index *within that
    /// filtered list* — the filter and the remap live together because the
    /// remap is only correct against exactly this filter's survivors.
    ///
    /// Placeholders (in-process ptys) die with the window and cannot be
    /// reattached; dead sessions have nothing to reattach to. The app tabs
    /// never appear at all — they are not `Tab`s, and their sentinel
    /// addresses are placeholders besides, so both facts exclude them. When
    /// the active tab is itself filtered out the index stays 0, so a restore
    /// leads with a real session rather than an out-of-range index; while
    /// the Settings tab holds the keyboard `active` still names the session
    /// underneath, which is the tab a restore should lead with.
    #[must_use]
    pub(crate) fn persistable(&self) -> (usize, Vec<&Tab>) {
        let mut tabs = Vec::new();
        let mut active = 0;
        for (i, tab) in self.tabs.iter().enumerate() {
            if is_placeholder(tab.addr) || tab.dead {
                continue;
            }
            if i == self.active {
                active = tabs.len();
            }
            tabs.push(tab);
        }
        (active, tabs)
    }

    /// Re-resolve every profile-launched tab's identity against a reloaded
    /// config. By name: the file is the truth for what "ubuntu" looks like,
    /// and a tab holding a stale copy would repaint with colours the editor
    /// no longer shows. A deleted profile resolves as empty-over-Defaults,
    /// so the tab degrades to Defaults' look rather than freezing.
    pub fn reresolve_identities(&mut self, settings: &zest_config::Settings) {
        for tab in &mut self.tabs {
            if let Some(identity) = &mut tab.identity {
                *identity = ProfileIdentity::resolve(settings, &identity.name);
            }
        }
    }

    /// Carry every tab launched from `from` over to the profile's new name
    /// (#283).
    ///
    /// Must run *before* the reload that follows a rename, because
    /// [`Self::reresolve_identities`] resolves by name and a name that no
    /// longer exists resolves as empty-over-Defaults — silently, by design
    /// (the never-crash rule). A tab left pointing at the old name would
    /// therefore lose its scheme, accent and icon with nothing to see and
    /// nothing logged, which reads as "renaming a profile broke my tabs".
    pub fn rename_profile(&mut self, from: &str, to: &str) {
        for tab in &mut self.tabs {
            if let Some(identity) = &mut tab.identity {
                if identity.name == from {
                    identity.name = to.to_string();
                }
            }
        }
    }

    /// Add a tab and make it active — a new tab is something the user just
    /// asked for, so it takes the keyboard (from the Settings tab too).
    pub fn push(&mut self, tab: Tab) {
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.settings_active = false;
    }

    /// Adopt a worker-built session tab, keeping the address unique: if a
    /// live tab already holds this session, that tab is activated (when
    /// `focus`) and the newcomer is handed back for the caller to drop —
    /// dropping detaches, the shell stays on its host. Two tabs on one
    /// session made every click on the second resolve to the first
    /// (`activate_addr` is first-match by address), which shipped as
    /// "clicking tab 3 selects tab 1" (#188) — and a duplicate that reaches
    /// the strip is persisted, so it survives every restart. A dead tab does
    /// not block re-attach — the session outlived the tab that reported it
    /// gone — but it must not stay either: sitting the fresh tab beside it
    /// would put the dead twin first in every lookup, which is #188 again,
    /// so the new attachment revives the dead tab's own slot.
    pub fn adopt(&mut self, tab: Tab, focus: bool) -> Option<Tab> {
        if let Some(i) = self.tabs.iter().position(|t| t.addr == tab.addr) {
            if self.tabs[i].dead {
                // Dropping the husk is safe — its session is already gone —
                // and its slot carries straight over to the revived tab.
                self.tabs[i] = tab;
                if focus {
                    self.activate(i);
                }
                return None;
            }
            if focus {
                self.activate(i);
            }
            return Some(tab);
        }
        if focus {
            self.push(tab);
        } else {
            self.push_background(tab);
        }
        None
    }

    /// Add a tab without taking the keyboard — restored tabs arrive in the
    /// background, and a launch that steals focus once per remembered tab
    /// would be unusable.
    pub fn push_background(&mut self, tab: Tab) {
        self.tabs.push(tab);
    }

    /// Returns true when the active tab changed. Activating a session takes
    /// the keyboard back from the Settings tab, which stays open in place.
    pub fn activate(&mut self, index: usize) -> bool {
        if index >= self.tabs.len() || (index == self.active && !self.settings_active) {
            return false;
        }
        self.settings_active = false;
        self.active = index;
        true
    }

    pub fn activate_addr(&mut self, addr: SessionAddr) -> bool {
        if addr == settings_addr() {
            if !self.settings_open || self.settings_active {
                return false;
            }
            self.settings_active = true;
            return true;
        }
        match self.tabs.iter().position(|t| t.addr == addr) {
            Some(i) => self.activate(i),
            None => false,
        }
    }

    /// Next/prev walk the drawn order — sessions, then Settings when open —
    /// so the app tab takes its turn in the cycle like the ordinary tab §11
    /// says it is.
    pub fn activate_next(&mut self) -> bool {
        // A window can be alive with zero tabs (a failed first spawn warns
        // and returns), and `% 0` panics — cycling nothing is a no-op.
        let len = self.display_len();
        if len == 0 {
            return false;
        }
        self.activate_display((self.display_active() + 1) % len)
    }

    pub fn activate_prev(&mut self) -> bool {
        let len = self.display_len();
        if len == 0 {
            return false;
        }
        self.activate_display((self.display_active() + len - 1) % len)
    }

    fn display_len(&self) -> usize {
        self.tabs.len() + usize::from(self.settings_open)
    }

    fn activate_display(&mut self, index: usize) -> bool {
        if index == self.display_active() {
            return false;
        }
        if index == self.tabs.len() && self.settings_open {
            self.settings_active = true;
            return true;
        }
        self.activate(index)
    }

    /// Remove a tab, keeping the active index on the tab the user was
    /// looking at — or its neighbour, when that was the one removed.
    pub fn close(&mut self, addr: SessionAddr) -> Option<Tab> {
        let index = self.tabs.iter().position(|t| t.addr == addr)?;
        let tab = self.tabs.remove(index);
        if index < self.active || (index == self.active && self.active > 0) {
            self.active -= 1;
        }
        Some(tab)
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    // #436: panes are a list, not a pair. These pin the shape that every
    // hit region, wakeup and render pass now indexes into.

    fn split_tab(n: usize) -> Tab {
        let mut tab = Tab::pending_for_test(placeholder_addr(1));
        for i in 0..n {
            tab.panes.push(SplitPane::pending_for_test(placeholder_addr(i as u64 + 2)));
        }
        tab
    }

    #[test]
    fn any_number_of_panes_and_the_focus_wraps_both_ways() {
        let mut tab = split_tab(4);
        assert_eq!(tab.pane_count(), 5, "the primary plus four — no cap at two");
        assert_eq!(tab.focused_addr(), placeholder_addr(1), "the primary starts focused");
        for i in 1..5 {
            assert!(tab.cycle_focus(1));
            assert_eq!(tab.focused_addr(), placeholder_addr(i as u64 + 1), "→ walks right");
        }
        assert!(tab.cycle_focus(1));
        assert_eq!(tab.focus, 0, "past the last pane wraps to the primary");
        assert!(tab.cycle_focus(-1));
        assert_eq!(tab.focus, 4, "← from the primary wraps to the last pane");
        assert!(tab.focus_pane(2), "a click names a pane by index");
        assert!(!tab.focus_pane(2), "refocusing the focused pane is not a change");
        assert!(!tab.focus_pane(9), "an index past the panes is ignored, never a panic");
        assert_eq!(tab.focus, 2);
    }

    #[test]
    fn an_unsplit_tab_has_nothing_to_cycle_or_close() {
        let mut tab = split_tab(0);
        assert!(!tab.is_split());
        assert!(!tab.cycle_focus(1), "one pane: the chord does nothing");
        assert!(!tab.close_focused_pane(), "no pane to close: ⌘W means the tab");
    }

    #[test]
    fn closing_a_middle_pane_moves_the_keyboard_one_left() {
        let mut tab = split_tab(3);
        tab.focus_pane(2);
        assert!(tab.close_focused_pane());
        assert_eq!(tab.pane_count(), 3);
        assert_eq!(tab.focus, 1, "the keyboard lands on the left neighbour");
        assert_eq!(
            tab.panes.iter().map(|p| p.addr).collect::<Vec<_>>(),
            vec![placeholder_addr(2), placeholder_addr(4)],
            "exactly the focused pane went"
        );
    }

    #[test]
    fn closing_the_primary_promotes_the_next_pane_into_the_tab() {
        // The tab keeps its slot in the strip while its surviving shells keep
        // running — the alternative is how sessions get orphaned.
        let mut tab = split_tab(2);
        assert!(tab.close_focused_pane());
        assert_eq!(tab.addr, placeholder_addr(2), "the first pane is the tab now");
        assert_eq!(tab.pane_count(), 2);
        assert_eq!(tab.focus, 0);
        assert_eq!(tab.panes[0].addr, placeholder_addr(3));
        assert!(tab.connecting, "the promoted pane's state comes with it");
    }

    #[test]
    fn a_pane_whose_shell_ended_leaves_without_moving_the_keyboard_off_its_pane() {
        let mut tab = split_tab(3);
        tab.focus_pane(3);
        assert!(tab.remove_gone_pane(placeholder_addr(2)), "the first pane's shell exited");
        assert_eq!(tab.focus, 2, "the focused pane is the same pane, one index left");
        assert_eq!(tab.focused_addr(), placeholder_addr(4));
        assert!(!tab.remove_gone_pane(placeholder_addr(1)), "the primary's exit is the tab's");
        assert!(!tab.remove_gone_pane(placeholder_addr(99)), "an unknown address is nobody's");
        tab.focus_pane(1);
        assert!(tab.remove_gone_pane(placeholder_addr(3)), "the focused pane itself goes");
        assert_eq!(tab.focus, 0, "the keyboard falls one left");
        assert!(!tab.pane_dead(0));
    }

    #[test]
    fn a_stale_index_reads_as_the_primary_rather_than_panicking() {
        let tab = split_tab(1);
        assert_eq!(tab.pane_addr(7), tab.addr, "a hit region a frame behind a close");
        assert_eq!(tab.pane_dead(7), tab.dead);
    }

    #[test]
    fn the_strip_finds_the_tab_that_owns_a_pane() {
        let mut strip = TabStrip::default();
        strip.push(split_tab(0));
        let mut owner = split_tab(2);
        owner.addr = placeholder_addr(10);
        strip.push(owner);
        assert_eq!(strip.find_pane_owner(placeholder_addr(3)).map(|t| t.addr), Some(placeholder_addr(10)));
        assert!(strip.find_pane_owner(placeholder_addr(10)).is_none(), "a tab's own address is not a pane's");
    }

    // TabStrip's only nontrivial logic is keeping `active` pointing at the
    // same tab across removals — exactly the off-by-one that shows up as
    // "closing a background tab switched my focus".

    #[test]
    fn adopting_an_already_open_session_activates_instead_of_duplicating() {
        // #188, from a user's own tabs.json: session 1 attached twice made
        // two tabs share one address, and activate_addr's first-match then
        // sent every click on the second to the first — "clicking tab 3
        // selects tab 1". Adoption refuses the duplicate and hands it back
        // (dropping detaches; the shell stays), activating the original.
        let mut strip = TabStrip::default();
        for n in 1..=2 {
            strip.push(fake(n));
        }
        strip.activate(1);
        let dup = fake(1); // the same session, arriving again
        let rejected = strip.adopt(dup, true);
        assert!(rejected.is_some(), "the duplicate comes back for the caller to detach-drop");
        assert_eq!(strip.len(), 2, "the strip holds each session once");
        assert_eq!(
            strip.active().expect("active").addr,
            placeholder_addr(1),
            "and asking for the session again activates the tab it already has"
        );
        rejected.expect("checked above").kill();

        // Background adoption (a restore) also collapses: the second copy
        // of a restored duplicate never reaches the strip, so a bad file
        // heals itself on the next launch.
        let restored_dup = fake(2);
        let rejected = strip.adopt(restored_dup, false);
        assert!(rejected.is_some(), "a restored duplicate is refused the same way");
        assert_eq!(strip.len(), 2);
        assert_eq!(
            strip.active().expect("active").addr,
            placeholder_addr(1),
            "a background adoption never steals the keyboard, duplicate or not"
        );
        rejected.expect("checked above").kill();

        // A dead tab does not block re-attach — the session outlived the
        // tab that reported it gone — but it must not stay either: a fresh
        // twin BESIDE it would put the dead tab first in every first-match
        // lookup, which is #188 all over again. The new attachment revives
        // the dead tab's own slot.
        strip.find_mut(placeholder_addr(2)).expect("tab 2 exists").dead = true;
        let fresh = fake(2);
        assert!(
            strip.adopt(fresh, false).is_none(),
            "the live session revives the dead tab's slot, nothing is refused"
        );
        assert_eq!(strip.len(), 2, "revival replaces; the address stays unique");
        assert!(
            !strip.find_mut(placeholder_addr(2)).expect("tab 2 exists").dead,
            "and the tab in that slot is the live one"
        );
        for addr in [placeholder_addr(1), placeholder_addr(2)] {
            strip.close(addr).expect("tab exists").kill();
        }
    }

    #[test]
    fn closing_a_background_tab_keeps_the_active_one() {
        let mut strip = TabStrip::default();
        // Placeholders stand in for sessions; the strip never looks inside.
        for n in 1..=3 {
            strip.push(fake(n));
        }
        strip.activate(1);
        // Close the tab *before* the active one: the index shifts left but
        // the same tab keeps the keyboard.
        let closed = strip.close(placeholder_addr(1)).expect("tab 1 exists");
        closed.kill();
        assert_eq!(strip.active().expect("active").addr, placeholder_addr(2));
        // Close the tab after it: nothing moves.
        strip.close(placeholder_addr(3)).expect("tab 3 exists").kill();
        assert_eq!(strip.active().expect("active").addr, placeholder_addr(2));
    }

    #[test]
    fn closing_a_background_chip_is_not_an_activation() {
        // close_tab() asks this *before* closing to decide whether
        // after_activation() runs — which sets strip_ensure_visible.
        // Answering true for a background chip snaps a wheel-scrolled strip
        // back to the active chip on every ×-click, pulling the next close
        // target out from under the pointer.
        let mut strip = TabStrip::default();
        for n in 1..=3 {
            strip.push(fake(n));
        }
        strip.activate(1);
        assert!(
            strip.is_active(placeholder_addr(2)),
            "closing the tab holding the keyboard is an activation"
        );
        assert!(
            !strip.is_active(placeholder_addr(1)),
            "closing a background chip is not, even though its index shift moves `active`"
        );
        for tab in [placeholder_addr(1), placeholder_addr(2), placeholder_addr(3)] {
            strip.close(tab).expect("tab exists").kill();
        }
    }

    #[test]
    fn closing_the_active_tab_falls_back_to_a_neighbour() {
        let mut strip = TabStrip::default();
        for n in 1..=3 {
            strip.push(fake(n));
        }
        strip.activate(2);
        strip.close(placeholder_addr(3)).expect("tab 3 exists").kill();
        assert_eq!(
            strip.active().expect("active").addr,
            placeholder_addr(2),
            "closing the last, active tab moves to its left neighbour"
        );
    }

    #[test]
    fn a_config_reload_reresolves_identities_by_name() {
        // The tab holds a *copy* of its profile's appearance, so an edit to
        // [profiles.ubuntu] must reach open tabs at reload or the editor and
        // the window disagree about what "ubuntu" looks like.
        let settings = |scheme: &str, opacity: f64| {
            let mut s = zest_config::Settings::default();
            let table: toml::Table = format!(
                "color_scheme = \"{scheme}\"\ntab_color = 2\n[window]\nopacity = {opacity}\n"
            )
            .parse()
            .expect("valid toml");
            s.profiles.insert("ubuntu".into(), table);
            s
        };

        let before = settings("nord", 0.9);
        let mut strip = TabStrip::default();
        strip.push(fake(1).with_identity(Some(ProfileIdentity::resolve(&before, "ubuntu"))));
        strip.push(fake(2)); // a plain tab, to prove reresolve leaves it alone

        let id = strip.iter().next().unwrap().identity.as_ref().expect("identity set");
        assert_eq!(id.scheme.as_deref(), Some("nord"));

        strip.reresolve_identities(&settings("paper", 0.5));
        let id = strip.iter().next().unwrap().identity.as_ref().expect("identity kept");
        assert_eq!(id.scheme.as_deref(), Some("paper"), "the reload's scheme wins");
        assert_eq!(
            id.selection_bg,
            scheme_selection_wash("paper"),
            "the cached wash follows the re-resolve — the render path reads \
             only the cache, so a stale one selects in the old scheme's colour"
        );
        assert_eq!(id.opacity, Some(0.5), "and so does its opacity");
        assert_eq!(id.tab_color, Some(2), "unchanged keys survive the re-resolve");
        assert!(
            strip.iter().nth(1).unwrap().identity.is_none(),
            "a tab launched from no profile must not grow one on reload"
        );
        for tab in [placeholder_addr(1), placeholder_addr(2)] {
            strip.close(tab).expect("tab exists").kill();
        }
    }

    #[test]
    fn a_renamed_profile_carries_its_open_tabs_with_it() {
        // #283. `reresolve_identities` resolves by name, and a name that no
        // longer exists resolves as empty-over-Defaults *silently* — so a tab
        // left on the old name loses its scheme, accent and icon with nothing
        // to see and nothing logged. The rename must move the tabs first.
        let settings = |scheme: &str| {
            let mut s = zest_config::Settings::default();
            let table: toml::Table = format!("color_scheme = \"{scheme}\"\ntab_color = 2\n")
                .parse()
                .expect("valid toml");
            s.profiles.insert("forge".into(), table);
            s
        };

        let mut strip = TabStrip::default();
        let mut before = zest_config::Settings::default();
        before.profiles.insert(
            "ubuntu".into(),
            "color_scheme = \"nord\"\ntab_color = 2\n".parse().expect("valid toml"),
        );
        strip.push(fake(1).with_identity(Some(ProfileIdentity::resolve(&before, "ubuntu"))));
        strip.push(fake(2)); // a plain tab, to prove the rename leaves it alone

        strip.rename_profile("ubuntu", "forge");
        assert_eq!(
            strip.iter().next().unwrap().identity.as_ref().expect("identity kept").name,
            "forge",
            "the tab still names the profile it was launched from"
        );
        assert!(
            strip.iter().nth(1).unwrap().identity.is_none(),
            "a tab launched from no profile must not grow one on a rename"
        );

        // And the re-resolve that follows now finds it, instead of quietly
        // degrading the tab to Defaults.
        strip.reresolve_identities(&settings("paper"));
        let id = strip.iter().next().unwrap().identity.as_ref().expect("identity kept");
        assert_eq!(
            id.scheme.as_deref(),
            Some("paper"),
            "the renamed profile's scheme reached the tab; a stale name would \
             have resolved as empty-over-Defaults and shown no scheme at all"
        );
        assert_eq!(id.tab_color, Some(2), "and its accent came with it");

        for tab in [placeholder_addr(1), placeholder_addr(2)] {
            strip.close(tab).expect("tab exists").kill();
        }
    }

    #[test]
    fn a_nan_opacity_degrades_to_the_window_not_into_the_render_path() {
        // TOML admits `nan`, and f32::clamp preserves it — an identity that
        // carried it would hand every fill computation a non-finite alpha.
        let mut s = zest_config::Settings::default();
        let table: toml::Table = "[window]\nopacity = nan\n".parse().expect("toml allows nan");
        s.profiles.insert("odd".into(), table);
        let id = ProfileIdentity::resolve(&s, "odd");
        assert_eq!(
            id.opacity, None,
            "a non-finite opacity is the file's problem, not the frame's — \
             None falls through to the window's value"
        );
    }

    #[test]
    fn identity_resolution_inherits_through_defaults() {
        // The §12 fleet story: color_from on Defaults reads by machine for
        // every profile that does not say otherwise.
        let mut s = zest_config::Settings::default();
        s.profiles.insert(
            "defaults".into(),
            "color_from = \"host\"\nicon = \"circle\"\n".parse().expect("valid toml"),
        );
        s.profiles
            .insert("ubuntu".into(), "icon = \"tux\"\n".parse().expect("valid toml"));
        let id = ProfileIdentity::resolve(&s, "ubuntu");
        assert_eq!(id.color_from, Some(zest_config::ColorFrom::Host), "fell through Defaults");
        assert_eq!(id.icon.as_deref(), Some("tux"), "own keys shadow Defaults'");
        assert_eq!(id.scheme, None, "unset stays unset — the window palette's cue");
        assert_eq!(id.selection_bg, None, "no scheme, no cached wash: render falls back live");
        assert_eq!(id.opacity, None);
    }

    #[test]
    fn a_connecting_tab_shows_provenance_and_settles_failed_with_the_error() {
        // The issue-#175 path: the tab exists before any socket does,
        // showing where the launch is going, and a host that never answers
        // turns it into the dead-tab treatment *carrying the reason* — the
        // pane says what failed, not a log line nobody reads.
        let pending = PendingSession::new(
            40,
            6,
            zest_core::Terminal::new(2, 2, 0).palette().clone(),
            "Ubuntu",
            "New session \u{b7} Ubuntu on forge \u{b7} wsl.exe",
            "forge",
        );
        let mut tab = Tab::connecting(placeholder_addr(1), pending, (40, 6));
        assert!(tab.connecting && !tab.dead);
        assert!(
            !tab.local,
            "closing a connecting tab must detach-by-drop, never kill anything"
        );
        {
            let term = tab.source().terminal();
            let term = term.lock();
            assert_eq!(term.title(), "Ubuntu", "the chip reads the profile, not 'shell'");
            assert!(
                term.screen_text().contains("New session \u{b7} Ubuntu on forge"),
                "the provenance line is in the pane: {}",
                term.screen_text()
            );
        }
        assert_eq!(
            tab.source().origin(),
            crate::source::Origin::Daemon { host: "forge".into(), local: false },
            "the chrome groups and inks this tab by the host it is dialling"
        );

        tab.resolve_failed("host 'forge' is not in the fleet");
        assert!(!tab.connecting && tab.dead, "failed is the dead-tab treatment");
        assert!(
            tab.source().terminal().lock().screen_text().contains("is not in the fleet"),
            "and the error rides in the pane"
        );
        tab.kill();
    }

    #[test]
    fn a_pending_pane_neutralizes_control_bytes_in_what_it_is_fed() {
        // An error (or profile name) carrying a stray escape would repaint
        // or retitle the very pane reporting it — the raw-VT trap, in the UI.
        let pending = PendingSession::new(
            40,
            6,
            zest_core::Terminal::new(2, 2, 0).palette().clone(),
            "bad\x1b]2;evil\x07name",
            "line\x1b[2Jwiped",
            "host",
        );
        let tab = Tab::connecting(placeholder_addr(2), pending, (40, 6));
        let term = tab.source().terminal();
        assert!(
            !term.lock().title().contains("evil"),
            "the embedded OSC never executed as a retitle: {:?}",
            term.lock().title()
        );
        assert!(
            term.lock().screen_text().contains("line [2Jwiped"),
            "the clear-screen never executed: {}",
            term.lock().screen_text()
        );
    }

    #[test]
    fn the_profiles_tab_opens_once_and_reopening_is_an_activation() {
        // The singleton rule: `⌘⇧,` (or the launcher's Manage-profiles row)
        // on an already-open Profiles tab must activate it, never grow a
        // second chip — the state itself makes a duplicate unrepresentable,
        // and this pins the open/reopen answers the caller branches on.
        let mut tabs = AppTabs::default();
        assert!(!tabs.profiles_open(), "nothing is open until asked");
        assert!(tabs.open_profiles(), "the first open reports newly created");
        assert!(tabs.profiles_open());
        assert!(
            !tabs.open_profiles(),
            "the second open reports already-there: an activation, not a duplicate"
        );
        assert!(tabs.profiles_open(), "…and it is still open, exactly once");
        tabs.close_profiles();
        assert!(!tabs.profiles_open(), "closing it is closing a tab");
        assert!(tabs.open_profiles(), "and it can come back");
    }

    #[test]
    fn the_profiles_chip_address_is_a_placeholder_no_session_reaches() {
        // Persistence and the picker skip placeholders, so the chip can
        // never be saved as a session; real placeholders count up from 1,
        // and the two app tabs' reserved ids must never collide with each
        // other either — both branches once picked u64::MAX independently.
        assert!(is_placeholder(profiles_tab_addr()));
        assert_ne!(profiles_tab_addr(), placeholder_addr(1));
        assert_ne!(
            profiles_tab_addr(),
            settings_addr(),
            "Settings and Profiles are different tabs and need different keys"
        );
    }

    #[test]
    fn the_settings_tab_is_a_singleton_that_activates_in_place() {
        // §11: ⌘, opens it; if it is already open it activates that tab
        // rather than opening a second. Closing it hands the keyboard back
        // to the session it took it from.
        let mut strip = TabStrip::default();
        for n in 1..=2 {
            strip.push(fake(n));
        }
        strip.activate(0);
        assert!(!strip.settings_open(), "nothing opens it but the user");

        strip.open_settings();
        assert!(strip.settings_open() && strip.settings_active());
        assert_eq!(
            strip.display_active(),
            2,
            "the settings chip sits after the session tabs and is the lit one"
        );
        assert!(strip.is_active(settings_addr()));
        assert_eq!(
            strip.active().expect("session kept").addr,
            placeholder_addr(1),
            "the session tab keeps its slot underneath"
        );

        strip.open_settings();
        assert!(strip.settings_open(), "a second ⌘, is an activation, not a second tab");

        strip.close_settings();
        assert!(!strip.settings_open() && !strip.settings_active());
        assert!(
            strip.is_active(placeholder_addr(1)),
            "closing the settings tab returns the keyboard to the session it took it from"
        );
        for tab in [placeholder_addr(1), placeholder_addr(2)] {
            strip.close(tab).expect("tab exists").kill();
        }
    }

    #[test]
    fn tab_cycling_takes_the_settings_tab_in_its_turn() {
        // §11: an ordinary tab in the strip, after the session tabs — so
        // next/prev include it, in that position.
        let mut strip = TabStrip::default();
        for n in 1..=2 {
            strip.push(fake(n));
        }
        strip.open_settings();
        strip.activate(0);
        assert!(!strip.settings_active(), "activating a session takes the keyboard back");

        strip.activate_next();
        assert_eq!(strip.display_active(), 1);
        strip.activate_next();
        assert!(strip.settings_active(), "settings takes its turn after the last session");
        strip.activate_next();
        assert_eq!(strip.display_active(), 0, "and the cycle wraps past it");
        strip.activate_prev();
        assert!(strip.settings_active(), "prev wraps back onto it");

        assert!(
            !strip.activate_addr(settings_addr()),
            "activating the already-active settings tab is not a change"
        );
        for tab in [placeholder_addr(1), placeholder_addr(2)] {
            strip.close(tab).expect("tab exists").kill();
        }
    }

    #[test]
    fn cycling_an_empty_strip_is_a_no_op_not_a_panic() {
        // A live window can hold zero tabs: `new_tab()` warns and returns
        // when the spawn fails, so Ctrl+Tab reaches next/prev with
        // `display_len() == 0` — which used to be a remainder-by-zero panic.
        let mut strip = TabStrip::default();
        assert!(!strip.activate_next(), "nothing to cycle to");
        assert!(!strip.activate_prev(), "in either direction");

        // And a strip holding only the Settings tab is a cycle of one:
        // still a no-op, never a change.
        strip.open_settings();
        assert!(!strip.activate_next(), "a lone tab has no next");
        assert!(!strip.activate_prev(), "nor a prev");
        assert!(strip.settings_active(), "and it keeps the keyboard");

    }

    #[test]
    fn a_new_tab_takes_the_keyboard_from_settings_but_a_background_one_does_not() {
        // One decision with two answers: push() is something the user just
        // asked for, so it takes the keyboard from the Settings tab like it
        // does from any session; push_background() is a restore or a
        // background attach, and a launch that yanked the keyboard out of
        // Settings once per remembered tab would make the screen unusable
        // exactly while tabs restore.
        let mut strip = TabStrip::default();
        strip.push(fake(1));
        strip.open_settings();
        assert!(strip.settings_active(), "sanity: settings holds the keyboard");

        strip.push(fake(2));
        assert!(
            !strip.settings_active(),
            "a new tab the user asked for takes the keyboard from the Settings tab"
        );
        assert_eq!(strip.active().expect("active").addr, placeholder_addr(2));
        assert!(
            strip.settings_open(),
            "the Settings tab stays open in place — it lost the keyboard, not its chip"
        );

        assert!(strip.activate_addr(settings_addr()), "the chip takes it back");
        strip.push_background(fake(3));
        assert!(
            strip.settings_active(),
            "a background arrival must not steal the keyboard from the Settings tab"
        );
        assert_eq!(
            strip.display_active(),
            strip.len(),
            "…and the settings chip is still the lit one"
        );
        for addr in [placeholder_addr(1), placeholder_addr(2), placeholder_addr(3)] {
            strip.close(addr).expect("tab exists").kill();
        }
    }

    #[test]
    fn persist_skips_filtered_tabs_and_remaps_the_active_index() {
        // `persistable`'s remap is the arithmetic `persist_tabs` writes to
        // disk: a filtered tab sitting before the active session must not
        // inflate the saved index, or the restore lights a neighbour of the
        // tab the user was looking at.
        let mut strip = TabStrip::default();
        strip.push(fake(1)); // a placeholder: the in-process fallback, dies with the window
        strip.push(real(2));
        strip.push(real(3));
        strip.activate(2);
        let (active, tabs) = strip.persistable();
        assert_eq!(tabs.len(), 2, "placeholder tabs never persist");
        assert_eq!(
            active, 1,
            "the saved index counts persisted tabs only — the placeholder \
             before the active session must not inflate it"
        );

        // The Settings tab holding the keyboard changes nothing: it is not
        // a `Tab`, and `active` still names the session underneath, which
        // is what the restore should lead with (Settings itself costs
        // nothing to reopen and means nothing to reattach).
        strip.open_settings();
        let (active, tabs) = strip.persistable();
        assert_eq!(
            (active, tabs.len()),
            (1, 2),
            "an open, active Settings tab is invisible to persistence"
        );
        strip.close_settings();

        // A dead session has nothing left to reattach to, and the remap
        // must hold as the filter tightens.
        strip.find_mut(real_addr(2)).expect("tab 2 exists").dead = true;
        let (active, tabs) = strip.persistable();
        assert_eq!(tabs.len(), 1, "dead sessions never persist");
        assert_eq!(active, 0, "…and the remap tracks the tightened filter");

        // The active tab is itself filtered out: lead with a real session
        // rather than an out-of-range index.
        strip.activate(0);
        let (active, _) = strip.persistable();
        assert_eq!(active, 0, "an active tab that does not persist saves as index 0");

        for addr in [placeholder_addr(1), real_addr(2), real_addr(3)] {
            strip.close(addr).expect("tab exists").kill();
        }
    }

    #[test]
    fn the_app_tab_sentinels_are_placeholders_no_counter_reaches() {
        // Persistence has exactly one address filter, `is_placeholder` — so
        // the app tabs' sentinels must be placeholders for a
        // sentinel-addressed tab that ever leaked into the strip to stay
        // out of tabs.json. The Profiles half is pinned in
        // the_profiles_chip_address_is_a_placeholder_no_session_reaches;
        // this is the Settings half, plus the boundary ids the guard in
        // `placeholder_addr` admits — the proof has to hold at the
        // boundary, not just for small counters.
        assert!(
            is_placeholder(settings_addr()),
            "the Settings sentinel must read as a placeholder, or persistence could save it"
        );
        for n in [0, 1, 1000, u64::MAX - 2] {
            assert_ne!(
                placeholder_addr(n),
                settings_addr(),
                "a placeholder tab must never hit-test as the Settings tab"
            );
            assert_ne!(
                placeholder_addr(n),
                profiles_tab_addr(),
                "a placeholder tab must never hit-test as the Profiles chip"
            );
        }
    }

    #[test]
    #[should_panic(expected = "app-tab sentinels")]
    fn a_placeholder_id_at_a_sentinel_fails_fast() {
        // A collision would silently activate or close the wrong tab in a
        // release build — every hit region keys on the address — so the
        // guard is an assert!, not a debug_assert!, and this pins that it
        // stays one.
        let _ = placeholder_addr(u64::MAX - 1);
    }

    #[test]
    fn next_and_prev_wrap() {
        let mut strip = TabStrip::default();
        for n in 1..=3 {
            strip.push(fake(n));
        }
        assert_eq!(strip.active_index(), 2, "a pushed tab takes the keyboard");
        strip.activate_next();
        assert_eq!(strip.active_index(), 0, "next wraps");
        strip.activate_prev();
        assert_eq!(strip.active_index(), 2, "prev wraps back");
    }

    /// A tab whose session is a real in-process pty would spawn a shell per
    /// test; a placeholder-address tab around a trivially small `Session` is
    /// not constructible without one. So these tests build tabs through a
    /// tiny command that exits immediately and never gets read.
    fn fake(n: u64) -> Tab {
        let spec = zest_pty::CommandSpec {
            command_line: if cfg!(windows) { "cmd /c exit" } else { "/usr/bin/true" }.into(),
            ..zest_pty::CommandSpec::default_shell()
        };
        // Retried, because allocating a pty is not reliable under load.
        //
        // On a busy macOS runner this fails intermittently with ENXIO — "Device
        // not configured" — from the `posix_openpt`/`grantpt` path, and every
        // test in this module wants one, in parallel. It failed twice in a row
        // on unrelated changes, on a different test each time, which is what a
        // resource flake looks like rather than a bug in the code under test.
        //
        // A few attempts a few milliseconds apart, and a message naming the
        // cause if they all fail — so a genuine breakage still reads as one
        // rather than being retried into a timeout.
        let mut last = None;
        for _ in 0..10 {
            match Session::spawn(&spec, zest_pty::PtySize::new(10, 4), 10, |_| {}) {
                Ok(session) => return Tab::in_process(session, placeholder_addr(n), (10, 4)),
                Err(e) => {
                    last = Some(e);
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
            }
        }
        panic!("spawn a trivial child, after 10 attempts: {last:?}")
    }

    /// A non-placeholder address, for tabs that should survive the
    /// persistence filter. Any non-zero host works; a real one is a key
    /// fingerprint, which a test has no way (and no need) to mint.
    fn real_addr(n: u64) -> SessionAddr {
        SessionAddr::new(HostId::from_bytes([9; 32]), SessionId(n))
    }

    /// A [`fake`] tab wearing a non-placeholder address, so `persistable`
    /// keeps it — a placeholder-addressed tab would be filtered for the
    /// wrong reason and the remap under test would never run.
    fn real(n: u64) -> Tab {
        let mut tab = fake(n);
        tab.addr = real_addr(n);
        tab
    }
}

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
        let opacity = resolved
            .overrides
            .get("window")
            .and_then(toml::Value::as_table)
            .and_then(|w| w.get("opacity"))
            .and_then(|v| match v {
                toml::Value::Float(f) => Some(*f as f32),
                toml::Value::Integer(i) => Some(*i as f32),
                _ => None,
            })
            // TOML admits `nan`, and clamp preserves it — a non-finite value
            // must degrade to None (the window's opacity), not ride NaN into
            // the render path.
            .filter(|o| o.is_finite())
            .map(|o| o.clamp(0.0, 1.0));

        let scheme = resolved.meta.color_scheme;
        Self {
            name: name.to_string(),
            selection_bg: scheme.as_deref().and_then(scheme_selection_wash),
            scheme,
            tab_color: resolved.meta.tab_color,
            icon: resolved.meta.icon,
            color_from: resolved.meta.color_from,
            opacity,
            title: resolved.meta.tab_title,
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
    match zest_theme::builtin::get(scheme) {
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
}

pub struct Tab {
    pub addr: SessionAddr,
    session: TabSession,
    /// A second pane, split right (design screen 5). Two panes is the
    /// design's shape; a tree is a later one, and boxed so an unsplit tab —
    /// every tab, most of the time — pays a pointer.
    pub split: Option<Box<SplitPane>>,
    /// Which pane owns the keyboard: `false` = left/primary, `true` = right.
    pub focus_right: bool,
    /// The session runs on this machine (loopback daemon or in-process).
    /// Decides close semantics: closing a local tab kills; a remote one
    /// detaches.
    pub local: bool,
    /// `Wakeup::SessionGone` arrived: the host answered and the session no
    /// longer exists. The tab stays put saying so rather than vanishing —
    /// or worse, silently becoming a different shell.
    pub dead: bool,
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
}

/// The right-hand pane of a split tab: a session with the little state a
/// pane needs, and none of a tab's (a pane is not a hit target in the strip,
/// does not persist, and cannot be dragged).
pub struct SplitPane {
    pub addr: SessionAddr,
    session: TabSession,
    pub local: bool,
    pub dead: bool,
    pub sized: (u16, u16),
}

impl SplitPane {
    pub fn daemon(remote: RemoteSession, local: bool, sized: (u16, u16)) -> Self {
        Self { addr: remote.addr(), session: TabSession::Daemon(remote), local, dead: false, sized }
    }

    pub fn in_process(session: Session, addr: SessionAddr, sized: (u16, u16)) -> Self {
        Self { addr, session: TabSession::InProcess(session), local: true, dead: false, sized }
    }

    pub fn source(&self) -> &dyn SessionSource {
        match &self.session {
            TabSession::Daemon(r) => r,
            TabSession::InProcess(s) => s,
        }
    }

    /// End the pane's session for good (local close); dropping detaches.
    pub fn kill(self) {
        match self.session {
            TabSession::Daemon(r) => r.kill(),
            TabSession::InProcess(s) => drop(s),
        }
    }
}

impl Tab {
    pub fn daemon(remote: RemoteSession, local: bool, sized: (u16, u16)) -> Self {
        Self {
            addr: remote.addr(),
            session: TabSession::Daemon(remote),
            split: None,
            focus_right: false,
            local,
            dead: false,
            sized,
            dial_hint: None,
            identity: None,
        }
    }

    #[must_use]
    pub fn with_dial_hint(mut self, hint: Option<String>) -> Self {
        self.dial_hint = hint;
        self
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
            split: None,
            focus_right: false,
            local: true,
            dead: false,
            sized,
            dial_hint: None,
            identity: None,
        }
    }

    pub fn source(&self) -> &dyn SessionSource {
        match &self.session {
            TabSession::Daemon(r) => r,
            TabSession::InProcess(s) => s,
        }
    }

    /// The pane the keyboard belongs to — what input, selection, IME and the
    /// status bar all act on. The primary pane unless a split holds focus.
    pub fn focused_source(&self) -> &dyn SessionSource {
        match (&self.split, self.focus_right) {
            (Some(split), true) => split.source(),
            _ => self.source(),
        }
    }

    /// The focused pane's session address.
    #[must_use]
    pub fn focused_addr(&self) -> SessionAddr {
        match (&self.split, self.focus_right) {
            (Some(split), true) => split.addr,
            _ => self.addr,
        }
    }

    /// Close the focused pane of a split tab; `false` when there is no
    /// split, in which case closing means the whole tab.
    ///
    /// Closing the *left* pane promotes the right one into the tab, so the
    /// tab keeps its identity in the strip while the surviving shell keeps
    /// running — the alternative (the tab vanishing while a pane lives) is
    /// how sessions get orphaned.
    pub fn close_focused_pane(&mut self) -> bool {
        let Some(split) = self.split.take() else { return false };
        if self.focus_right {
            self.focus_right = false;
            if split.local {
                split.kill();
            }
            // A remote pane's drop detaches, exactly like a remote tab.
        } else {
            let old_addr = self.addr;
            let was_local = self.local;
            let old = core::mem::replace(&mut self.session, split.session);
            self.addr = split.addr;
            self.local = split.local;
            self.dead = split.dead;
            self.sized = split.sized;
            let _ = old_addr;
            if was_local {
                match old {
                    TabSession::Daemon(r) => r.kill(),
                    TabSession::InProcess(s) => drop(s),
                }
            }
        }
        true
    }

    /// End this tab's session for good.
    ///
    /// Only meaningful for daemon sessions — an in-process pty dies with its
    /// `Session` drop regardless, which is also what a dead tab needs. A
    /// split pane follows its tab's fate: local panes die, remote detach.
    pub fn kill(self) {
        if let Some(split) = self.split {
            if split.local {
                split.kill();
            }
        }
        match self.session {
            TabSession::Daemon(r) => r.kill(),
            TabSession::InProcess(s) => drop(s),
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
    SessionAddr::new(HostId::from_bytes([0; 32]), SessionId(n))
}

/// Whether an address is a placeholder rather than a session in the fleet.
#[must_use]
#[allow(dead_code, reason = "the picker and persistence skip placeholder tabs, next in #23")]
pub fn is_placeholder(addr: SessionAddr) -> bool {
    addr.host == HostId::from_bytes([0; 32])
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
    pub fn active_source(&self) -> Option<&dyn SessionSource> {
        self.active().map(Tab::focused_source)
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

    /// The tab holding `addr` as its *split* pane, if any.
    pub fn find_split_owner(&mut self, addr: SessionAddr) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.split.as_ref().is_some_and(|s| s.addr == addr))
    }

    pub fn iter(&self) -> impl Iterator<Item = &Tab> {
        self.tabs.iter()
    }

    pub fn find_mut(&mut self, addr: SessionAddr) -> Option<&mut Tab> {
        self.tabs.iter_mut().find(|t| t.addr == addr)
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

    /// Add a tab and make it active — a new tab is something the user just
    /// asked for, so it takes the keyboard (from the Settings tab too).
    pub fn push(&mut self, tab: Tab) {
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
        self.settings_active = false;
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

    /// Drop every tab — the window-close path, where dropping *is* the
    /// detach (`RemoteSession`'s destructor sends it).
    pub fn clear(&mut self) {
        self.tabs.clear();
        self.active = 0;
        self.settings_open = false;
        self.settings_active = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // TabStrip's only nontrivial logic is keeping `active` pointing at the
    // same tab across removals — exactly the off-by-one that shows up as
    // "closing a background tab switched my focus".

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
}

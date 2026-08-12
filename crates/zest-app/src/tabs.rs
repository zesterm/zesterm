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

use zest_proto::{HostId, SessionAddr, SessionId};

use crate::remote::RemoteSession;
use crate::session::Session;
use crate::source::SessionSource;

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
        }
    }

    #[must_use]
    pub fn with_dial_hint(mut self, hint: Option<String>) -> Self {
        self.dial_hint = hint;
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

/// The window's open tabs, and which one the keyboard belongs to.
#[derive(Default)]
pub struct TabStrip {
    tabs: Vec<Tab>,
    active: usize,
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
        self.active().is_some_and(|t| t.addr == addr)
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

    /// Add a tab and make it active — a new tab is something the user just
    /// asked for, so it takes the keyboard.
    pub fn push(&mut self, tab: Tab) {
        self.tabs.push(tab);
        self.active = self.tabs.len() - 1;
    }

    /// Add a tab without taking the keyboard — restored tabs arrive in the
    /// background, and a launch that steals focus once per remembered tab
    /// would be unusable.
    pub fn push_background(&mut self, tab: Tab) {
        self.tabs.push(tab);
    }

    /// Returns true when the active tab changed.
    pub fn activate(&mut self, index: usize) -> bool {
        if index >= self.tabs.len() || index == self.active {
            return false;
        }
        self.active = index;
        true
    }

    pub fn activate_addr(&mut self, addr: SessionAddr) -> bool {
        match self.tabs.iter().position(|t| t.addr == addr) {
            Some(i) => self.activate(i),
            None => false,
        }
    }

    pub fn activate_next(&mut self) -> bool {
        if self.tabs.len() < 2 {
            return false;
        }
        self.activate((self.active + 1) % self.tabs.len())
    }

    pub fn activate_prev(&mut self) -> bool {
        if self.tabs.len() < 2 {
            return false;
        }
        self.activate((self.active + self.tabs.len() - 1) % self.tabs.len())
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

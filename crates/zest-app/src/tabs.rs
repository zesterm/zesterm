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
}

impl Tab {
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

    /// End this tab's session for good.
    ///
    /// Only meaningful for daemon sessions — an in-process pty dies with its
    /// `Session` drop regardless, which is also what a dead tab needs.
    pub fn kill(self) {
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

    /// The active tab's terminal, which is what nearly every caller wants:
    /// input, rendering, selection and IME all act on what is visible.
    #[must_use]
    pub fn active_source(&self) -> Option<&dyn SessionSource> {
        self.active().map(Tab::source)
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
        let session = Session::spawn(&spec, zest_pty::PtySize::new(10, 4), 10, |_| {})
            .expect("spawn a trivial child");
        Tab::in_process(session, placeholder_addr(n), (10, 4))
    }
}

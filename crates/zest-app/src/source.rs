//! Where the app gets a terminal from.
//!
//! **This is a frozen contract** — see `docs/CONTRACTS.md`. Window chrome,
//! motion, input and the daemon client are all being written against it at the
//! same time, so its shape changing means four things change at once.
//!
//! # Why an abstraction over something the app already owns
//!
//! Today `zest-app` spawns a pty and owns the `Terminal` behind a mutex. That is
//! the right and fastest thing for a local session, and it stays.
//!
//! But the point of the fleet is that a window on this machine can show a shell
//! running on the Mac. That session has no local pty, no local `Terminal` being
//! mutated by a local parser thread — it has a socket delivering grid deltas.
//! Both are "a thing the renderer reads and the keyboard writes to", and this
//! trait is that idea.
//!
//! Retrofitting it later would mean rewriting the event loop, which is where
//! chrome, motion and input all land — a conflict with three workstreams at
//! once. It is here now for the same reason `GlyphInstance` carries absolute
//! pixels: cheap today, structural tomorrow.
//!
//! # What it deliberately does not abstract
//!
//! Not `Terminal` itself. The renderer reads the grid directly under a lock, and
//! putting a trait between the renderer and the cells would either allocate per
//! frame or force every implementation through an iterator that defeats the
//! ~50-150µs extract. A remote session keeps a *real* local `Terminal` that
//! deltas are applied into, so the renderer's path is unchanged whichever end of
//! the mesh the bytes came from.

use std::sync::Arc;

use zest_core::Terminal;

use crate::fair_mutex::FairMutex;
use crate::session::Session;

/// A terminal the app can render and type into, wherever it actually runs.
pub trait SessionSource {
    /// The grid to render.
    ///
    /// A remote session applies deltas into a local `Terminal`, so this is a
    /// real grid in both cases and the renderer never learns the difference.
    fn terminal(&self) -> &Arc<FairMutex<Terminal>>;

    /// Send already-encoded terminal bytes.
    ///
    /// Encoding happens at the keyboard, not at the session, because modifier
    /// and keymap conventions belong to the platform that produced the
    /// keystroke — a Mac session driven from a Windows keyboard should follow
    /// Windows conventions.
    fn write(&self, bytes: Vec<u8>);

    /// The rendered viewport changed size.
    fn resize(&self, cols: u16, rows: u16);

    /// Take the redraw flag, clearing it.
    ///
    /// The 0%-idle guarantee runs through here: a frame is only drawn when this
    /// returns true.
    fn take_dirty(&self) -> bool;

    /// Force a redraw on the next frame.
    fn mark_dirty(&self);

    /// Where this session actually runs.
    ///
    /// Surfaced rather than hidden, so a slow keystroke has an explanation. See
    /// [`Origin`].
    fn origin(&self) -> Origin {
        Origin::InProcess
    }
}

// Deliberately *not* on this trait: `has_exited`. Nothing calls it — exit
// arrives as a `Wakeup::Exited` event — and a contract three workstreams build
// against should carry only what is actually used. Speculative methods are how
// an interface becomes something implementers have to satisfy without knowing
// why.

/// Where a session's shell is running.
///
/// Surfaced rather than hidden: a user typing into a window should be able to
/// tell whether the shell is on this machine or three hundred miles away, and a
/// terminal that hides that is one where a slow keystroke has no explanation.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "Daemon is the variant WS-F constructs; the enum is already returned by origin()"
)]
pub enum Origin {
    /// A pty owned by this process.
    InProcess,
    /// A daemon, local or remote, named for display.
    Daemon { host: String, local: bool },
}

impl Origin {
    /// Whether input should expect perceptible delay.
    #[must_use]
    #[allow(dead_code, reason = "the chrome and the daemon client are both written against this")]
    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Daemon { local: false, .. })
    }
}

impl SessionSource for Session {
    fn terminal(&self) -> &Arc<FairMutex<Terminal>> {
        &self.terminal
    }

    fn write(&self, bytes: Vec<u8>) {
        Session::write(self, bytes);
    }

    fn resize(&self, cols: u16, rows: u16) {
        Session::resize(self, cols, rows);
    }

    fn take_dirty(&self) -> bool {
        Session::take_dirty(self)
    }

    fn mark_dirty(&self) {
        Session::mark_dirty(self);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_local_session_is_not_remote() {
        assert!(!Origin::InProcess.is_remote());
        assert!(!Origin::Daemon { host: "this".into(), local: true }.is_remote());
    }

    #[test]
    fn a_session_on_another_machine_is_remote() {
        // The distinction the UI needs in order to explain a slow keystroke
        // rather than leaving it as an unexplained bad feeling.
        assert!(Origin::Daemon { host: "andy-mac".into(), local: false }.is_remote());
    }

    /// The existing local session satisfies the trait.
    ///
    /// Compile-time only, and that is the point: if `Session` ever stops fitting
    /// the shape the daemon client is also built against, the two have diverged
    /// and this stops building.
    #[test]
    fn the_local_session_implements_the_contract() {
        fn assert_impl<T: SessionSource>() {}
        assert_impl::<Session>();
    }
}

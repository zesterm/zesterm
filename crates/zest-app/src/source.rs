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
use zest_proto::{Key, Policy};

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

    /// Ask the session's host what directories `path` holds (#439).
    ///
    /// `true` means the question is on the wire and a
    /// [`crate::session::Wakeup::DirListingReady`] will follow; `false` —
    /// the default, and every in-process source's answer — means there is
    /// no host to ask and the caller lists the local filesystem itself,
    /// which for an in-process session is the host's filesystem (#434's
    /// rule).
    fn request_dirs(&self, _path: &str) -> bool {
        false
    }

    /// Take the parked answer to [`Self::request_dirs`], clearing it.
    fn take_dir_listing(&self) -> Option<crate::session::DirListing> {
        None
    }

    /// Ask the session's host to read a file, for a pane showing one (#464).
    ///
    /// [`Self::request_dirs`]'s shape and its bargain: `true` means the
    /// question is on the wire and a
    /// [`crate::session::Wakeup::FileContentsReady`] will follow; `false` is
    /// every in-process source's answer, and means the caller reads the file
    /// itself — which for an in-process session *is* the host's filesystem
    /// (#434). `cwd` is what a relative `path` resolves against, on that host.
    fn request_file(&self, _path: &str, _cwd: &str) -> bool {
        false
    }

    /// Take the parked answer to [`Self::request_file`], clearing it.
    fn take_file_contents(&self) -> Option<crate::editor::FileReply> {
        None
    }

    /// Where this session actually runs.
    ///
    /// Surfaced rather than hidden, so a slow keystroke has an explanation. See
    /// [`Origin`].
    fn origin(&self) -> Origin {
        Origin::InProcess
    }

    /// Pull the page of history before the oldest row this session holds,
    /// and say whether more is coming (#545).
    ///
    /// The default is [`HistoryState::Settled`], which is the honest answer
    /// for an in-process session: the parser writes into this very grid, so
    /// there is no host holding rows it has not sent. A replica overrides
    /// it — a keyframe is a *viewport*, so everything that scrolled past
    /// before this window attached exists only on the daemon, and until
    /// something asks, ⌘F and a scroll to the top both stop at whatever
    /// happened to arrive.
    ///
    /// Called repeatedly — once a frame while the find bar is open, and on
    /// a scroll that reaches the top — so it is the implementation's job to
    /// keep one request in flight and to stop when the host has no more.
    fn backfill_history(&self) -> HistoryState {
        HistoryState::Settled
    }

    /// A keystroke is about to be written: guess its echo, if this session
    /// guesses at all. Called *before* `write`, with the key as the keyboard
    /// knew it — the predictor never un-encodes bytes. A local pty never
    /// guesses: its echo is a lock away, and a guess would only ever be a
    /// flicker ahead of the truth. → ADR-016.
    fn predict(&self, _key: Key, _policy: Policy) {}

    /// The guesses still standing, and where the caret belongs while they do.
    /// `None` for a session that never guesses, and for one with nothing to
    /// show — so the renderer's ordinary path is untouched in both cases.
    fn predicted(&self, _policy: Policy) -> Option<PredictedEcho> {
        None
    }

    /// Whether a guess is standing — the frame scheduler's question, answered
    /// without building the overlay it would otherwise have to build twice a
    /// frame.
    fn predicting(&self, _policy: Policy) -> bool {
        false
    }
}

/// Guessed echo, as the renderer wants it: owned, because the predictor lives
/// behind the reader thread's lock and a frame must not hold that.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PredictedEcho {
    pub cells: Vec<zest_render_wgpu::PredictedCell>,
    pub caret: (u16, u16),
}

/// Whether more of this session's history is on its way (#545).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HistoryState {
    /// Nothing is being fetched: an in-process session, a replica that has
    /// drained its host, one holding as much as it will keep, or a link
    /// that is down. Deliberately not called "complete" — this window can
    /// say what it is *doing*, and only a live link could say what the host
    /// still has.
    #[default]
    Settled,
    /// A page is on the wire, and another may follow it.
    Fetching,
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

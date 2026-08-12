//! Watching a terminal change, without being the renderer.
//!
//! **Frozen contract.** The daemon and the protocol are built against this
//! shape, so changing it is a coordinated change — see `docs/CONTRACTS.md`.
//!
//! # The problem it solves
//!
//! The native renderer reads the whole visible grid every frame, which is right
//! for it: it has to draw all of it anyway, and the extract is ~50-150µs under
//! the lock. A remote client cannot work that way. Sending the full grid at
//! 60 Hz over a phone link is absurd, so someone has to answer *"what changed
//! since sequence N"* — and only the terminal can, because only it saw the
//! writes.
//!
//! # Why acknowledgement is in the contract
//!
//! A subscriber states the highest sequence it has successfully applied. That
//! single number is the entire resync mechanism: if the gap is small the host
//! sends deltas, and if it is large — a phone that was in a tunnel for a
//! minute — it sends a keyframe instead, because at some point the delta chain
//! is bigger than the state it describes.
//!
//! Without acknowledgement the host cannot know which of those to send, and the
//! usual fallback is to keep every delta forever "just in case". A terminal that
//! leaks history because a client went to sleep is a terminal that dies on the
//! commute.

use crate::grid::LineId;

/// A subscriber's position in the session's history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscriberId(pub u32);

/// What a subscriber should be sent next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Update {
    /// Nothing has changed since their acknowledgement.
    Idle,
    /// A delta chain from their acknowledged sequence is available.
    Delta { from: u64, to: u64 },
    /// They have fallen too far behind; send the whole state.
    ///
    /// Not an error, and not rare — a phone that was in a tunnel, a laptop that
    /// slept, a browser tab that was backgrounded. Treating it as exceptional is
    /// how reconnection code ends up untested.
    Keyframe { at: u64 },
}

/// How far behind a subscriber may fall before a keyframe is cheaper.
///
/// A guess, deliberately: the honest measure is "bytes of delta versus bytes of
/// keyframe", which needs both encoded first. This is the cheap approximation,
/// and the constant is here rather than inline so a benchmark can replace it
/// with a measured one instead of it being scattered across the daemon.
pub const KEYFRAME_THRESHOLD: u64 = 256;

/// Reading changes out of a terminal.
///
/// A trait rather than inherent methods so the daemon and its tests can run
/// against a fake that produces a scripted sequence of updates without a pty.
/// That is what makes the chaos-resync test — a disconnect at every point of
/// every recording — possible at all.
pub trait ChangeSource {
    /// The current state number. Bumped on every mutation.
    fn seq(&self) -> u64;

    /// What this subscriber needs, given what it has applied.
    fn update_for(&self, acked: u64) -> Update;

    /// The oldest line still held, so a client knows what it can still request.
    fn oldest_line(&self) -> LineId;
}

// Deliberately *not* on this trait: `release_before`.
//
// It was here when the plan assumed the terminal would retain a delta history
// that subscribers acknowledged their way through. The encoder instead keeps a
// shadow of what each subscriber last saw and diffs against it
// (`zest-proto::encode`), so there is no shared history and nothing to release.
//
// Removed rather than left as a no-op: a memory-management method that manages
// nothing tells every caller that memory *is* being managed, which is exactly
// the belief that stops someone checking. The property it was protecting —
// a detached session on a machine nobody has opened in a week must not grow
// without bound — now holds by construction, because per-subscriber state
// disappears when the subscriber does.

/// Decide what to send, given a subscriber's acknowledgement.
///
/// Free function rather than a default method so the daemon and its tests share
/// exactly one copy of this rule — a fake that made the decision differently
/// would test the fake.
#[must_use]
pub fn update_for(current_seq: u64, acked: u64) -> Update {
    if acked >= current_seq {
        return Update::Idle;
    }
    // A subscriber ahead of the host has been talking to a different session --
    // a daemon restart, or a client that kept state across a host it should not
    // have. A keyframe is the only honest answer.
    if current_seq.saturating_sub(acked) > KEYFRAME_THRESHOLD {
        Update::Keyframe { at: current_seq }
    } else {
        Update::Delta { from: acked, to: current_seq }
    }
}

impl ChangeSource for crate::Terminal {
    fn seq(&self) -> u64 {
        crate::Terminal::seq(self)
    }

    fn update_for(&self, acked: u64) -> Update {
        update_for(crate::Terminal::seq(self), acked)
    }

    fn oldest_line(&self) -> LineId {
        // The first line still held, which is where scrollback begins. A client
        // asking for anything older is asking for something that has been
        // evicted, and needs to be told so rather than handed blanks.
        // Active space, for the same reason as `oldest_retained_line`: a client
        // is asking what the host still holds, not where anyone is looking.
        self.grid().active_row(0).id.saturating_sub(self.grid().scrollback_len() as u64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_caught_up_subscriber_is_sent_nothing() {
        // The 0%-idle guarantee extended across the network: an idle terminal
        // must not generate traffic any more than it generates frames.
        assert_eq!(update_for(100, 100), Update::Idle);
    }

    #[test]
    fn a_slightly_behind_subscriber_gets_deltas() {
        assert_eq!(update_for(110, 100), Update::Delta { from: 100, to: 110 });
    }

    #[test]
    fn a_subscriber_that_slept_gets_a_keyframe() {
        // The phone-in-a-tunnel case. Normal, not exceptional.
        assert_eq!(
            update_for(100_000, 100),
            Update::Keyframe { at: 100_000 },
            "a delta chain this long is larger than the state it describes"
        );
    }

    #[test]
    fn a_subscriber_ahead_of_the_host_is_resynced_rather_than_trusted() {
        // Happens after a daemon restart. Treating their sequence as valid would
        // mean sending deltas against a base the host does not have.
        assert_eq!(update_for(5, 900), Update::Idle);
    }

    #[test]
    fn the_threshold_is_the_only_place_the_tradeoff_lives() {
        let just_under = update_for(KEYFRAME_THRESHOLD + 1, 1);
        assert!(matches!(just_under, Update::Delta { .. }));
        let just_over = update_for(KEYFRAME_THRESHOLD + 2, 1);
        assert!(matches!(just_over, Update::Keyframe { .. }));
    }
}

#[cfg(test)]
mod terminal_impl_tests {
    use super::*;
    use crate::Terminal;

    #[test]
    fn a_terminal_that_has_not_changed_needs_nothing_sent() {
        let mut t = Terminal::new(20, 5, 100);
        t.advance(b"hello");
        let seq = ChangeSource::seq(&t);
        assert_eq!(ChangeSource::update_for(&t, seq), Update::Idle);
    }

    #[test]
    fn writing_moves_the_sequence_forward() {
        let mut t = Terminal::new(20, 5, 100);
        let before = ChangeSource::seq(&t);
        t.advance(b"x");
        assert!(ChangeSource::seq(&t) > before, "a write did not bump the sequence");
        assert!(matches!(ChangeSource::update_for(&t, before), Update::Delta { .. }));
    }

    #[test]
    fn the_oldest_line_is_where_scrollback_begins() {
        // What a client is told it may still ask for. Reporting the viewport's
        // first line instead would make every scrollback request past the top of
        // the screen look unavailable.
        let mut t = Terminal::new(20, 3, 100);
        for i in 0..10 {
            t.advance(format!("line{i}\r\n").as_bytes());
        }
        let oldest = ChangeSource::oldest_line(&t);
        let viewport_top = t.grid().row(0).id;
        assert!(
            oldest < viewport_top,
            "oldest {oldest} should precede the viewport top {viewport_top}"
        );
    }

    #[test]
    fn an_evicted_session_still_reports_a_sane_oldest_line() {
        // Past the scrollback limit the oldest line advances rather than going
        // negative or wrapping, which a saturating subtraction is there to
        // guarantee -- LineId is unsigned.
        let mut t = Terminal::new(20, 3, 2);
        for i in 0..50 {
            t.advance(format!("l{i}\r\n").as_bytes());
        }
        let oldest = ChangeSource::oldest_line(&t);
        assert!(oldest > 0, "scrollback evicted but oldest_line stayed at 0");
        assert!(oldest <= t.grid().row(0).id);
    }
}

//! Watching a terminal change, without being the renderer.
//!
//! **Frozen contract, skeleton implementation.** WS-F fills this in; it is here
//! so the daemon and the protocol can be written against a settled shape.
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
/// Implemented on `Terminal` by WS-F. Kept as a trait so the daemon and its
/// tests can be written against a fake that produces a scripted sequence of
/// updates without a pty, which is what makes the chaos-resync test — ten
/// thousand disconnects at random points — possible at all.
pub trait ChangeSource {
    /// The current state number. Bumped on every mutation.
    fn seq(&self) -> u64;

    /// What this subscriber needs, given what it has applied.
    fn update_for(&self, acked: u64) -> Update;

    /// The oldest line still held, so a client knows what it can still request.
    fn oldest_line(&self) -> LineId;

    /// Forget history no subscriber still needs.
    ///
    /// Called with the minimum acknowledgement across all subscribers. A session
    /// with no subscribers keeps nothing, which is what stops a detached session
    /// on a machine nobody has opened in a week from growing without bound.
    fn release_before(&mut self, seq: u64);
}

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

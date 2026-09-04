//! The find bar's state, as data.
//!
//! Pure: no window, no GPU, no terminal lock. The app hands it the hits a scan
//! produced and it answers which one the keyboard is on and what the counter
//! reads — so stepping, wrapping and the honest-count rules live under
//! `cargo test` rather than behind a rendered frame.
//!
//! The query text itself is a [`TextField`](crate::text_field::TextField) held
//! beside this, not in it: caret, selection and every clipboard chord belong to
//! that one owner (AGENTS.md § App UI), and a second copy here is how a field
//! ends up eating ⌘V.

use zest_core::search::{Match, Matches};

/// Which pane's grid the open bar is searching, and what it found there.
#[derive(Default)]
pub struct FindState {
    /// Hits from the last scan, oldest first.
    pub hits: Vec<Match>,
    /// Index into `hits` the keyboard is on. `None` when there are none.
    pub current: Option<usize>,
    /// The scan stopped at its cap.
    pub truncated: bool,
    /// Whether the query was taken case-sensitively, for the `Aa` chip.
    pub case_sensitive: bool,
    /// A page of this session's history is on the wire (#545). The count is
    /// still growing, so the bar says so rather than letting a number that
    /// is about to change read as the final answer.
    pub fetching: bool,
}

impl FindState {
    /// Take a fresh scan's answer, keeping the reader as close to where they
    /// were as the new hits allow.
    ///
    /// `near` is the line the viewport is showing. Re-anchoring on it rather
    /// than resetting to the first hit is what makes typing another letter feel
    /// like narrowing a search instead of starting one.
    pub fn accept(&mut self, found: Matches, near: Option<u64>) {
        self.truncated = found.truncated;
        self.hits = found.hits;
        self.current = self.nearest(near);
    }

    /// The hit closest to `line`, at or after it where possible.
    ///
    /// Opening the bar while scrolled back must land on what is under the
    /// reader's eyes, not on the top of a scrollback they navigated away from.
    #[must_use]
    pub fn nearest(&self, line: Option<u64>) -> Option<usize> {
        if self.hits.is_empty() {
            return None;
        }
        let Some(line) = line else { return Some(0) };
        // At or below the viewport's first line, else the last hit above it —
        // reading runs downwards, so the next thing you want is ahead of you.
        self.hits
            .iter()
            .position(|h| h.start.line >= line)
            .or(Some(self.hits.len() - 1))
    }

    /// Move `delta` hits, wrapping at both ends.
    ///
    /// Wrapping rather than stopping: a find bar that goes dead at the last hit
    /// makes you reopen it to check whether that was really the last one.
    pub fn step(&mut self, delta: isize) {
        if self.hits.is_empty() {
            self.current = None;
            return;
        }
        let len = self.hits.len() as isize;
        let at = self.current.unwrap_or(0) as isize;
        self.current = Some(((at + delta).rem_euclid(len)) as usize);
    }

    /// The hit the keyboard is on.
    #[must_use]
    pub fn selected(&self) -> Option<&Match> {
        self.hits.get(self.current?)
    }

    /// What the counter reads.
    ///
    /// Never `0 of 0`: a bar showing two zeroes reads as a broken counter
    /// rather than as an answer, and "no results" is a fact worth stating in
    /// words. A truncated scan says so too — a count that stops at a round
    /// number without admitting it reads as the whole truth.
    #[must_use]
    pub fn count_label(&self, query_empty: bool) -> String {
        if query_empty {
            return String::new();
        }
        if self.hits.is_empty() {
            return "No results".to_string();
        }
        let at = self.current.map_or(1, |i| i + 1);
        let total = self.hits.len();
        if self.truncated {
            format!("{at} of {total}+")
        } else {
            format!("{at} of {total}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zest_core::AbsPos;

    fn hits(lines: &[u64]) -> Vec<Match> {
        lines
            .iter()
            .map(|l| Match { start: AbsPos::new(*l, 0), end: AbsPos::new(*l, 2) })
            .collect()
    }

    fn state(lines: &[u64]) -> FindState {
        FindState { hits: hits(lines), current: Some(0), ..FindState::default() }
    }

    #[test]
    fn stepping_wraps_at_both_ends() {
        // A bar that goes dead at the last hit makes you reopen it to find out
        // whether that really was the last one.
        let mut s = state(&[1, 2, 3]);
        s.step(1);
        s.step(1);
        assert_eq!(s.current, Some(2), "walked to the end");
        s.step(1);
        assert_eq!(s.current, Some(0), "and wrapped forwards");
        s.step(-1);
        assert_eq!(s.current, Some(2), "and backwards");
    }

    #[test]
    fn opening_while_scrolled_back_lands_on_the_nearest_hit_not_the_first() {
        // Someone who scrolled to line 50 and pressed ⌘F is asking about what
        // is in front of them. Jumping to hit #1 throws away the navigation
        // they just did.
        let s = state(&[1, 40, 60, 90]);
        assert_eq!(s.nearest(Some(50)), Some(2), "the first hit at or below the viewport");
    }

    #[test]
    fn a_viewport_below_every_hit_lands_on_the_last_one() {
        // Reading runs downwards, so when nothing is ahead the nearest thing is
        // the last one behind -- not a wrap to the top, which would read as the
        // bar having ignored the question.
        let s = state(&[1, 2, 3]);
        assert_eq!(s.nearest(Some(900)), Some(2));
    }

    #[test]
    fn the_count_never_says_zero_of_zero() {
        // Two zeroes read as a broken counter, not as an answer.
        let s = FindState::default();
        assert_eq!(s.count_label(false), "No results");
        assert_eq!(s.count_label(true), "", "and an empty query says nothing at all");
    }

    #[test]
    fn a_truncated_count_admits_it() {
        // A count that stops at the cap without saying so reads as the whole
        // truth, and the number it shows is the one thing it is not.
        let mut s = state(&[1, 2, 3]);
        s.truncated = true;
        assert_eq!(s.count_label(false), "1 of 3+");
    }

    #[test]
    fn accepting_a_scan_with_no_hits_leaves_nothing_selected() {
        let mut s = state(&[1, 2]);
        s.accept(Matches::default(), Some(1));
        assert_eq!(s.current, None, "nothing to be on");
        assert!(s.selected().is_none());
        assert_eq!(s.count_label(false), "No results");
    }

    #[test]
    fn stepping_with_no_hits_selects_nothing_rather_than_panicking() {
        let mut s = FindState::default();
        s.step(1);
        assert_eq!(s.current, None);
    }
}

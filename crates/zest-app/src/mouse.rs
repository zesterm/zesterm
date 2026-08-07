//! Mouse-driven selection.
//!
//! Click counting is done here rather than relying on the OS: winit reports raw
//! press/release, and double- and triple-click need both a time *and* a distance
//! threshold. Without the distance check, a slow drag that happens to pause
//! twice registers as a triple-click and selects the whole line.

use std::time::{Duration, Instant};

use zest_core::{AbsPos, Selection, SelectionMode};

/// Maximum gap between clicks to count as a multi-click.
///
/// Windows' own default is 500ms; matching it keeps the terminal feeling like
/// the rest of the desktop.
const MULTI_CLICK_INTERVAL: Duration = Duration::from_millis(500);

/// Maximum movement between clicks, in cells.
const MULTI_CLICK_DISTANCE: usize = 1;

#[derive(Default)]
pub struct MouseState {
    last_click: Option<(Instant, usize, usize)>,
    click_count: u8,
    dragging: bool,
}

impl MouseState {
    /// Register a press and return the selection mode it implies.
    pub fn press(&mut self, row: usize, col: usize) -> SelectionMode {
        let now = Instant::now();

        let consecutive = self.last_click.is_some_and(|(t, r, c)| {
            now.duration_since(t) < MULTI_CLICK_INTERVAL
                && r.abs_diff(row) <= MULTI_CLICK_DISTANCE
                && c.abs_diff(col) <= MULTI_CLICK_DISTANCE
        });

        self.click_count = if consecutive { (self.click_count % 3) + 1 } else { 1 };
        self.last_click = Some((now, row, col));
        self.dragging = true;

        match self.click_count {
            2 => SelectionMode::Word,
            3 => SelectionMode::Line,
            _ => SelectionMode::Simple,
        }
    }

    pub fn release(&mut self) {
        self.dragging = false;
    }

    #[must_use]
    pub fn is_dragging(&self) -> bool {
        self.dragging
    }
}

/// Build the selection for a press.
///
/// Double-click expands to the word under the pointer immediately, so the
/// highlight appears on the click rather than waiting for a drag.
#[must_use]
pub fn begin(
    term: &zest_core::Terminal,
    pos: AbsPos,
    mode: SelectionMode,
    block: bool,
) -> Selection {
    let mode = if block { SelectionMode::Block } else { mode };

    match mode {
        SelectionMode::Word => {
            let (start, end) = term.word_at(pos);
            Selection { anchor: start, head: end, mode: SelectionMode::Word }
        }
        SelectionMode::Line => Selection { anchor: pos, head: pos, mode: SelectionMode::Line },
        m => Selection::new(pos, m),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_single_click_is_a_simple_selection() {
        let mut m = MouseState::default();
        assert_eq!(m.press(5, 10), SelectionMode::Simple);
        assert!(m.is_dragging());
        m.release();
        assert!(!m.is_dragging());
    }

    #[test]
    fn clicks_in_place_escalate_to_word_then_line() {
        let mut m = MouseState::default();
        assert_eq!(m.press(5, 10), SelectionMode::Simple);
        assert_eq!(m.press(5, 10), SelectionMode::Word);
        assert_eq!(m.press(5, 10), SelectionMode::Line);
        // A fourth click wraps back rather than staying on Line forever, which
        // is what every other terminal does.
        assert_eq!(m.press(5, 10), SelectionMode::Simple);
    }

    #[test]
    fn moving_between_clicks_starts_a_new_count() {
        // The bug this guards: without a distance check, a slow drag that pauses
        // twice registers as a triple-click and selects the whole line.
        let mut m = MouseState::default();
        assert_eq!(m.press(5, 10), SelectionMode::Simple);
        assert_eq!(m.press(5, 40), SelectionMode::Simple, "a click far away is not a double");
        assert_eq!(m.press(12, 40), SelectionMode::Simple, "nor is one on another row");
    }

    #[test]
    fn one_cell_of_jitter_still_counts_as_a_double_click() {
        // Hands are not steady; requiring an exact cell match makes
        // double-clicking feel broken.
        let mut m = MouseState::default();
        assert_eq!(m.press(5, 10), SelectionMode::Simple);
        assert_eq!(m.press(5, 11), SelectionMode::Word);
    }

    #[test]
    fn a_slow_second_click_starts_over() {
        let mut m = MouseState::default();
        m.press(5, 10);
        // Simulate the interval elapsing by rewinding the recorded time.
        m.last_click = Some((Instant::now() - Duration::from_millis(600), 5, 10));
        assert_eq!(m.press(5, 10), SelectionMode::Simple);
    }

    #[test]
    fn alt_forces_a_block_selection_regardless_of_click_count() {
        let mut t = zest_core::Terminal::new(20, 5, 10);
        t.advance(b"hello world");
        let pos = t.abs_pos(0, 2).unwrap();

        let s = begin(&t, pos, SelectionMode::Word, true);
        assert_eq!(s.mode, SelectionMode::Block);
    }

    #[test]
    fn double_click_highlights_the_word_without_dragging() {
        let mut t = zest_core::Terminal::new(40, 5, 10);
        t.advance(b"run /usr/local/bin now");
        let pos = t.abs_pos(0, 8).unwrap();

        let s = begin(&t, pos, SelectionMode::Word, false);
        assert!(!s.is_empty(), "the word should be selected on the click itself");
        assert_eq!(t.grid().selection_text(&s), "/usr/local/bin");
    }
}

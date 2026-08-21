//! Text selection.
//!
//! # Absolute coordinates, not viewport ones
//!
//! A selection is anchored to [`LineId`]s — monotonic ids counted from the first
//! line ever written — rather than to viewport rows. That is what lets a
//! selection survive scrolling, new output arriving, and the selected lines
//! eventually falling out of scrollback entirely.
//!
//! Storing viewport rows instead is the obvious implementation and it breaks
//! immediately: the moment a line of output arrives, every row index shifts by
//! one and the highlight slides up the screen while the user is still dragging.

use alloc::string::String;
use alloc::vec::Vec;

use crate::cell::CellFlags;
use crate::grid::{Grid, LineId};

/// A position in the scrollback, independent of what is on screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct AbsPos {
    pub line: LineId,
    pub col: usize,
}

impl AbsPos {
    #[must_use]
    pub const fn new(line: LineId, col: usize) -> Self {
        Self { line, col }
    }
}

/// How a drag expands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SelectionMode {
    /// Character-by-character.
    #[default]
    Simple,
    /// Whole words — double-click.
    Word,
    /// Whole lines — triple-click.
    Line,
    /// A rectangle rather than a text flow — Alt-drag.
    Block,
}

/// An in-progress or completed selection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    /// Where the drag started. Stays put as the head moves.
    pub anchor: AbsPos,
    /// Where the pointer is now.
    pub head: AbsPos,
    pub mode: SelectionMode,
}

impl Selection {
    #[must_use]
    pub fn new(at: AbsPos, mode: SelectionMode) -> Self {
        Self { anchor: at, head: at, mode }
    }

    /// Ordered endpoints, regardless of drag direction.
    #[must_use]
    pub fn range(&self) -> (AbsPos, AbsPos) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// True if the selection covers no cells.
    ///
    /// A click without a drag produces an empty selection, and that must not
    /// clobber the clipboard.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.mode == SelectionMode::Simple && self.anchor == self.head
    }

    /// The selected span on a given line, as a half-open column range.
    ///
    /// `None` when the line is outside the selection.
    #[must_use]
    pub fn span_on(&self, line: LineId, cols: usize) -> Option<(usize, usize)> {
        let (start, end) = self.range();

        if self.mode == SelectionMode::Block {
            // A rectangle uses the same column range on every line it covers,
            // independent of where the drag started and ended horizontally.
            if line < start.line || line > end.line {
                return None;
            }
            let (lo, hi) = if self.anchor.col <= self.head.col {
                (self.anchor.col, self.head.col)
            } else {
                (self.head.col, self.anchor.col)
            };
            return Some((lo.min(cols), (hi + 1).min(cols)));
        }

        if line < start.line || line > end.line {
            return None;
        }
        if self.mode == SelectionMode::Line {
            return Some((0, cols));
        }

        // A text-flow selection runs to the end of every line but the last, and
        // from the start of every line but the first.
        let from = if line == start.line { start.col } else { 0 };
        let to = if line == end.line { (end.col + 1).min(cols) } else { cols };
        (from < to).then_some((from, to))
    }
}

/// Characters that end a word for double-click expansion.
///
/// Deliberately excludes `/`, `.`, `-` and `_`: double-clicking a path or an
/// identifier should select the whole thing, which is what makes the gesture
/// useful in a terminal at all.
const WORD_SEPARATORS: &[char] = &[
    ' ', '\t', '"', '\'', '`', '(', ')', '[', ']', '{', '}', '<', '>', ',', ';', ':', '=', '|',
    '&', '!', '?', '*', '\0',
];

fn is_separator(ch: char) -> bool {
    WORD_SEPARATORS.contains(&ch)
}

impl Grid {
    /// The absolute id of a viewport row. Display space — what the reader is
    /// pointing at, which is what a selection means.
    #[must_use]
    pub fn line_id_at(&self, row: usize) -> Option<LineId> {
        (row < self.rows()).then(|| self.row(row).id)
    }

    /// The absolute id of a row of the *live* screen. Active space.
    ///
    /// What the parser means by "the cursor's line": a shell emitting an OSC
    /// 133 marker is naming the row it is printing on, never the row someone
    /// happens to have scrolled to.
    #[must_use]
    pub fn active_line_id_at(&self, row: usize) -> Option<LineId> {
        (row < self.rows()).then(|| self.active_row(row).id)
    }

    /// Where a line currently sits in the viewport, if it is visible.
    #[must_use]
    pub fn row_of_line(&self, line: LineId) -> Option<usize> {
        (0..self.rows()).find(|&r| self.row(r).id == line)
    }

    /// Expand a position to the word around it.
    #[must_use]
    pub fn word_at(&self, pos: AbsPos) -> (AbsPos, AbsPos) {
        let Some(row) = self.row_of_line(pos.line) else { return (pos, pos) };
        let r = self.row(row);
        let cols = self.cols();

        let at = |c: usize| r.get(c).map_or(' ', |cell| cell.ch);
        if pos.col >= cols || is_separator(at(pos.col)) {
            return (pos, pos);
        }

        let mut start = pos.col;
        while start > 0 && !is_separator(at(start - 1)) {
            start -= 1;
        }
        let mut end = pos.col;
        while end + 1 < cols && !is_separator(at(end + 1)) {
            end += 1;
        }
        (AbsPos::new(pos.line, start), AbsPos::new(pos.line, end))
    }

    /// The text covered by a selection.
    ///
    /// Rows that wrapped are joined without a newline. Inserting one would mean
    /// that copying a long command and pasting it back runs two broken
    /// fragments instead of the command — the single most annoying selection bug
    /// a terminal can have.
    #[must_use]
    pub fn selection_text(&self, sel: &Selection) -> String {
        if sel.is_empty() {
            return String::new();
        }

        let cols = self.cols();
        let (start, end) = sel.range();
        let mut out = String::new();
        let mut pending: Vec<(String, bool)> = Vec::new();

        // Walk *all* retained lines, not just the visible ones. A selection can
        // extend into scrollback -- and it stays valid as new output pushes the
        // selected lines off the top, which is the entire point of anchoring to
        // absolute ids.
        for index in 0..self.total_lines() {
            let Some(r) = self.line(index) else { continue };
            if r.id < start.line || r.id > end.line {
                continue;
            }
            let Some((from, to)) = sel.span_on(r.id, cols) else { continue };

            let mut text = String::new();
            for col in from..to {
                let Some(cell) = r.get(col) else { break };
                if cell.flags.contains(CellFlags::WIDE_SPACER) {
                    continue;
                }
                text.push(cell.ch);
                if let Some(extra) = r.extra(cell) {
                    text.extend(extra.zerowidth.iter().copied());
                }
            }

            // Trailing blanks on a full-width span are padding, not content.
            // A block selection keeps them, because a rectangle's whole point is
            // preserving column alignment.
            if sel.mode != SelectionMode::Block {
                while text.ends_with(' ') {
                    text.pop();
                }
            }

            pending.push((text, r.wrapped()));
        }

        for (i, (text, wrapped)) in pending.iter().enumerate() {
            out.push_str(text);
            let last = i + 1 == pending.len();
            if !last && !wrapped {
                out.push('\n');
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::Cell;

    fn grid_with(lines: &[&str], cols: usize) -> Grid {
        let mut g = Grid::new(cols, lines.len().max(1), 100);
        for (row, text) in lines.iter().enumerate() {
            for (col, ch) in text.chars().enumerate() {
                if let Some(c) = g.cell_mut(row, col) {
                    c.ch = ch;
                }
            }
        }
        g
    }

    fn sel(g: &Grid, from: (usize, usize), to: (usize, usize), mode: SelectionMode) -> Selection {
        let a = AbsPos::new(g.line_id_at(from.0).unwrap(), from.1);
        let b = AbsPos::new(g.line_id_at(to.0).unwrap(), to.1);
        Selection { anchor: a, head: b, mode }
    }

    #[test]
    fn a_click_without_a_drag_selects_nothing() {
        // Must not clobber the clipboard on a stray click.
        let g = grid_with(&["hello"], 10);
        let s = sel(&g, (0, 2), (0, 2), SelectionMode::Simple);
        assert!(s.is_empty());
        assert_eq!(g.selection_text(&s), "");
    }

    #[test]
    fn dragging_backwards_selects_the_same_text() {
        let g = grid_with(&["hello world"], 20);
        let forward = sel(&g, (0, 0), (0, 4), SelectionMode::Simple);
        let backward = sel(&g, (0, 4), (0, 0), SelectionMode::Simple);
        assert_eq!(g.selection_text(&forward), "hello");
        assert_eq!(g.selection_text(&backward), "hello");
    }

    #[test]
    fn a_multi_line_selection_spans_full_intermediate_lines() {
        let g = grid_with(&["first", "second", "third"], 10);
        let s = sel(&g, (0, 3), (2, 1), SelectionMode::Simple);
        assert_eq!(g.selection_text(&s), "st\nsecond\nth");
    }

    #[test]
    fn wrapped_lines_join_without_a_newline() {
        // The bug this guards: copying a wrapped command and pasting it back
        // would run two broken fragments instead of the command.
        let mut g = grid_with(&["echo hello", "world"], 10);
        g.set_wrapped(0, true);

        let s = sel(&g, (0, 0), (1, 4), SelectionMode::Simple);
        assert_eq!(g.selection_text(&s), "echo helloworld");
    }

    #[test]
    fn unwrapped_lines_keep_their_newline() {
        let g = grid_with(&["one", "two"], 10);
        let s = sel(&g, (0, 0), (1, 2), SelectionMode::Simple);
        assert_eq!(g.selection_text(&s), "one\ntwo");
    }

    #[test]
    fn trailing_padding_is_trimmed() {
        // Cells past the text are blanks, not content; copying them would paste
        // a wall of spaces.
        let g = grid_with(&["hi", "there"], 40);
        let s = sel(&g, (0, 0), (1, 39), SelectionMode::Simple);
        assert_eq!(g.selection_text(&s), "hi\nthere");
    }

    #[test]
    fn line_mode_takes_whole_lines_regardless_of_columns() {
        let g = grid_with(&["alpha", "beta"], 10);
        let s = sel(&g, (0, 3), (1, 1), SelectionMode::Line);
        assert_eq!(g.selection_text(&s), "alpha\nbeta");
    }

    #[test]
    fn block_mode_takes_the_same_columns_from_every_line() {
        let g = grid_with(&["abcdef", "ghijkl", "mnopqr"], 10);
        let s = sel(&g, (0, 1), (2, 3), SelectionMode::Block);
        assert_eq!(g.selection_text(&s), "bcd\nhij\nnop");
    }

    #[test]
    fn block_mode_preserves_alignment_padding() {
        // A rectangle's purpose is column alignment, so trailing blanks inside
        // the rectangle are meaningful and must survive.
        let g = grid_with(&["ab", "abcd"], 10);
        let s = sel(&g, (0, 0), (1, 3), SelectionMode::Block);
        assert_eq!(g.selection_text(&s), "ab  \nabcd");
    }

    #[test]
    fn double_click_selects_paths_and_identifiers_whole() {
        // The gesture is only useful if `/usr/local/bin` and `some_name` come
        // out in one piece.
        let g = grid_with(&["run /usr/local/bin/tool now"], 40);
        let line = g.line_id_at(0).unwrap();

        let (a, b) = g.word_at(AbsPos::new(line, 8));
        assert_eq!(a.col, 4);
        assert_eq!(b.col, 22, "the whole path, dots slashes and all");

        let (a, b) = g.word_at(AbsPos::new(line, 1));
        assert_eq!((a.col, b.col), (0, 2), "run");
    }

    #[test]
    fn double_click_on_a_separator_selects_nothing() {
        let g = grid_with(&["a b"], 10);
        let line = g.line_id_at(0).unwrap();
        let pos = AbsPos::new(line, 1);
        assert_eq!(g.word_at(pos), (pos, pos));
    }

    #[test]
    fn wide_characters_contribute_one_char_not_two() {
        let mut g = grid_with(&[""], 10);
        g.cell_mut(0, 0).unwrap().ch = '世';
        g.cell_mut(0, 0).unwrap().flags = CellFlags::WIDE;
        g.cell_mut(0, 1).unwrap().flags = CellFlags::WIDE_SPACER;
        g.cell_mut(0, 2).unwrap().ch = 'x';

        let s = sel(&g, (0, 0), (0, 2), SelectionMode::Simple);
        assert_eq!(g.selection_text(&s), "世x");
    }

    #[test]
    fn selection_survives_scrolling() {
        // The property the whole absolute-coordinate design exists for. With
        // viewport rows, this highlight would slide up the screen as output
        // arrived.
        let mut g = grid_with(&["keep me", "second"], 20);
        let s = sel(&g, (0, 0), (0, 6), SelectionMode::Simple);
        assert_eq!(g.selection_text(&s), "keep me");

        g.scroll_up(1, &Cell::default());
        assert_eq!(
            g.selection_text(&s),
            "keep me",
            "the same text is selected after the screen scrolled"
        );

        // It has left the viewport, so it no longer renders -- but the text is
        // still retrievable, which is what matters for a copy.
        assert_eq!(g.row_of_line(s.anchor.line), None, "scrolled off the top");
    }

    #[test]
    fn a_selection_spanning_scrollback_and_screen_is_whole() {
        let mut g = grid_with(&["first", "second"], 20);
        let first = g.line_id_at(0).unwrap();
        g.scroll_up(1, &Cell::default());

        // "first" is now in scrollback; "second" is on screen.
        let s = Selection {
            anchor: AbsPos::new(first, 0),
            head: AbsPos::new(g.line_id_at(0).unwrap(), 5),
            mode: SelectionMode::Simple,
        };
        assert_eq!(g.selection_text(&s), "first\nsecond");
    }

    #[test]
    fn spans_clamp_to_the_grid_width() {
        let g = grid_with(&["abc"], 5);
        let line = g.line_id_at(0).unwrap();
        let s = Selection {
            anchor: AbsPos::new(line, 0),
            head: AbsPos::new(line, 99),
            mode: SelectionMode::Simple,
        };
        assert_eq!(s.span_on(line, 5), Some((0, 5)));
    }
}

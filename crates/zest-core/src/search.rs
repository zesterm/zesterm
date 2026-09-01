//! Finding text in the grid and its scrollback.
//!
//! # A match is a selection
//!
//! [`Match::highlight`] turns a hit into a [`Selection`], and that is the whole
//! design. The renderer then asks [`Selection::span_on`] which columns to paint
//! on each row, exactly as it does for a drag — so the span algebra deciding
//! what a highlight covers exists once, and a find hit and a selection can never
//! disagree about where a run of text sits. A second copy would drift the first
//! time either learned something about wrapping or wide cells.
//!
//! [`Match`] stays a distinct type rather than *being* a `Selection` because a
//! `Selection` also answers "what does ⌘C copy", and there is one of those and
//! potentially thousands of these.
//!
//! # Logical lines, not rows
//!
//! A command that wrapped is one line to the person reading it, so the scan
//! joins consecutive rows while [`Row::wrapped`] is set and searches the join.
//! That is what lets `helloworld` be found across the seam — and it is also why
//! [`Row::text`] is not simply called per row here: it trims trailing blanks,
//! which is right for the *last* row of a logical line and wrong for the rows
//! before it, where a trailing space is content the next row continues after.
//! Trimming those would glue `"abc "` and `"def"` into `"abcdef"` and report a
//! match that is not on the screen.
//!
//! # No regex, deliberately
//!
//! Not only for the dependency — `regex` needs `std` and this crate builds for
//! `wasm32-unknown-unknown` with `--no-default-features` — but because `^` and
//! `$` would anchor to the *logical* line, whose extent changes when the window
//! is resized. The same pattern would then match different text at different
//! widths, which is a worse thing to explain than its absence. [`Query`] and
//! [`Matches`] are shaped so a pattern kind can be added additively behind an
//! off-by-default feature if that trade ever changes.
//!
//! [`Row::wrapped`]: crate::Row::wrapped
//! [`Row::text`]: crate::Row::text

use alloc::string::String;
use alloc::vec::Vec;

use crate::cell::CellFlags;
use crate::grid::Grid;
use crate::selection::{AbsPos, Selection, SelectionMode};

/// One hit, addressed the way a selection is.
///
/// `end` is **inclusive**, matching [`Selection::head`] — `span_on` adds the one
/// back. `end.line` may exceed `start.line`, which is a match that ran across a
/// wrap.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    pub start: AbsPos,
    pub end: AbsPos,
}

impl Match {
    /// The hit as a selection, for anything that paints or copies spans.
    ///
    /// **Do not ask the result [`Selection::is_empty`].** That answers a drag's
    /// question — a click that did not move selects nothing and must not clobber
    /// the clipboard — and it is true whenever `anchor == head`, which for a
    /// match means a perfectly good *one-cell* hit. A painter that consults it
    /// silently draws nothing for every single-character search, which is one of
    /// the likeliest things anyone types. Ask [`Selection::span_on`] instead: it
    /// is inclusive of `head` and answers correctly for a hit of any length.
    #[must_use]
    pub const fn highlight(&self) -> Selection {
        Selection { anchor: self.start, head: self.end, mode: SelectionMode::Simple }
    }
}

/// What to look for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub needle: String,
    pub case_sensitive: bool,
}

impl Query {
    /// A query that means it when you type a capital.
    ///
    /// The convention every editor uses: a lowercase needle matches anything,
    /// and the moment a capital appears the search takes it literally. It is the
    /// behaviour that needs no checkbox in the common case; the `Aa` toggle
    /// exists for the uncommon one.
    #[must_use]
    pub fn smart(needle: impl Into<String>) -> Self {
        let needle: String = needle.into();
        let case_sensitive = needle.chars().any(char::is_uppercase);
        Self { needle, case_sensitive }
    }

    /// A query that always folds case.
    #[must_use]
    pub fn insensitive(needle: impl Into<String>) -> Self {
        Self { needle: needle.into(), case_sensitive: false }
    }
}

/// What a scan found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Matches {
    /// Hits, oldest line first.
    pub hits: Vec<Match>,
    /// More existed than `hits` holds. Said rather than silently cut: a count
    /// that stops at a round number without admitting it reads as the answer.
    pub truncated: bool,
}

/// Most hits one scan will collect.
///
/// A bound rather than a budget: a scan runs inside a frame, and a needle like
/// `e` over a full scrollback would otherwise build a vector the size of the
/// history before anything was drawn.
pub const DEFAULT_MATCH_LIMIT: usize = 5_000;

/// Compare one character, folding case unless asked not to.
///
/// Per character and never over the whole string, deliberately: full Unicode
/// folding changes length (`ß` folds to `ss`), and this scan maps every
/// character back to the cell it came from. A fold that inserted a character
/// would slide every later column by one and report hits in the wrong place.
fn eq_folded(a: char, b: char, case_sensitive: bool) -> bool {
    if a == b {
        return true;
    }
    if case_sensitive {
        return false;
    }
    a.to_lowercase().eq(b.to_lowercase())
}

/// Find `needle` in one logical line's flattened characters.
///
/// Free rather than a method: it touches no grid, which is what keeps the hit
/// arithmetic testable on its own and stops it reaching for a row lookup.
fn scan_line(
    chars: &[char],
    at: &[AbsPos],
    ends: &[usize],
    needle: &[char],
    case_sensitive: bool,
    limit: usize,
    found: &mut Matches,
) {
    if needle.len() > chars.len() {
        return;
    }
    let mut i = 0;
    while i + needle.len() <= chars.len() {
        let hit = needle
            .iter()
            .enumerate()
            .all(|(k, n)| eq_folded(chars[i + k], *n, case_sensitive));
        if !hit {
            i += 1;
            continue;
        }
        let last = i + needle.len() - 1;
        found.hits.push(Match { start: at[i], end: AbsPos::new(at[last].line, ends[last]) });
        if found.hits.len() >= limit {
            found.truncated = true;
            return;
        }
        // Non-overlapping, the universal convention: `aa` in `aaaa` is two hits,
        // not three.
        i += needle.len();
    }
}

impl Grid {
    /// Every occurrence of `query` in scrollback and the viewport, oldest first.
    ///
    /// Scans logical lines — consecutive rows joined while `wrapped` is set — so
    /// a hit may span rows and `Match::end.line` may exceed `Match::start.line`.
    #[must_use]
    pub fn search(&self, query: &Query, limit: usize) -> Matches {
        let mut found = Matches::default();
        // An empty needle matches nothing rather than everything: a bar that has
        // just opened must highlight nothing at all, and "every position in the
        // scrollback" is the other reading of the same input.
        if query.needle.is_empty() || limit == 0 {
            return found;
        }
        let needle: Vec<char> = query.needle.chars().collect();

        // Reused across logical lines, so a whole scan allocates a handful of
        // times rather than once per row. Scrolling is allocation-free by
        // construction in this crate and searching should not be what undoes it.
        let mut chars: Vec<char> = Vec::new();
        let mut at: Vec<AbsPos> = Vec::new();
        // The last column each character occupies — one past `at` for a wide
        // glyph. Gathered here because the cell is in hand; recovering it per
        // hit afterwards would mean finding the row again for every match.
        let mut ends: Vec<usize> = Vec::new();

        let total = self.total_lines();
        let mut index = 0;
        while index < total {
            chars.clear();
            at.clear();
            ends.clear();

            // One logical line: rows joined while each says it wrapped into the
            // next.
            let mut end = index;
            while let Some(row) = self.line(end) {
                let wrapped = row.wrapped();
                // A wrapped row runs to its full width — its trailing blanks are
                // cells the next row continues after, not padding. Only the row
                // ending the logical line may be trimmed.
                let upto = if wrapped { row.len() } else { row.trimmed_len() };
                for col in 0..upto {
                    let Some(cell) = row.get(col) else { break };
                    // The spacer a wide character leaves behind is not a
                    // character; `Row::text` and `Selection::selection_text`
                    // both skip it, and a scan that did not would need a space
                    // typed between every pair of CJK glyphs to match them.
                    if cell.flags.contains(CellFlags::WIDE_SPACER) {
                        continue;
                    }
                    // `span_on` paints through `end.col` inclusive, so a hit
                    // ending on a wide glyph's base cell would leave its right
                    // half unpainted.
                    let last = if cell.flags.contains(CellFlags::WIDE) { col + 1 } else { col };
                    chars.push(cell.ch);
                    at.push(AbsPos::new(row.id, col));
                    ends.push(last);
                    if let Some(extra) = row.extra(cell) {
                        for mark in &extra.zerowidth {
                            chars.push(*mark);
                            at.push(AbsPos::new(row.id, col));
                            ends.push(last);
                        }
                    }
                }
                end += 1;
                if !wrapped {
                    break;
                }
            }

            scan_line(&chars, &at, &ends, &needle, query.case_sensitive, limit, &mut found);
            if found.truncated {
                return found;
            }
            index = end.max(index + 1);
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::CellFlags;
    use alloc::vec;

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

    fn find(g: &Grid, needle: &str) -> Matches {
        g.search(&Query::insensitive(needle), DEFAULT_MATCH_LIMIT)
    }

    #[test]
    fn a_match_is_addressed_the_way_a_selection_is() {
        // The load-bearing one. A highlight is drawn by handing
        // `Match::highlight()` to the same `span_on` a drag uses, so if the two
        // ever disagreed a find bar would light up text the clipboard would not
        // produce -- and nothing else in the suite would notice.
        let g = grid_with(&["hello world"], 20);
        let m = find(&g, "world");
        assert_eq!(m.hits.len(), 1, "one hit");
        assert_eq!(
            g.selection_text(&m.hits[0].highlight()),
            "world",
            "the highlight covers exactly the needle, by the clipboard's own algebra"
        );
    }

    #[test]
    fn a_match_spanning_a_wrapped_row_is_one_hit() {
        // A command that wrapped is one line to the person reading it, so the
        // text they can see runs across the seam and must be findable there.
        let mut g = grid_with(&["echo hello", "world"], 10);
        g.set_wrapped(0, true);
        let m = find(&g, "helloworld");
        assert_eq!(m.hits.len(), 1, "the wrap is a seam, not a boundary");
        assert_eq!(g.selection_text(&m.hits[0].highlight()), "helloworld");
    }

    #[test]
    fn a_newline_is_not_a_wrap_and_does_not_join() {
        // The mirror of the test above, and the reason it cannot simply
        // concatenate every row: two rows that merely follow each other are two
        // logical lines, and joining them would invent matches nobody can see.
        let g = grid_with(&["echo hello", "world"], 10);
        assert!(find(&g, "helloworld").hits.is_empty(), "unwrapped rows are separate lines");
    }

    #[test]
    fn a_wrapped_rows_trailing_space_is_content_not_padding() {
        // Why `Row::text()` is not called per row here: it trims, which is right
        // for the last row of a logical line and wrong for the ones before it.
        // Trimming here would report a match on "abcdef", which is not on screen.
        let mut g = grid_with(&["abc ", "def"], 4);
        g.set_wrapped(0, true);
        assert_eq!(find(&g, "abc def").hits.len(), 1, "the space between them is a real cell");
        assert!(find(&g, "abcdef").hits.is_empty(), "and it must not be trimmed away");
    }

    #[test]
    fn smart_case_means_an_uppercase_query_means_it() {
        let g = grid_with(&["Error and error"], 20);
        assert_eq!(
            g.search(&Query::smart("error"), DEFAULT_MATCH_LIMIT).hits.len(),
            2,
            "a lowercase needle matches either case"
        );
        assert_eq!(
            g.search(&Query::smart("Error"), DEFAULT_MATCH_LIMIT).hits.len(),
            1,
            "a capital in the needle is taken literally"
        );
    }

    #[test]
    fn folding_a_character_never_moves_a_column() {
        // Whole-string folding would be the obvious implementation and it breaks
        // the column map: `ß` folds to `ss`, one character becoming two, and
        // every later hit on the line would be reported one column left of where
        // it is drawn.
        let g = grid_with(&["straße x"], 12);
        let m = find(&g, "x");
        assert_eq!(m.hits.len(), 1);
        assert_eq!(
            (m.hits[0].start.col, m.hits[0].end.col),
            (7, 7),
            "a character that folds to two did not slide the columns after it"
        );
    }

    #[test]
    fn a_one_cell_hit_still_covers_one_cell() {
        // `Selection::is_empty` is true whenever anchor == head, because for a
        // drag that means a click that never moved. For a match it means a
        // perfectly good single-character hit, and a painter that consults it
        // draws nothing for every one-letter search. `span_on` is the question
        // to ask, and it answers inclusively.
        let g = grid_with(&["hello"], 8);
        let m = find(&g, "e");
        assert_eq!(m.hits.len(), 1);
        let sel = m.hits[0].highlight();
        assert!(sel.is_empty(), "anchor == head, so the drag question says empty");
        assert_eq!(
            sel.span_on(m.hits[0].start.line, g.cols()),
            Some((1, 2)),
            "but the span is one real cell, and that is what a painter must use"
        );
    }

    #[test]
    fn a_wide_cell_is_highlighted_whole() {
        // A wide glyph occupies two columns and `span_on` paints through `end`
        // inclusive, so ending on the base cell would paint half a character.
        let mut g = grid_with(&["ab"], 6);
        {
            let row = g.row_mut(0);
            row.cells_mut()[0].ch = '日';
            row.cells_mut()[0].flags |= CellFlags::WIDE;
            row.cells_mut()[1].ch = ' ';
            row.cells_mut()[1].flags |= CellFlags::WIDE_SPACER;
        }
        let m = find(&g, "日");
        assert_eq!(m.hits.len(), 1, "the spacer is skipped, so the glyph is one character");
        assert_eq!(
            m.hits[0].end.col - m.hits[0].start.col + 1,
            2,
            "and the highlight covers both of its columns"
        );
    }

    #[test]
    fn a_combining_mark_rides_its_base_character() {
        // `e` + U+0301 is one thing on screen occupying one cell. It must be
        // findable as the composed text the reader sees, and its hit must not
        // claim a column the mark does not occupy.
        let mut g = grid_with(&["xe y"], 8);
        g.row_mut(0).push_zerowidth(1, '\u{301}');
        let m = find(&g, "e\u{301}");
        assert_eq!(m.hits.len(), 1, "the mark is part of the text, not a separate cell");
        assert_eq!(m.hits[0].start.col, 1);
        assert_eq!(m.hits[0].end.col, 1, "a zero-width mark widens nothing");
    }

    #[test]
    fn matches_do_not_overlap() {
        let g = grid_with(&["aaaa"], 8);
        let m = find(&g, "aa");
        assert_eq!(m.hits.len(), 2, "`aa` in `aaaa` is two hits, not three");
        assert_eq!(m.hits[0].start.col, 0);
        assert_eq!(m.hits[1].start.col, 2);
    }

    #[test]
    fn the_limit_bounds_the_answer_and_says_so() {
        // An unbounded scan for a common letter over a full scrollback builds a
        // vector the size of the history inside the frame that wanted to draw.
        let g = grid_with(&["aaaaaaaaaa"], 12);
        let m = g.search(&Query::insensitive("a"), 4);
        assert_eq!(m.hits.len(), 4, "stopped where it was told");
        assert!(m.truncated, "and admitted it, rather than reading as the whole answer");
    }

    #[test]
    fn an_empty_needle_matches_nothing_rather_than_everything() {
        // A bar that has just opened highlights nothing. "Every position in the
        // scrollback" is the other reading of the same input.
        let g = grid_with(&["hello"], 8);
        assert!(find(&g, "").hits.is_empty());
    }

    #[test]
    fn a_hit_in_scrollback_is_found_after_it_has_scrolled_off() {
        // The whole point of searching rather than looking: the text is above
        // the viewport. Ids, not viewport rows, are what make that expressible.
        let mut g = Grid::new(10, 2, 100);
        let template = crate::cell::Cell::default();
        for word in ["needle", "one", "two", "three"] {
            for (col, ch) in word.chars().enumerate() {
                if let Some(c) = g.cell_mut(g.rows() - 1, col) {
                    c.ch = ch;
                }
            }
            g.scroll_up(1, &template);
        }
        let m = find(&g, "needle");
        assert_eq!(m.hits.len(), 1, "found above the viewport");
        assert_eq!(g.selection_text(&m.hits[0].highlight()), "needle");
    }

    #[test]
    fn every_hit_is_reported_once() {
        // The logical-line walk advances by the line it consumed, so a wrapped
        // line must not be re-scanned from its second row.
        let mut g = grid_with(&["aa", "aa", "bb"], 2);
        g.set_wrapped(0, true);
        let m = find(&g, "a");
        assert_eq!(m.hits.len(), 4, "four `a` cells, four hits");
        let mut seen = vec![];
        for h in &m.hits {
            assert!(!seen.contains(&(h.start.line, h.start.col)), "reported twice: {h:?}");
            seen.push((h.start.line, h.start.col));
        }
    }
}

//! Applying deltas back into a grid.
//!
//! The reference decoder. The web and phone clients reimplement this in
//! TypeScript, and **this is what they are checked against** — the conformance
//! corpus replays a real terminal session through the encoder, applies the
//! result here, and asserts the two agree cell for cell at every frame.
//!
//! Two rules a client must not break, both of which look like details and are
//! not:
//!
//! **Never recompute cell widths.** [`Run::cells`] is the host's decision.
//! Three renderers will not agree on `wcwidth` for every emoji and combining
//! sequence, and a client that computes its own drifts from the host's grid by
//! one column in a way that is nearly impossible to trace back to its cause.
//!
//! **Apply `Scroll` before `Row`** within a delta. The encoder emits them in
//! that order; a decoder that sorts or reorders will write rows into positions
//! the scroll is about to overwrite.

use std::collections::HashMap;

use crate::delta::{AttrDef, AttrId, BlockPayload, CursorState, Delta, DeltaOp, RowPayload};
use crate::encode::Keyframe;

/// A client's reconstruction of a session.
#[derive(Debug, Clone, Default)]
pub struct GridView {
    pub cols: u16,
    pub rows: Vec<RowPayload>,
    /// Cumulative for the session, exactly as the encoder's table is.
    pub attrs: HashMap<AttrId, AttrDef>,
    pub cursor: CursorState,
    pub alt_screen: bool,
    /// The host's mode bits, raw.
    ///
    /// Left as an integer rather than `zest_core::Modes` because this type is
    /// the reference *for the TypeScript clients*, and they have no bitflags
    /// type to widen it into — what they need is exactly the number.
    pub modes: u32,
    pub title: String,
    /// Lines that left the viewport, oldest first.
    ///
    /// Kept client-side because the host may evict them before this client asks:
    /// a phone that was asleep for an hour cannot rely on the desktop still
    /// holding what scrolled past.
    pub scrollback: Vec<RowPayload>,
    /// Command blocks, ascending by id.
    ///
    /// The phone's list view is this and nothing else, which is why it is kept
    /// here rather than derived: a client that re-parsed the grid to find its
    /// own prompts would be the second VT interpretation ADR-004 exists to
    /// avoid, and it would disagree with the desktop about where a command
    /// started.
    pub blocks: Vec<BlockPayload>,
    /// Shadow of [`Keyframe::history_clears`]; only ever raised. See the
    /// applier's field of the same name. (#314)
    pub history_clears: u32,
}

impl Default for CursorState {
    fn default() -> Self {
        Self { row: 0, col: 0, visible: true, shape: 0 }
    }
}

impl GridView {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace everything with a complete state.
    pub fn apply_keyframe(&mut self, k: &Keyframe) {
        // ED 3 destroyed the session's scrollback since this client last
        // honoured one — ours goes too. Never lowered: an alt-screen keyframe
        // carries that grid's 0. The displaced-row carry-over below is
        // suppressed for the same keyframe: the rows this view holds are from
        // *before* the destruction, and filing them into scrollback would keep
        // client-side exactly what the ED 3 destroyed. (#314)
        let cleared = k.history_clears > self.history_clears;
        if cleared {
            self.scrollback.clear();
            self.history_clears = k.history_clears;
        }
        // A *height* change scrolls rows out of the viewport and into the
        // host's history, and at an unchanged width their ids still mean what
        // they meant. Replacing `rows` wholesale would throw away rows this
        // client holds and the host still has, and the blocks anchored there
        // would go on naming them — rendering as blocks with no rows at all,
        // which reads as every block having vanished. (#200)
        //
        // Gated on the width for the reason `grid-view.ts` gives at length: a
        // reflow renumbers every id, so displaced rows cannot be filed under a
        // numbering the keyframe has just replaced. The *stale-scrollback* half
        // of that rule is still missing here and is tracked as #139; this adds
        // only the case where there is nothing to renumber.
        if self.cols == k.cols && !cleared {
            if let Some(first) = k.rows_data.iter().map(|r| r.line).find(|&l| l != i64::MIN) {
                let last_held = self.scrollback.last().map(|r| r.line);
                let displaced = self.rows.iter().filter(|r| {
                    r.line != i64::MIN && r.line < first && last_held.is_none_or(|l| r.line > l)
                });
                self.scrollback.extend(displaced.cloned());
            }
        }
        self.cols = k.cols;
        self.rows = k.rows_data.clone();
        for a in &k.attrs {
            self.attrs.insert(a.id, *a);
        }
        self.cursor = k.cursor;
        self.modes = k.modes.bits();
        self.alt_screen = k.modes.contains(zest_core::Modes::ALT_SCREEN);
        // Replaced wholesale, not merged: a keyframe is the complete state, and
        // a block this client holds that the keyframe does not mention is one
        // the host has evicted.
        self.blocks = k.blocks.clone();
    }

    /// Insert or replace a block, keeping the list ascending by id.
    ///
    /// The same rule as `zest_core::BlockIndex::upsert`, and it has to be: the
    /// conformance suite asserts the two references agree, and a list that
    /// drifted out of order would still *contain* the right blocks while
    /// answering "which block is this line in" differently.
    fn upsert_block(&mut self, b: &BlockPayload) {
        match self.blocks.binary_search_by_key(&b.id, |x| x.id) {
            Ok(i) => self.blocks[i] = b.clone(),
            Err(i) => self.blocks.insert(i, b.clone()),
        }
    }

    /// Apply a change.
    ///
    /// Attributes are merged before the ops that reference them, or a run would
    /// name a style this client has never been told about.
    pub fn apply_delta(&mut self, d: &Delta) {
        for a in &d.attrs {
            self.attrs.insert(a.id, *a);
        }

        for op in &d.ops {
            match op {
                DeltaOp::Scroll { top, bottom, lines } => {
                    let (top, bottom) = (*top as usize, *bottom as usize);
                    let n = usize::try_from(*lines).unwrap_or(0);
                    if n == 0 || top > bottom || bottom >= self.rows.len() {
                        continue;
                    }
                    let region = top..=bottom.min(self.rows.len() - 1);
                    let blank =
                        RowPayload { line: i64::MIN, runs: Vec::new(), wrapped: false };
                    for i in region.clone() {
                        let src = i + n;
                        self.rows[i] =
                            if src <= *region.end() { self.rows[src].clone() } else { blank.clone() };
                    }
                }
                DeltaOp::Row { row, payload } => {
                    let i = *row as usize;
                    if i < self.rows.len() {
                        self.rows[i] = payload.clone();
                    } else {
                        // A row past the end means a resize this client has not
                        // been told about. Growing is the forgiving answer;
                        // dropping it would leave a permanently stale line.
                        self.rows.resize(
                            i + 1,
                            RowPayload { line: i64::MIN, runs: Vec::new(), wrapped: false },
                        );
                        self.rows[i] = payload.clone();
                    }
                }
                DeltaOp::Erase { top, left, bottom, right, attr } => {
                    // Not emitted by the current encoder, which sends whole
                    // rows -- and implemented anyway, because the *other*
                    // reference (`Applier`) implements it, and two references
                    // for one wire format that disagree about an op is how a
                    // client author learns the format is ambiguous.
                    //
                    // A run list has no cells to clear in place, so the erased
                    // span is rebuilt as a run of spaces in the named
                    // attribute. `cells` stays authoritative for width.
                    let (top, bottom) = (usize::from(*top), usize::from(*bottom));
                    let (left, right) = (usize::from(*left), usize::from(*right));
                    if top > bottom || left > right {
                        continue;
                    }
                    for r in top..=bottom.min(self.rows.len().saturating_sub(1)) {
                        let Some(row) = self.rows.get_mut(r) else { continue };
                        *row = erase_span(row, left, right, *attr);
                    }
                }
                DeltaOp::Cursor { cursor } => self.cursor = *cursor,
                DeltaOp::SbPush { payload } => self.scrollback.push(payload.clone()),
                DeltaOp::AltScreen { active } => self.alt_screen = *active,
                DeltaOp::Title { title } => self.title.clone_from(title),
                DeltaOp::Modes { bits } => self.modes = *bits,
            }
        }

        // After the ops, matching `Applier`: a block names absolute line ids
        // that the rows in this same batch have just established.
        for b in &d.blocks {
            self.upsert_block(b);
        }
    }

    /// The visible rows, for comparison against the host's own encoding.
    #[must_use]
    pub fn rows(&self) -> &[RowPayload] {
        &self.rows
    }
}

/// Rebuild one row with `[left, right]` replaced by blanks in `attr`.
///
/// Runs are re-split cell by cell and then coalesced, which is not the fastest
/// possible thing and is the clearest: `Erase` is rare, and a clever in-place
/// splice across run boundaries is exactly where an off-by-one lives.
fn erase_span(row: &RowPayload, left: usize, right: usize, attr: AttrId) -> RowPayload {
    // Flatten to (attr, char) per cell. A run's text may be shorter than its
    // cell count -- that is a wide character's spacer -- so the same padding
    // rule as everywhere else applies: take characters in order, then spaces.
    let mut flat: Vec<(AttrId, char)> = Vec::new();
    for run in &row.runs {
        let mut chars = run.text.chars();
        for _ in 0..run.cells {
            flat.push((run.attr, chars.next().unwrap_or(' ')));
        }
    }
    if right >= flat.len() {
        flat.resize(right + 1, (attr, ' '));
    }
    for slot in &mut flat[left..=right] {
        *slot = (attr, ' ');
    }

    let mut runs: Vec<crate::delta::Run> = Vec::new();
    for (a, ch) in flat {
        match runs.last_mut() {
            Some(last) if last.attr == a => {
                last.cells += 1;
                last.text.push(ch);
            }
            _ => runs.push(crate::delta::Run {
                attr: a,
                cells: 1,
                text: ch.to_string(),
                marks: Vec::new(),
            }),
        }
    }
    RowPayload { line: row.line, runs, wrapped: row.wrapped }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta::Run;
    use crate::encode::Encoder;
    use zest_core::{Modes, Terminal};

    fn cursor() -> CursorState {
        CursorState { row: 0, col: 0, visible: true, shape: 0 }
    }

    fn row(line: i64, text: &str) -> RowPayload {
        RowPayload {
            line,
            runs: vec![Run { attr: AttrId(0), cells: text.len() as u16, text: text.into(), marks: Vec::new() }],
            wrapped: false,
        }
    }

    fn keyframe(cols: u16, lines: &[(i64, &str)]) -> Keyframe {
        Keyframe {
            cols,
            rows: lines.len() as u16,
            rows_data: lines.iter().map(|&(l, t)| row(l, t)).collect(),
            attrs: Vec::new(),
            cursor: cursor(),
            modes: Modes::empty(),
            blocks: Vec::new(),
            blocks_from: 0,
            title: String::new(),
            history_clears: 0,
        }
    }

    #[test]
    fn a_height_change_keyframe_keeps_the_rows_that_left_the_viewport() {
        // Dragging a window's height down and back is where "every block is
        // gone" comes from. The width never changes, so nothing is renumbered
        // and nothing needs re-anchoring -- but the rows that were on screen
        // are history now, and replacing `rows` wholesale threw away this
        // client's only copy. The blocks anchored there went on naming them and
        // rendered with no rows at all. (#200)
        let mut view = GridView::new();
        view.apply_keyframe(&keyframe(20, &[(0, "line 0"), (1, "line 1"), (2, "line 2")]));
        view.apply_keyframe(&keyframe(20, &[(3, "line 3"), (4, "line 4"), (5, "line 5")]));

        let held: Vec<i64> = view.scrollback.iter().map(|r| r.line).collect();
        assert_eq!(held, vec![0, 1, 2], "the displaced rows were dropped, not kept");
    }

    #[test]
    fn a_keyframe_that_says_history_was_cleared_empties_the_views_scrollback() {
        // The `history_clears` counter is what carries an ED 3 to a client
        // that applies keyframes: the rows are not damaged, they are gone,
        // and nothing else in a keyframe can say so. Advanced counter drops
        // the view's history; unmoved counter leaves it — eviction stays
        // silent and a client's longer history stays its own. (#314)
        let mut view = GridView::new();
        view.apply_keyframe(&keyframe(20, &[(0, "line 0"), (1, "line 1"), (2, "line 2")]));
        view.apply_keyframe(&keyframe(20, &[(3, "line 3"), (4, "line 4"), (5, "line 5")]));
        assert!(!view.scrollback.is_empty(), "the view holds no history, so this proves nothing");

        let mut cleared = keyframe(20, &[(6, "$")]);
        cleared.rows = 3;
        cleared.rows_data =
            vec![row(6, "$"), row(i64::MIN, ""), row(i64::MIN, "")];
        cleared.history_clears = 1;
        view.apply_keyframe(&cleared);
        assert!(
            view.scrollback.is_empty(),
            "a keyframe carrying an advanced counter left the view's history: {:?}",
            view.scrollback.iter().map(|r| r.line).collect::<Vec<_>>()
        );

        // Same counter again: nothing more is destroyed.
        view.apply_keyframe(&keyframe(20, &[(0, "old 0"), (7, "line 7")]));
        view.apply_keyframe(&{
            let mut k = keyframe(20, &[(8, "line 8"), (9, "line 9")]);
            k.history_clears = 1;
            k
        });
        assert!(
            !view.scrollback.is_empty(),
            "an unmoved counter re-cleared history it had no right to"
        );
    }

    #[test]
    fn a_width_change_keyframe_still_drops_what_it_cannot_renumber() {
        // The counterpart, and why the carry-over is gated on the width: a
        // reflow renumbers every id, so displaced rows cannot be filed under a
        // numbering the keyframe has just replaced.
        let mut view = GridView::new();
        view.apply_keyframe(&keyframe(20, &[(0, "line 0"), (1, "line 1")]));
        view.apply_keyframe(&keyframe(10, &[(2, "line 2"), (3, "line 3")]));

        assert!(view.scrollback.is_empty(), "old-numbering rows must not survive a reflow");
    }

    /// Replay a session and assert the decoder never diverges.
    ///
    /// Checked at *every* step, not at the end: two errors that cancel out would
    /// pass a final comparison, and the whole point is to find the first frame
    /// where the two disagree.
    fn conformance(cols: usize, rows: usize, chunks: &[&str]) {
        let mut term = Terminal::new(cols, rows, 1000);
        let mut enc = Encoder::new();
        let mut view = GridView::new();

        let k = enc.keyframe(term.grid(), cursor(), Modes::empty(), "", term.blocks());
        view.apply_keyframe(&k);

        for (i, chunk) in chunks.iter().enumerate() {
            term.advance(chunk.as_bytes());
            let d = enc.delta(term.grid(), cursor(), term.modes(), "", term.blocks());
            view.apply_delta(&d);

            // The host's own encoding of the same grid is the reference.
            let mut probe = Encoder::new();
            let truth = probe.keyframe(term.grid(), cursor(), term.modes(), "", term.blocks());

            assert_eq!(
                view.rows().len(),
                truth.rows_data.len(),
                "step {i}: row count diverged"
            );
            for (r, (got, want)) in view.rows().iter().zip(truth.rows_data.iter()).enumerate() {
                let got_text: String = got.runs.iter().map(|x| x.text.as_str()).collect();
                let want_text: String = want.runs.iter().map(|x| x.text.as_str()).collect();
                assert_eq!(
                    got_text, want_text,
                    "step {i}, row {r}: text diverged after chunk {chunk:?}"
                );
                let got_cells: u16 = got.runs.iter().map(|x| x.cells).sum();
                let want_cells: u16 = want.runs.iter().map(|x| x.cells).sum();
                assert_eq!(got_cells, want_cells, "step {i}, row {r}: cell count diverged");
                assert_eq!(got.line, want.line, "step {i}, row {r}: line id diverged");
            }
        }
    }

    #[test]
    fn plain_output_round_trips() {
        conformance(20, 5, &["hello", " world", "\r\nsecond", "\r\nthird"]);
    }

    #[test]
    fn scrolling_round_trips() {
        // The case the SCROLL op exists for, and the one where an ordering bug
        // shows up as content one row out.
        let chunks: Vec<&str> = vec!["a\r\n", "b\r\n", "c\r\n", "d\r\n", "e\r\n", "f\r\n"];
        conformance(20, 3, &chunks);
    }

    #[test]
    fn colour_changes_round_trip() {
        conformance(
            40,
            4,
            &[
                "\x1b[31mred\x1b[0m ",
                "\x1b[42mgreen bg\x1b[0m ",
                "\x1b[1;4mbold underline\x1b[0m",
                "\r\n\x1b[38;2;10;20;30mtruecolor\x1b[0m",
            ],
        );
    }

    #[test]
    fn wide_characters_round_trip() {
        conformance(20, 3, &["世界", "\r\nabc", "\r\n日本語テキスト"]);
    }

    #[test]
    fn erase_and_reposition_round_trip() {
        conformance(
            20,
            5,
            &["one\r\ntwo\r\nthree", "\x1b[H", "\x1b[2J", "fresh", "\x1b[3;5Hplaced"],
        );
    }

    #[test]
    fn the_alternate_screen_round_trips() {
        conformance(20, 4, &["shell prompt", "\x1b[?1049h", "full screen app", "\x1b[?1049l"]);
    }

    #[test]
    fn a_long_session_round_trips() {
        // Enough scrolling to exercise eviction and the scrollback push path.
        let lines: Vec<String> = (0..200).map(|i| format!("line {i}\r\n")).collect();
        let chunks: Vec<&str> = lines.iter().map(String::as_str).collect();
        conformance(40, 10, &chunks);
    }

    #[test]
    fn a_scroll_moves_rows_up_and_blanks_the_bottom() {
        let mut v = GridView::new();
        v.rows = vec![row(0, "a"), row(1, "b"), row(2, "c")];
        v.apply_delta(&Delta {
            blocks: Vec::new(),
            attrs: vec![],
            ops: vec![DeltaOp::Scroll { top: 0, bottom: 2, lines: 1 }],
        });
        assert_eq!(v.rows[0].runs[0].text, "b");
        assert_eq!(v.rows[1].runs[0].text, "c");
        assert!(v.rows[2].runs.is_empty(), "the exposed row was not blanked");
    }

    #[test]
    fn attributes_arrive_before_the_runs_that_use_them() {
        // A run naming a style the client has never been told about would render
        // in whatever it happened to have. The delta carries both.
        let mut v = GridView::new();
        v.rows = vec![row(0, "x")];
        v.apply_delta(&Delta {
            blocks: Vec::new(),
            attrs: vec![AttrDef {
                id: AttrId(7),
                fg: zest_core::Color::Indexed(1),
                bg: zest_core::Color::Default,
                flags: zest_core::CellFlags::empty(),
            }],
            ops: vec![DeltaOp::Row {
                row: 0,
                payload: RowPayload {
                    line: 0,
                    runs: vec![Run { attr: AttrId(7), cells: 3, text: "red".into(), marks: Vec::new() }],
                    wrapped: false,
                },
            }],
        });
        assert!(v.attrs.contains_key(&AttrId(7)));
        assert_eq!(v.rows[0].runs[0].attr, AttrId(7));
    }

    #[test]
    fn scrollback_accumulates_client_side() {
        // The host may evict a line before this client asks for it, so what
        // scrolled past has to be kept as it goes.
        let mut term = Terminal::new(20, 2, 100);
        let mut enc = Encoder::new();
        let mut view = GridView::new();
        view.apply_keyframe(&enc.keyframe(term.grid(), cursor(), Modes::empty(), "", term.blocks()));

        for i in 0..5 {
            term.advance(format!("line{i}\r\n").as_bytes());
            view.apply_delta(&enc.delta(term.grid(), cursor(), Modes::empty(), "", term.blocks()));
        }
        assert!(!view.scrollback.is_empty(), "nothing was pushed to scrollback");
    }
}

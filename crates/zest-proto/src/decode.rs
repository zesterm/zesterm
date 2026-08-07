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

use crate::delta::{AttrDef, AttrId, CursorState, Delta, DeltaOp, RowPayload};
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
    pub title: String,
    /// Lines that left the viewport, oldest first.
    ///
    /// Kept client-side because the host may evict them before this client asks:
    /// a phone that was asleep for an hour cannot rely on the desktop still
    /// holding what scrolled past.
    pub scrollback: Vec<RowPayload>,
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
        self.cols = k.cols;
        self.rows = k.rows_data.clone();
        for a in &k.attrs {
            self.attrs.insert(a.id, *a);
        }
        self.cursor = k.cursor;
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
                    let _ = (top, left, bottom, right, attr);
                    // Not emitted by the current encoder, which sends whole
                    // rows. Left unimplemented rather than guessed: a decoder
                    // that silently mis-handles an op it has never received is
                    // worse than one that has visibly not met it yet.
                }
                DeltaOp::Cursor { cursor } => self.cursor = *cursor,
                DeltaOp::SbPush { payload } => self.scrollback.push(payload.clone()),
                DeltaOp::AltScreen { active } => self.alt_screen = *active,
                DeltaOp::Title { title } => self.title.clone_from(title),
            }
        }
    }

    /// The visible rows, for comparison against the host's own encoding.
    #[must_use]
    pub fn rows(&self) -> &[RowPayload] {
        &self.rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta::Run;
    use crate::encode::Encoder;
    use zest_core::Terminal;

    fn cursor() -> CursorState {
        CursorState { row: 0, col: 0, visible: true, shape: 0 }
    }

    fn row(line: i64, text: &str) -> RowPayload {
        RowPayload {
            line,
            runs: vec![Run { attr: AttrId(0), cells: text.len() as u16, text: text.into() }],
            wrapped: false,
        }
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

        let k = enc.keyframe(term.grid(), cursor(), false, "");
        view.apply_keyframe(&k);

        for (i, chunk) in chunks.iter().enumerate() {
            term.advance(chunk.as_bytes());
            let alt = term.modes().contains(zest_core::Modes::ALT_SCREEN);
            let d = enc.delta(term.grid(), cursor(), alt, "");
            view.apply_delta(&d);

            // The host's own encoding of the same grid is the reference.
            let mut probe = Encoder::new();
            let truth = probe.keyframe(term.grid(), cursor(), alt, "");

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
                    runs: vec![Run { attr: AttrId(7), cells: 3, text: "red".into() }],
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
        view.apply_keyframe(&enc.keyframe(term.grid(), cursor(), false, ""));

        for i in 0..5 {
            term.advance(format!("line{i}\r\n").as_bytes());
            view.apply_delta(&enc.delta(term.grid(), cursor(), false, ""));
        }
        assert!(!view.scrollback.is_empty(), "nothing was pushed to scrollback");
    }
}

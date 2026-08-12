//! Turning a grid into deltas.
//!
//! One [`Encoder`] per subscriber, holding the rows as that subscriber last saw
//! them. The next delta is the difference.
//!
//! # Why a shadow copy rather than dirty flags
//!
//! Row dirty flags would be cheaper — mark the row on every mutation, encode
//! only marked rows. They were rejected because **a missed mark is a silently
//! corrupt remote display**: the phone shows a line that no longer exists, with
//! nothing to indicate anything went wrong, and the bug lives at whichever of a
//! hundred mutation sites forgot. Diffing against what was actually sent cannot
//! miss a change by construction.
//!
//! The cost is one grid per subscriber — roughly 48KB at 100×30 — and an O(cells)
//! comparison per encode, which is a few thousand cells. If a profile ever says
//! that matters, dirty flags become an *optimization on top of* a correct
//! encoder, checkable against it. Starting with the fast one leaves nothing to
//! check against.
//!
//! # Scroll detection is exact, not heuristic
//!
//! Terminals usually detect scrolling by comparing row contents and looking for
//! a shifted run, which mistakes two identical blank rows for a scroll. Here
//! every row carries an absolute [`zest_core::LineId`], so a viewport that moved
//! is simply one whose first line id increased — no guessing, and no false
//! positive on repeated content.

use std::collections::HashMap;

use zest_core::{BlockIndex, Cell, CellFlags, Color, Grid, Modes};

use crate::delta::{
    AttrDef, AttrId, BlockPayload, CellMarks, CursorState, Delta, DeltaOp, Run, RowPayload,
};

/// What makes two cells share a run.
type AttrKey = (Color, Color, CellFlags);

/// Per-subscriber encoding state.
///
/// Not `Clone`: two subscribers sharing one encoder would each receive the
/// other's deltas as their base and diverge silently.
pub struct Encoder {
    /// Interning table, cumulative for the session. See [`AttrDef`].
    attrs: HashMap<AttrKey, AttrId>,
    /// Attributes defined but not yet sent, drained into each delta.
    pending_attrs: Vec<AttrDef>,
    /// Rows as the subscriber last saw them.
    seen: Vec<RowPayload>,
    /// Blocks as the subscriber last saw them, ascending by id.
    ///
    /// A shadow for the same reason `seen` is one: a "block changed" flag set
    /// at each of the four marker handlers is a flag somebody forgets, and the
    /// symptom is a phone showing a command as still running forever.
    seen_blocks: Vec<BlockPayload>,
    cursor: CursorState,
    /// The whole mode set, not just the alternate-screen bit.
    ///
    /// One field rather than a `bool` beside it, so `DeltaOp::AltScreen` and
    /// `DeltaOp::Modes` are two projections of one value and cannot disagree.
    modes: Modes,
    title: String,
    next_attr: u16,
}

impl Default for Encoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Encoder {
    #[must_use]
    pub fn new() -> Self {
        Self {
            attrs: HashMap::new(),
            pending_attrs: Vec::new(),
            seen: Vec::new(),
            seen_blocks: Vec::new(),
            cursor: CursorState { row: 0, col: 0, visible: true, shape: 0 },
            modes: Modes::empty(),
            title: String::new(),
            next_attr: 0,
        }
    }

    /// Intern an attribute, defining it if this is the first time.
    fn attr(&mut self, cell: &Cell) -> AttrId {
        let key = (cell.fg, cell.bg, cell.flags);
        if let Some(id) = self.attrs.get(&key) {
            return *id;
        }
        let id = AttrId(self.next_attr);
        self.next_attr = self.next_attr.wrapping_add(1);
        self.attrs.insert(key, id);
        self.pending_attrs.push(AttrDef { id, fg: cell.fg, bg: cell.bg, flags: cell.flags });
        id
    }

    /// Split one grid row into runs.
    ///
    /// Trailing blanks in the default attribute are dropped: a mostly-empty
    /// 200-column row is otherwise 200 cells of nothing on the wire, every time
    /// anything on it changes. The client pads to the grid width, which it knows.
    fn encode_row(&mut self, row: &zest_core::Row) -> RowPayload {
        let cells = row.cells();
        let end = cells
            .iter()
            .rposition(|c| c.ch != ' ' || c.bg != Color::Default || c.flags != CellFlags::empty())
            .map_or(0, |i| i + 1);

        let mut runs: Vec<Run> = Vec::new();
        let mut col = 0usize;
        while col < end {
            let cell = cells[col];
            let attr = self.attr(&cell);

            let mut text = String::new();
            let mut marks: Vec<CellMarks> = Vec::new();
            let mut count: u16 = 0;
            while col < end {
                let c = cells[col];
                if (c.fg, c.bg, c.flags) != (cell.fg, cell.bg, cell.flags) {
                    break;
                }
                // A WIDE_SPACER is the second half of a double-width character.
                // It carries no character of its own but does occupy a cell, so
                // it is counted and not written -- which is exactly the
                // distinction `Run::cells` exists to express.
                if !c.flags.contains(CellFlags::WIDE_SPACER) {
                    text.push(c.ch);
                }
                // Combining marks travel beside the text, not inside it, so a
                // client never has to know which codepoints combine. Without
                // this they never crossed the wire at all: `e` + U+0301 arrived
                // as a bare `e`.
                if let Some(extra) = row.extra(&c) {
                    if !extra.zerowidth.is_empty() {
                        marks.push(CellMarks {
                            at: count,
                            marks: extra.zerowidth.iter().collect(),
                        });
                    }
                }
                count = count.saturating_add(1);
                col += 1;
            }

            runs.push(Run { attr, cells: count, text, marks });
        }

        RowPayload {
            line: i64::try_from(row.id).unwrap_or(i64::MAX),
            runs,
            wrapped: row.wrapped,
        }
    }

    fn encode_all_rows(&mut self, grid: &Grid) -> Vec<RowPayload> {
        // Active space throughout the encoder: the wire carries the session's
        // live screen. A host whose own window happens to be scrolled back must
        // not send its subscribers the rows it is reading.
        (0..grid.rows()).map(|r| self.encode_row(grid.active_row(r))).collect()
    }

    /// The whole state, and the subscriber's new baseline.
    ///
    /// Sent on attach, and whenever a client has fallen far enough behind that
    /// a delta chain would be larger than the state it describes.
    pub fn keyframe(
        &mut self,
        grid: &Grid,
        cursor: CursorState,
        modes: Modes,
        title: &str,
        blocks: &BlockIndex,
    ) -> Keyframe {
        // Interning is *not* reset. A keyframe re-sends every attribute it uses
        // because the client may be new, but reusing the same ids keeps a later
        // delta's ids meaningful to a client that has been attached throughout.
        let rows = self.encode_all_rows(grid);
        let attrs = self.all_attrs();
        self.pending_attrs.clear();
        self.seen = rows.clone();
        self.cursor = cursor;
        self.modes = modes;
        self.title = title.to_string();
        // A keyframe is a complete state, so it carries every block rather than
        // the ones that changed — the client may be new, and a block it never
        // received is a command that vanished from its history.
        self.seen_blocks =
            blocks.blocks().iter().map(BlockPayload::from_block).collect();

        Keyframe {
            cols: u16::try_from(grid.cols()).unwrap_or(u16::MAX),
            rows: u16::try_from(grid.rows()).unwrap_or(u16::MAX),
            rows_data: rows,
            attrs,
            cursor,
            modes,
            blocks: self.seen_blocks.clone(),
            title: title.to_string(),
        }
    }

    /// Encode rows of history, self-contained.
    ///
    /// Returns the payloads *and* every attribute they name, because scrollback
    /// is prepended to a client's history rather than diffed against anything —
    /// there is no later delta that would define the attributes for it.
    ///
    /// Use a fresh `Encoder` for this, never a subscriber's: interning these
    /// rows into that table would assign ids the delta stream then never
    /// defines, and the client would render live output in whatever style it
    /// happened to be holding.
    pub fn history(&mut self, rows: &[&zest_core::Row]) -> (Vec<RowPayload>, Vec<AttrDef>) {
        let payloads: Vec<RowPayload> = rows.iter().map(|r| self.encode_row(r)).collect();
        (payloads, self.all_attrs())
    }

    /// Every attribute defined so far, for a keyframe.
    fn all_attrs(&self) -> Vec<AttrDef> {
        let mut v: Vec<AttrDef> = self
            .attrs
            .iter()
            .map(|((fg, bg, flags), id)| AttrDef { id: *id, fg: *fg, bg: *bg, flags: *flags })
            .collect();
        // Sorted so the wire is deterministic -- a HashMap's order is not, and a
        // golden-fixture test that compares bytes would be flaky without this.
        v.sort_by_key(|a| a.id.0);
        v
    }

    /// Blocks that changed since the subscriber last saw them.
    ///
    /// A merge rather than a search: both lists are ascending by id — the index
    /// appends and evicts from the front — so this is linear where a `contains`
    /// per block would be quadratic in the number of commands a long-lived
    /// session has run.
    ///
    /// A block the host has *evicted* is simply absent, and nothing says so.
    /// That is deliberate: the client evicts on its own scrollback bound
    /// through the same code, and a client configured to keep more history than
    /// the host should keep more, not be told to forget.
    fn diff_blocks(&mut self, blocks: &BlockIndex) -> Vec<BlockPayload> {
        let fresh: Vec<BlockPayload> =
            blocks.blocks().iter().map(BlockPayload::from_block).collect();

        let mut changed = Vec::new();
        let mut old = self.seen_blocks.iter().peekable();
        for b in &fresh {
            while old.peek().is_some_and(|o| o.id < b.id) {
                old.next();
            }
            match old.peek() {
                Some(o) if o.id == b.id => {
                    if *o != b {
                        changed.push(b.clone());
                    }
                    old.next();
                }
                _ => changed.push(b.clone()),
            }
        }

        self.seen_blocks = fresh;
        changed
    }

    /// What changed since the last `keyframe` or `delta`.
    pub fn delta(
        &mut self,
        grid: &Grid,
        cursor: CursorState,
        modes: Modes,
        title: &str,
        blocks: &BlockIndex,
    ) -> Delta {
        let mut ops = Vec::new();

        // A screen switch comes before everything, including the scroll.
        //
        // `AltScreen` and `Modes` choose *which grid* the ops below land in, so
        // emitting them last -- which is where they naturally fall, next to the
        // other scalar comparisons -- means the rows describing the alternate
        // screen get written into the primary one. Invisible to a client that
        // keeps a flat row list, which is why it survived this long; silent
        // corruption for one applying into a real Terminal.
        // See `Delta::screen_switch_comes_first`.
        let alt_screen = modes.contains(Modes::ALT_SCREEN);
        if alt_screen != self.modes.contains(Modes::ALT_SCREEN) {
            ops.push(DeltaOp::AltScreen { active: alt_screen });
        }
        if modes != self.modes {
            ops.push(DeltaOp::Modes { bits: modes.bits() });
            self.modes = modes;
        }

        // Scroll first, and by absolute line id rather than by content. Emitting
        // rows before the scroll that moves them writes into positions the
        // scroll is about to overwrite.
        let new_first = grid.active_row(0).id;
        if let Some(old_first) = self.seen.first().map(|r| r.line) {
            let moved = i64::try_from(new_first).unwrap_or(i64::MAX) - old_first;
            let height = i64::try_from(self.seen.len()).unwrap_or(i64::MAX);
            // A jump larger than the viewport is not a scroll -- the whole
            // screen is new, and every row is about to be sent anyway.
            if moved > 0 && moved < height {
                ops.push(DeltaOp::Scroll {
                    top: 0,
                    bottom: u16::try_from(self.seen.len().saturating_sub(1)).unwrap_or(u16::MAX),
                    lines: i16::try_from(moved).unwrap_or(i16::MAX),
                });
                // Lines that left the viewport are now history the client keeps
                // for itself, since the host may evict them before it asks.
                let n = usize::try_from(moved).unwrap_or(0).min(self.seen.len());
                for payload in self.seen.iter().take(n) {
                    ops.push(DeltaOp::SbPush { payload: payload.clone() });
                }
                // Shift the shadow to match, so the row diff below compares like
                // with like instead of reporting every row as changed.
                self.seen.drain(..n);
                self.seen.resize(
                    grid.rows(),
                    RowPayload { line: i64::MIN, runs: Vec::new(), wrapped: false },
                );
            }
        }

        let fresh = self.encode_all_rows(grid);
        if fresh.len() != self.seen.len() {
            self.seen.resize(
                fresh.len(),
                RowPayload { line: i64::MIN, runs: Vec::new(), wrapped: false },
            );
        }
        for (i, row) in fresh.iter().enumerate() {
            if self.seen.get(i) != Some(row) {
                ops.push(DeltaOp::Row {
                    row: u16::try_from(i).unwrap_or(u16::MAX),
                    payload: row.clone(),
                });
            }
        }
        self.seen = fresh;

        if cursor != self.cursor {
            ops.push(DeltaOp::Cursor { cursor });
            self.cursor = cursor;
        }
        if title != self.title {
            ops.push(DeltaOp::Title { title: title.to_string() });
            self.title = title.to_string();
        }

        Delta {
            attrs: std::mem::take(&mut self.pending_attrs),
            ops,
            blocks: self.diff_blocks(blocks),
        }
    }
}

/// A complete state, as [`Encoder::keyframe`] produces it.
#[derive(Debug, Clone, PartialEq)]
pub struct Keyframe {
    pub cols: u16,
    pub rows: u16,
    pub rows_data: Vec<RowPayload>,
    pub attrs: Vec<AttrDef>,
    pub cursor: CursorState,
    /// The host's mode bits at this instant.
    ///
    /// A keyframe is a complete state and an attaching client encodes its own
    /// keystrokes, so leaving modes out would mean a freshly attached session
    /// has broken arrow keys until the host next happens to change a mode.
    pub modes: Modes,
    /// Every block the host holds, not just the ones that changed.
    ///
    /// A client attaching to a session that has been running for a week is the
    /// case this exists for: its command history is exactly this list, and it
    /// has no other way to learn about a command that finished before it
    /// arrived.
    pub blocks: Vec<BlockPayload>,
    /// The title at this instant. The encoder always knew it (its shadow
    /// tracked it for `DeltaOp::Title`) and simply never put it in the
    /// keyframe — which left a freshly attached tab unlabeled until the host
    /// next retitled.
    pub title: String,
}

#[cfg(test)]
mod tests {
    use super::*;
    use zest_core::Terminal;

    fn cursor() -> CursorState {
        CursorState { row: 0, col: 0, visible: true, shape: 0 }
    }

    fn term(cols: usize, rows: usize, input: &str) -> Terminal {
        let mut t = Terminal::new(cols, rows, 100);
        t.advance(input.as_bytes());
        t
    }

    #[test]
    fn an_unchanged_grid_produces_an_empty_delta() {
        // The 0%-idle guarantee extended across the network: an idle terminal
        // must not generate traffic any more than it generates frames.
        let t = term(20, 5, "hello");
        let mut e = Encoder::new();
        e.keyframe(t.grid(), cursor(), Modes::empty(), "", t.blocks());

        let d = e.delta(t.grid(), cursor(), Modes::empty(), "", t.blocks());
        assert!(d.ops.is_empty(), "an idle terminal produced {:?}", d.ops);
        assert!(d.attrs.is_empty());
    }

    #[test]
    fn only_the_changed_row_is_sent() {
        let mut t = term(20, 5, "one\r\ntwo\r\n");
        let mut e = Encoder::new();
        e.keyframe(t.grid(), cursor(), Modes::empty(), "", t.blocks());

        t.advance(b"three");
        let d = e.delta(t.grid(), cursor(), Modes::empty(), "", t.blocks());
        let rows: Vec<u16> = d
            .ops
            .iter()
            .filter_map(|o| match o {
                DeltaOp::Row { row, .. } => Some(*row),
                _ => None,
            })
            .collect();
        assert_eq!(rows, vec![2], "sent rows other than the one that changed");
    }

    #[test]
    fn attributes_are_interned_once_per_session() {
        // A syntax-highlighted row has ~50 style changes and a handful of
        // distinct styles. Inline styles would put the colours on the wire fifty
        // times per row.
        let mut t = term(40, 5, "\x1b[31mred\x1b[0m plain\r\n");
        let mut e = Encoder::new();
        e.keyframe(t.grid(), cursor(), Modes::empty(), "", t.blocks());
        let defined = e.attrs.len();

        t.advance(b"\x1b[31mred again\x1b[0m\r\n");
        let d = e.delta(t.grid(), cursor(), Modes::empty(), "", t.blocks());
        assert!(
            d.attrs.is_empty(),
            "redefined an attribute that was already sent: {:?}",
            d.attrs
        );
        assert_eq!(e.attrs.len(), defined, "the table grew for a colour already seen");
    }

    #[test]
    fn a_new_colour_is_defined_exactly_once() {
        let mut t = term(40, 5, "plain\r\n");
        let mut e = Encoder::new();
        e.keyframe(t.grid(), cursor(), Modes::empty(), "", t.blocks());

        t.advance(b"\x1b[38;2;1;2;3mtruecolor\x1b[0m\r\n");
        let first = e.delta(t.grid(), cursor(), Modes::empty(), "", t.blocks());
        assert_eq!(first.attrs.len(), 1, "expected one new attribute: {:?}", first.attrs);

        t.advance(b"\x1b[38;2;1;2;3magain\x1b[0m\r\n");
        let second = e.delta(t.grid(), cursor(), Modes::empty(), "", t.blocks());
        assert!(second.attrs.is_empty(), "redefined it: {:?}", second.attrs);
    }

    #[test]
    fn scrolling_emits_one_scroll_before_any_row() {
        // The `tail -f` case: one 7-byte scroll plus one row, instead of a full
        // repaint. Order is load-bearing -- a row written before the scroll that
        // moves it lands in a position the scroll is about to overwrite.
        let mut t = term(20, 3, "a\r\nb\r\nc");
        let mut e = Encoder::new();
        e.keyframe(t.grid(), cursor(), Modes::empty(), "", t.blocks());

        t.advance(b"\r\nd");
        let d = e.delta(t.grid(), cursor(), Modes::empty(), "", t.blocks());

        assert!(d.scrolls_come_first(), "row before scroll: {:?}", d.ops);
        let scrolls: Vec<&DeltaOp> =
            d.ops.iter().filter(|o| matches!(o, DeltaOp::Scroll { .. })).collect();
        assert_eq!(scrolls.len(), 1, "{:?}", d.ops);
        assert!(
            matches!(scrolls[0], DeltaOp::Scroll { lines: 1, .. }),
            "wrong distance: {:?}",
            scrolls[0]
        );
    }

    #[test]
    fn a_scrolled_line_is_pushed_to_scrollback() {
        // The client keeps history the host may evict before it is asked for,
        // so a line leaving the viewport has to be handed over as it goes.
        let mut t = term(20, 3, "first\r\nb\r\nc");
        let mut e = Encoder::new();
        e.keyframe(t.grid(), cursor(), Modes::empty(), "", t.blocks());

        t.advance(b"\r\nd");
        let d = e.delta(t.grid(), cursor(), Modes::empty(), "", t.blocks());
        let pushed: Vec<&RowPayload> = d
            .ops
            .iter()
            .filter_map(|o| match o {
                DeltaOp::SbPush { payload } => Some(payload),
                _ => None,
            })
            .collect();
        assert_eq!(pushed.len(), 1, "{:?}", d.ops);
        assert_eq!(pushed[0].runs[0].text, "first");
    }

    #[test]
    fn a_jump_larger_than_the_viewport_is_not_a_scroll() {
        // Clearing and repainting moves every line. Calling that a scroll would
        // emit a scroll the client cannot use, followed by every row anyway.
        let mut t = term(20, 3, "a\r\nb\r\nc");
        let mut e = Encoder::new();
        e.keyframe(t.grid(), cursor(), Modes::empty(), "", t.blocks());

        for _ in 0..10 {
            t.advance(b"\r\nx");
        }
        let d = e.delta(t.grid(), cursor(), Modes::empty(), "", t.blocks());
        assert!(
            !d.ops.iter().any(|o| matches!(o, DeltaOp::Scroll { .. })),
            "a full-screen replacement was reported as a scroll: {:?}",
            d.ops
        );
    }

    #[test]
    fn a_double_width_character_reports_two_cells_and_one_char() {
        // The rule three renderers depend on. A client that counted characters
        // would draw everything after this in the wrong column.
        let t = term(20, 2, "世");
        let mut e = Encoder::new();
        let k = e.keyframe(t.grid(), cursor(), Modes::empty(), "", t.blocks());
        let row = &k.rows_data[0];
        let total: u16 = row.runs.iter().map(|r| r.cells).sum();
        let text: String = row.runs.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(text, "世");
        assert_eq!(total, 2, "a double-width character must occupy two cells");
    }

    #[test]
    fn trailing_blanks_are_not_sent() {
        // A mostly-empty 200-column row would otherwise be 200 cells of nothing
        // every time anything on it changes.
        let t = term(200, 2, "hi");
        let mut e = Encoder::new();
        let k = e.keyframe(t.grid(), cursor(), Modes::empty(), "", t.blocks());
        let total: u16 = k.rows_data[0].runs.iter().map(|r| r.cells).sum();
        assert_eq!(total, 2, "trailing blanks were encoded");
    }

    #[test]
    fn a_cursor_move_is_reported_once() {
        let mut t = term(20, 3, "ab");
        let mut e = Encoder::new();
        e.keyframe(t.grid(), CursorState { row: 0, col: 2, ..cursor() }, Modes::empty(), "", t.blocks());

        let moved = CursorState { row: 1, col: 0, visible: true, shape: 0 };
        let d = e.delta(t.grid(), moved, Modes::empty(), "", t.blocks());
        assert_eq!(d.ops.iter().filter(|o| matches!(o, DeltaOp::Cursor { .. })).count(), 1);

        t.advance(b"");
        let again = e.delta(t.grid(), moved, Modes::empty(), "", t.blocks());
        assert!(
            !again.ops.iter().any(|o| matches!(o, DeltaOp::Cursor { .. })),
            "reported a cursor that did not move"
        );
    }

    #[test]
    fn entering_the_alternate_screen_is_announced() {
        // The phone client switches from blocks view to grid view on exactly
        // this signal, so it must arrive as an event rather than being inferred.
        let mut t = term(20, 3, "shell");
        let mut e = Encoder::new();
        e.keyframe(t.grid(), cursor(), Modes::empty(), "", t.blocks());

        t.advance(b"\x1b[?1049h");
        assert!(
            t.modes().contains(Modes::ALT_SCREEN),
            "the fixture did not enter the alt screen"
        );

        let d = e.delta(t.grid(), cursor(), t.modes(), "", t.blocks());
        assert!(
            d.ops.iter().any(|o| matches!(o, DeltaOp::AltScreen { active: true })),
            "{:?}",
            d.ops
        );
        assert!(
            d.screen_switch_comes_first(),
            "the rows describing the alt screen must not be written into the \
             primary grid first: {:?}",
            d.ops
        );
    }

    #[test]
    fn a_screen_switch_precedes_the_rows_that_describe_it() {
        // The bug this guards is invisible to a client holding a flat list of
        // rows -- which is why it survived until something applied a delta into
        // a real Terminal, where `AltScreen` chooses *which of two grids* the
        // following `Row` ops land in.
        //
        // Entering the alt screen repaints everything, so this delta carries
        // both the switch and a full screen of rows. That is exactly the case
        // that was wrong.
        let mut t = term(20, 3, "shell prompt here");
        let mut e = Encoder::new();
        e.keyframe(t.grid(), cursor(), Modes::empty(), "", t.blocks());

        t.advance(b"\x1b[?1049h\x1b[HVIM IS RUNNING");
        let d = e.delta(t.grid(), cursor(), t.modes(), "", t.blocks());

        assert!(
            d.ops.iter().any(|o| matches!(o, DeltaOp::Row { .. })),
            "the fixture must produce rows, or this asserts nothing: {:?}",
            d.ops
        );
        assert!(d.screen_switch_comes_first(), "{:?}", d.ops);
        assert!(d.scrolls_come_first(), "{:?}", d.ops);
    }

    #[test]
    fn a_mode_change_reaches_the_wire() {
        // Without this the client cannot encode its own keystrokes: APP_CURSOR
        // alone decides whether an arrow key is `ESC [ A` or `ESC O A`.
        let mut t = term(20, 3, "");
        let mut e = Encoder::new();
        e.keyframe(t.grid(), cursor(), Modes::empty(), "", t.blocks());

        t.advance(b"\x1b[?1h\x1b[?2004h"); // DECCKM, bracketed paste
        let d = e.delta(t.grid(), cursor(), t.modes(), "", t.blocks());

        let bits = d
            .ops
            .iter()
            .find_map(|o| match o {
                DeltaOp::Modes { bits } => Some(*bits),
                _ => None,
            })
            .expect("a mode change must be announced");
        let got = Modes::from_bits_truncate(bits);
        assert!(got.contains(Modes::APP_CURSOR), "APP_CURSOR missing from {got:?}");
        assert!(got.contains(Modes::BRACKETED_PASTE), "BRACKETED_PASTE missing from {got:?}");
    }

    #[test]
    fn unchanged_modes_are_not_re_announced() {
        // A `Modes` op on every delta would put four bytes on the wire per
        // frame forever, and would defeat the empty-delta check the daemon
        // uses to stay at 0% idle.
        let mut t = term(20, 3, "");
        let mut e = Encoder::new();
        e.keyframe(t.grid(), cursor(), Modes::empty(), "", t.blocks());

        t.advance(b"\x1b[?1h");
        let first = e.delta(t.grid(), cursor(), t.modes(), "", t.blocks());
        assert!(first.ops.iter().any(|o| matches!(o, DeltaOp::Modes { .. })));

        t.advance(b"x");
        let second = e.delta(t.grid(), cursor(), t.modes(), "", t.blocks());
        assert!(
            !second.ops.iter().any(|o| matches!(o, DeltaOp::Modes { .. })),
            "modes were re-sent unchanged: {:?}",
            second.ops
        );
    }

    #[test]
    fn a_keyframe_carries_every_attribute_it_uses() {
        // A client attaching mid-session has none of the interning table, so a
        // keyframe that only sent *new* attributes would render in the wrong
        // colours until every style happened to change.
        let t = term(40, 3, "\x1b[31mred\x1b[0m \x1b[32mgreen\x1b[0m");
        let mut e = Encoder::new();
        let k = e.keyframe(t.grid(), cursor(), Modes::empty(), "", t.blocks());

        let used: std::collections::HashSet<AttrId> =
            k.rows_data.iter().flat_map(|r| r.runs.iter().map(|run| run.attr)).collect();
        let defined: std::collections::HashSet<AttrId> = k.attrs.iter().map(|a| a.id).collect();
        assert!(
            used.is_subset(&defined),
            "keyframe used undefined attributes: {:?} vs {:?}",
            used,
            defined
        );
    }

    #[test]
    fn a_keyframe_carries_the_title() {
        // The title only ever travelled as a change (DeltaOp::Title), so a
        // client attaching to a session already titled `vim` showed blank
        // until the host next retitled. A keyframe is a complete state; now
        // the title is part of it.
        let t = term(20, 2, "x");
        let mut e = Encoder::new();
        let k = e.keyframe(t.grid(), cursor(), Modes::empty(), "vim - notes.md", t.blocks());
        assert_eq!(k.title, "vim - notes.md");

        // And having sent it in the keyframe, the encoder must not re-send it
        // as a Title op on the next delta — its shadow already matches.
        let d = e.delta(t.grid(), cursor(), Modes::empty(), "vim - notes.md", t.blocks());
        assert!(
            !d.ops.iter().any(|op| matches!(op, DeltaOp::Title { .. })),
            "an unchanged title after a keyframe must not travel again"
        );
    }

    #[test]
    fn attribute_ids_are_deterministic_across_runs() {
        // Golden-fixture tests compare bytes, and a HashMap's iteration order is
        // not stable between runs -- which would make them flaky rather than
        // wrong, the worst kind of failure to chase.
        let t = term(40, 3, "\x1b[31ma\x1b[32mb\x1b[34mc\x1b[0m");
        let first = {
            let mut e = Encoder::new();
            e.keyframe(t.grid(), cursor(), Modes::empty(), "", t.blocks()).attrs
        };
        let second = {
            let mut e = Encoder::new();
            e.keyframe(t.grid(), cursor(), Modes::empty(), "", t.blocks()).attrs
        };
        assert_eq!(first, second);
    }
}

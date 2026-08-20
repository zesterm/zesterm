//! Writing state that was computed somewhere else.
//!
//! A session running on another machine is parsed by *that* machine's
//! `Terminal`, shipped as grid deltas, and applied into a local `Terminal`
//! here. `docs/CONTRACTS.md` puts it plainly under "deliberately not
//! abstracted":
//!
//! > A *remote* session keeps a real local `Terminal` that deltas are applied
//! > into, so the renderer's path is identical at both ends of the mesh.
//!
//! [`RemoteWriter`] is that door. It exists so there is exactly one named,
//! documented place that can write a grid without parsing VT — the alternative
//! is making `TermState`'s fields public, after which "who else writes this
//! grid" has no answer short of reading the whole workspace.
//!
//! # The one rule
//!
//! **Never mix this with [`Terminal::advance`] on the same terminal.** A parser
//! and a delta stream are two authorities over one grid and the loser is
//! whichever wrote last. A local session has a pty and no deltas; a remote one
//! has deltas and no pty. Nothing legitimately has both.
//!
//! # Sequence numbers are assigned, not counted
//!
//! On a host, `seq` counts mutations. On a client it *is the host's number* —
//! the value to acknowledge, and the value the next delta will name as its
//! base. So [`RemoteWriter::set_seq`] assigns; it never increments. Applying a
//! batch bumps `seq` incidentally through the ordinary mutation paths, and the
//! assignment at the end of the batch overwrites that with the truth.

use alloc::vec::Vec;

use crate::grid::LineId;
use crate::term::TermState;
use crate::{Cell, CursorStyle, Modes, ScrollRegion, Terminal};

impl Terminal {
    /// A handle for applying state computed on another machine.
    ///
    /// See the [module docs](self) — in particular that this must never be
    /// mixed with [`Terminal::advance`] on the same terminal.
    pub fn remote(&mut self) -> RemoteWriter<'_> {
        RemoteWriter { state: &mut self.state }
    }
}

/// Writes a grid from decoded state rather than from a byte stream.
pub struct RemoteWriter<'a> {
    state: &'a mut TermState,
}

impl RemoteWriter<'_> {
    // --- grid ------------------------------------------------------------

    /// Overwrite one viewport row.
    ///
    /// `cells` shorter than the grid is padded with blanks and longer is
    /// truncated, because the encoder drops trailing blanks and the client is
    /// the side that knows the width. Out-of-range rows are ignored; the caller
    /// is expected to have noticed and asked for a keyframe.
    pub fn write_row(&mut self, row: usize, id: LineId, cells: &[Cell], wrapped: bool) {
        if row >= self.state.grid().rows() {
            return;
        }
        let grid = self.state.grid_mut();
        let target = grid.row_mut(row);
        target.id = id;
        // Both the row flag and the cells are copied exactly as the host sent
        // them, and neither is derived from the other.
        //
        // `Grid::set_wrapped` is the tempting call here and is wrong: it sets
        // `CellFlags::WRAPLINE` on the last cell *from* `row.wrapped`, so on a
        // host row where the two disagree — which happens, because the parser
        // sets them at different moments — it would erase a flag that arrived
        // in the cells. The client's job is to reproduce the host, including
        // any inconsistency in it, not to normalize on the way in.
        target.wrapped = wrapped;
        let dst = target.cells_mut();
        for (i, slot) in dst.iter_mut().enumerate() {
            *slot = cells.get(i).copied().unwrap_or_default();
        }
        self.state.touch();
    }

    /// Attach combining marks to a cell that has already been written.
    ///
    /// Separate from [`Self::write_row`] because the marks travel beside the
    /// text on the wire rather than inside it — see `Run::marks`. Call after
    /// the row, since the row write resets the cell.
    pub fn push_marks(&mut self, row: usize, col: usize, marks: &str) {
        if marks.is_empty() || row >= self.state.grid().rows() {
            return;
        }
        let target = self.state.grid_mut().row_mut(row);
        for mark in marks.chars() {
            target.push_zerowidth(col, mark);
        }
    }

    /// Move rows within a region.
    ///
    /// Positive `lines` moves content up, which is what ordinary output does. A
    /// **full-height region feeds scrollback**, exactly as local output does,
    /// so a client gets correct history for free from the same op that moves
    /// the viewport. A shorter region is a `DECSTBM` scroll and does not.
    pub fn scroll(&mut self, top: usize, bottom: usize, lines: i32) {
        if lines == 0 || top > bottom {
            return;
        }
        let template = self.state.template;
        let rows = self.state.grid().rows();
        let full = top == 0 && bottom + 1 >= rows;

        let grid = self.state.grid_mut();
        let saved = grid.region;
        if !full {
            grid.region = ScrollRegion { top, bottom: bottom.min(rows.saturating_sub(1)) };
        }
        #[allow(clippy::cast_sign_loss, reason = "sign is tested immediately above")]
        if lines > 0 {
            grid.scroll_up(lines as usize, &template);
        } else {
            grid.scroll_down((-lines) as usize, &template);
        }
        if !full {
            grid.region = saved;
        }
        self.state.touch_full();
    }

    /// Clear a rectangle to an attribute, corners inclusive.
    pub fn erase(&mut self, top: usize, left: usize, bottom: usize, right: usize, template: &Cell) {
        let rows = self.state.grid().rows();
        let cols = self.state.grid().cols();
        if top > bottom || left > right || top >= rows || left >= cols {
            return;
        }
        let grid = self.state.grid_mut();
        for row in top..=bottom.min(rows - 1) {
            grid.erase_in_row(row, left, right.min(cols - 1), template);
        }
        self.state.touch();
    }

    /// Prepend history this client did not hold, oldest first.
    ///
    /// For `RequestScrollback`: a keyframe is a *viewport*, so anything that
    /// scrolled past before this client attached exists only on the host.
    pub fn prepend_history(&mut self, rows: &[(LineId, Vec<Cell>, bool)]) {
        if rows.is_empty() {
            return;
        }
        self.state.grid_mut().push_history(rows);
        self.state.touch_full();
    }

    /// Drop history this keyframe is about to re-deliver as viewport rows.
    ///
    /// Call with the line ids the keyframe carries, before writing its rows.
    /// See [`crate::grid::Grid::drop_scrollback_rows`] — without it a settled
    /// grow leaves the same line in both halves of this grid and everything
    /// that walks the session by id shows it twice (#291); and exactly the
    /// *named* lines, never an id sweep, or a client's only copy of rows the
    /// host destroyed goes with them (#313).
    pub fn drop_history(&mut self, named: &[LineId]) {
        if self.state.grid_mut().drop_scrollback_rows(named) > 0 {
            self.state.touch_full();
        }
    }

    /// File the top `n` viewport rows as history, before a keyframe that
    /// starts later than they do overwrites the only copy of them.
    ///
    /// The replica half of the host's strand (#341); see
    /// [`crate::grid::Grid::bank_viewport_top`]. The exclusive counterpart of
    /// [`Self::drop_history`]: a keyframe either re-delivers rows this grid
    /// filed as history (take back the stale copies) or starts beyond rows it
    /// still shows (bank them) — never both, because the first requires the
    /// newest held line to be at or past the keyframe's first and the second
    /// requires the opposite.
    pub fn bank_displaced(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        let template = self.state.template;
        self.state.grid_mut().bank_viewport_top(n, &template);
        self.state.touch_full();
    }

    /// Destroy this replica's scrollback, because the host's keyframe says an
    /// ED 3 destroyed the session's.
    ///
    /// The *primary* grid's, like the host's own [`Grid::clear_history`] —
    /// scrollback lives there whichever screen is active. Reaches history the
    /// host never held too: a client deliberately keeping more than the host
    /// still drops it on a `cls`, which is what separates announced
    /// destruction from silent eviction. (#314)
    pub fn clear_history(&mut self) {
        self.state.grid.clear_history();
        self.state.touch_full();
    }

    /// Keep the client's line-id counter level with the host's.
    ///
    /// Rows exposed by a scroll are stamped from the *client's* counter, which
    /// knows nothing of the host's. Every such row is re-sent by the encoder
    /// and restamped by [`Self::write_row`], so the cells are right either way
    /// — but `Grid::next_line_id` and therefore `ChangeSource::oldest_line`
    /// would drift, and command blocks are indexed by absolute line.
    pub fn sync_next_line_id(&mut self, next: LineId) {
        self.state.grid_mut().set_next_line_id(next);
    }

    // --- terminal state --------------------------------------------------

    pub fn set_cursor(&mut self, row: usize, col: usize) {
        let grid = self.state.grid_mut();
        grid.cursor.row = row.min(grid.rows().saturating_sub(1));
        grid.cursor.col = col.min(grid.cols().saturating_sub(1));
        self.state.touch();
    }

    pub fn set_cursor_style(&mut self, style: CursorStyle) {
        self.state.cursor_style = style;
        self.state.touch();
    }

    /// Switch between the primary and alternate screens.
    ///
    /// **Must be applied before the rows of the batch that carries it**, or the
    /// rows describing one screen land in the other. The encoder guarantees the
    /// order and `Delta::screen_switch_comes_first` asserts it.
    pub fn set_alt_screen(&mut self, active: bool) {
        self.state.set_alt_screen(active);
    }

    pub fn set_title(&mut self, title: &str) {
        self.state.title.clear();
        self.state.title.push_str(title);
        self.state.touch();
    }

    /// Mirror the host's `OSC 9;4` progress.
    ///
    /// No `touch()`: a progress tick changes no cell, and bumping the damage
    /// sequence for one would make a build's per-percent chatter look like
    /// grid output to every subscriber below. The chrome reads this directly
    /// and repaints on its own invalidation.
    pub fn set_progress(&mut self, progress: crate::term::Progress) {
        self.state.progress = progress;
    }

    /// Mirror the host's modes.
    ///
    /// This is what lets an attached client encode its own keystrokes: whether
    /// an arrow key is `ESC [ A` or `ESC O A` is `APP_CURSOR`, and the client
    /// is the side holding the keyboard.
    ///
    /// `ALT_SCREEN` is applied through [`Self::set_alt_screen`] rather than
    /// written straight into the bits, because entering the alternate screen
    /// allocates a second grid — setting the bit alone would claim a screen
    /// switch that never happened.
    pub fn set_modes(&mut self, modes: Modes) {
        let want_alt = modes.contains(Modes::ALT_SCREEN);
        if want_alt != self.state.modes.contains(Modes::ALT_SCREEN) {
            self.state.set_alt_screen(want_alt);
        }
        self.state.modes = modes;
        self.state.touch();
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        // A keyframe is about to restate every visible row, so a grow must not
        // pull this replica's history down into rows that are about to be
        // overwritten. It is ConPTY's argument one layer out, and it went wrong
        // the same way: the pull moves those rows out of scrollback, the
        // keyframe blanks what it now owns, and history is destroyed rather
        // than misplaced -- host-side that was #200, and the client had been
        // doing it all along because nothing here ever set the flag. (#247)
        //
        // Set on this door rather than where a replica is built, because *this
        // door is what makes a terminal a replica*: a grid resized through it
        // has its authority somewhere else by definition, and there is no
        // second place to remember. A replica never settles the debt either --
        // settling runs from the parser, and the module rule above is that
        // nothing mixes the two.
        self.state.set_viewport_restated_elsewhere(true);
        self.state.resize(cols, rows);
    }

    /// Insert or replace a command block the host computed.
    ///
    /// Blocks arrive whole rather than as markers to replay: the shell talked to
    /// the machine it runs on, and that machine's parser already decided where
    /// the command began. A client re-deriving them from the grid would be the
    /// second VT interpretation ADR-004 exists to avoid.
    ///
    /// The lines it names are meaningful here only because
    /// [`Self::sync_next_line_id`] keeps this grid's numbering the host's.
    pub fn upsert_block(&mut self, block: crate::Block) {
        self.state.blocks.upsert(block);
        self.state.touch();
    }

    /// Drop blocks from `first` up, before a keyframe restates them.
    ///
    /// A keyframe is a complete state, but applying one only ever *added*: a
    /// block the host destroyed rather than evicted — `cls` erasing the rows it
    /// described — survived on the client for ever and painted a stale header
    /// over the live prompt. Trimming from the host's oldest id rather than
    /// clearing keeps the older history a client may deliberately hold beyond
    /// what the host retains.
    pub fn drop_blocks_from(&mut self, first: crate::BlockId) {
        self.state.blocks.retain_below(first);
        self.state.touch();
    }

    // --- bookkeeping -----------------------------------------------------

    /// Adopt the host's sequence number. **Assigned, never incremented.**
    ///
    /// Call last in a batch, so the value describes a fully applied state: it
    /// is what the client acknowledges, and acknowledging a half-applied batch
    /// tells the host to send a delta from a state that does not exist.
    ///
    /// Deliberately unchecked. A monotonicity assertion here looks obviously
    /// right and is wrong: every write above bumps `seq` through the ordinary
    /// mutation path, so between two assignments the field is a local mutation
    /// count that is *expected* to exceed the host's number and then be
    /// overwritten by it. Ordering is the caller's to enforce, and it has the
    /// information to — a client discards any update whose `base` is not the
    /// sequence it last applied.
    pub fn set_seq(&mut self, seq: u64) {
        self.state.seq = seq;
    }

    /// Mark the viewport dirty without changing it.
    pub fn mark(&mut self) {
        self.state.touch();
    }

    /// Mark everything dirty — a scroll, a screen switch, a resize.
    pub fn mark_full(&mut self) {
        self.state.touch_full();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cell(ch: char) -> Cell {
        Cell { ch, ..Cell::default() }
    }

    fn row_of(text: &str) -> Vec<Cell> {
        text.chars().map(cell).collect()
    }

    #[test]
    fn a_written_row_carries_the_hosts_line_id() {
        // The absolute id is what makes scroll detection exact and what command
        // blocks are indexed by. A client that stamps its own would answer
        // "which line is this" differently from the host.
        let mut t = Terminal::new(10, 3, 100);
        t.remote().write_row(1, 42, &row_of("hello"), false);
        assert_eq!(t.grid().row(1).id, 42);
        assert_eq!(t.grid().row(1).text().trim_end(), "hello");
    }

    #[test]
    fn a_short_row_is_padded_rather_than_left_stale() {
        // The encoder drops trailing blanks, so almost every row arrives short.
        // Leaving the tail alone would keep whatever was there before.
        let mut t = Terminal::new(10, 3, 100);
        t.remote().write_row(0, 1, &row_of("XXXXXXXXXX"), false);
        t.remote().write_row(0, 2, &row_of("ab"), false);
        assert_eq!(t.grid().row(0).text().trim_end(), "ab");
    }

    #[test]
    fn a_full_height_scroll_feeds_scrollback() {
        // The client gets correct history from the same op that moves the
        // viewport -- no separate bookkeeping, and nothing to get out of step.
        let mut t = Terminal::new(10, 3, 100);
        for r in 0..3 {
            t.remote().write_row(r, r as LineId, &row_of("line"), false);
        }
        assert_eq!(t.grid().scrollback_len(), 0);
        t.remote().scroll(0, 2, 1);
        assert_eq!(t.grid().scrollback_len(), 1, "the displaced row became history");
    }

    #[test]
    fn a_replica_grow_does_not_pull_its_own_history_into_the_viewport() {
        // The keyframe that follows this resize restates every visible row, so
        // rows pulled down to meet it are overwritten -- and the pull has
        // already moved them out of scrollback, so they are gone from the client
        // entirely while the host still holds them. That is #200 exactly, one
        // layer out, and it was live in every client because nothing here ever
        // told the grid it was a replica. (#247)
        let mut t = Terminal::new(10, 6, 100);
        for r in 0..6 {
            t.remote().write_row(r, r as LineId, &row_of("line"), false);
        }
        t.remote().scroll(0, 5, 4);
        // On the last row, so the shrink has no blank rows below the cursor to
        // give up and has to take them over the top -- the real gesture, and the
        // only one where a grow has anything to pull back.
        t.remote().set_cursor(5, 0);
        let history = t.grid().scrollback_len();
        assert_eq!(history, 4, "the fixture did not build any history");

        t.remote().resize(10, 3);
        assert_eq!(t.grid().scrollback_len(), history + 3, "the shrink banked nothing");
        t.remote().resize(10, 6);

        assert_eq!(
            t.grid().scrollback_len(),
            history + 3,
            "the grow pulled history into rows the keyframe is about to overwrite"
        );
    }

    #[test]
    fn a_region_scroll_does_not_feed_scrollback() {
        // A DECSTBM scroll inside a region is not lines leaving the screen.
        // Treating it as history is how a client's scrollback fills with the
        // middle of a redrawing TUI.
        let mut t = Terminal::new(10, 5, 100);
        t.remote().scroll(1, 3, 1);
        assert_eq!(t.grid().scrollback_len(), 0);
    }

    #[test]
    fn a_negative_scroll_moves_content_down() {
        let mut t = Terminal::new(10, 3, 100);
        t.remote().write_row(0, 0, &row_of("top"), false);
        t.remote().scroll(0, 2, -1);
        assert_eq!(t.grid().row(1).text().trim_end(), "top");
    }

    #[test]
    fn history_is_prepended_oldest_first() {
        let mut t = Terminal::new(10, 2, 100);
        t.remote().prepend_history(&[
            (100, row_of("older"), false),
            (101, row_of("newer"), false),
        ]);
        assert_eq!(t.grid().scrollback_len(), 2);
        assert_eq!(t.grid().line(0).map(|r| r.text().trim_end().to_string()).unwrap(), "older");
        assert_eq!(t.grid().line(1).map(|r| r.text().trim_end().to_string()).unwrap(), "newer");
    }

    #[test]
    fn the_sequence_is_adopted_not_counted() {
        // On a client `seq` is the host's number: the value to acknowledge, and
        // the base the next delta will name. Counting local mutations here
        // would ack a sequence the host never sent.
        let mut t = Terminal::new(10, 3, 100);
        t.remote().write_row(0, 0, &row_of("a"), false);
        t.remote().write_row(1, 1, &row_of("b"), false);
        t.remote().set_seq(9_000);
        assert_eq!(t.seq(), 9_000);
    }

    #[test]
    fn setting_the_alt_screen_bit_switches_the_screen() {
        // Writing the bit straight into `modes` would claim a screen switch
        // that never happened -- the alternate grid is allocated lazily.
        let mut t = Terminal::new(10, 3, 100);
        t.remote().write_row(0, 0, &row_of("primary"), false);

        t.remote().set_modes(Modes::ALT_SCREEN);
        assert!(t.modes().contains(Modes::ALT_SCREEN));
        assert_ne!(t.grid().row(0).text().trim_end(), "primary", "still on the primary grid");

        t.remote().set_modes(Modes::empty());
        assert_eq!(t.grid().row(0).text().trim_end(), "primary", "the primary grid survived");
    }

    #[test]
    fn a_row_past_the_end_is_ignored_rather_than_growing_the_grid() {
        // The grid's size is something the shell on the other side also
        // believes in. Growing it here to fit a stray row would leave the two
        // permanently disagreeing about how wide the screen is.
        let mut t = Terminal::new(10, 3, 100);
        t.remote().write_row(99, 99, &row_of("nope"), false);
        assert_eq!(t.grid().rows(), 3);
    }

    #[test]
    fn a_keyframe_that_gives_history_back_does_not_leave_two_copies_of_it() {
        // The host settles a grow and its keyframe's viewport starts earlier
        // than it did, so every row it re-sends is one this replica already
        // holds above the boundary. Left there the same line exists twice in
        // one `Storage`, and everything that walks the session by id shows it
        // twice: the listing duplicated, and a block whose range spans both
        // copies drawing on its own.
        //
        // #247 fixed this shape in the web client and missed the Rust one. It
        // stayed invisible until #281 made the host's settle actually fire,
        // which is what a grow keyframe re-delivering anything depends on.
        let mut t = Terminal::new(10, 6, 100);
        for r in 0..6 {
            t.remote().write_row(r, r as LineId, &row_of("line"), false);
        }
        t.remote().set_cursor(5, 0);

        // Shrunk: four rows go over the top and become this replica's history.
        t.remote().resize(10, 2);
        assert_eq!(t.grid().scrollback_len(), 4, "the shrink banked nothing");

        // Grown, and the host settled -- so the keyframe carries lines 0..5
        // again, four of which are sitting in scrollback right now.
        t.remote().resize(10, 6);
        t.remote().drop_history(&[0, 1, 2, 3, 4, 5]);
        for r in 0..6 {
            t.remote().write_row(r, r as LineId, &row_of("line"), false);
        }

        let ids: Vec<LineId> =
            (0..t.grid().total_lines()).map(|i| t.grid().line(i).unwrap().id).collect();
        assert_eq!(ids, vec![0, 1, 2, 3, 4, 5], "a line is held twice: {ids:?}");
        assert_eq!(t.grid().scrollback_len(), 0, "history the viewport now holds is not history");
    }

    #[test]
    fn dropping_history_holds_a_scrolled_back_reader_still() {
        // The rows dropped here are the ones the viewport is about to hold, so
        // the same content is in the same place afterwards — one boundary
        // further down. `viewport_base` is measured from the end of storage,
        // which just got shorter, so leaving the offset alone slides the reader
        // into the past while they are reading. (#291)
        let mut t = Terminal::new(10, 4, 100);
        for r in 0..4 {
            t.remote().write_row(r, r as LineId, &row_of("live"), false);
        }
        // Eight lines of history, oldest first, so there is somewhere to scroll.
        let hist: Vec<(LineId, Vec<Cell>, bool)> = (0..8)
            .map(|i| (100 + i as LineId, row_of(&format!("old{i}")), false))
            .collect();
        t.remote().prepend_history(&hist);
        t.scroll_display(5);
        let reading = t.grid().row(0).text().trim_end().to_string();
        assert!(reading.starts_with("old"), "the fixture is not scrolled back: {reading:?}");

        // A keyframe re-delivers the newest two history lines as viewport rows.
        t.remote().drop_history(&[106, 107]);

        assert_eq!(
            t.grid().row(0).text().trim_end(),
            reading,
            "the text under the reader moved when history was de-duplicated"
        );
    }

    #[test]
    fn history_the_keyframe_never_names_is_not_dropped_on_an_id_comparison() {
        // Line ids have gaps -- `truncate_bottom` destroys the newest ids
        // without rewinding the counter -- so "id at or above the keyframe's
        // first" is not the same set as "id the keyframe names", and the
        // difference is exactly the rows a client holds that the host has
        // destroyed. Sweeping them on an id comparison deletes the client's
        // only copy, unrecoverably; keeping them at worst keeps a row the
        // host blanked. The web client wrote this argument down
        // (`grid-view.ts`) and the Rust replica shipped the opposite rule;
        // two reference decoders, two semantics, and this is the half that
        // turned a host-side bug into "text gone" in the window. (#313)
        let mut t = Terminal::new(10, 4, 100);
        for r in 0..4 {
            t.remote().write_row(r, 108 + r as LineId, &row_of("live"), false);
        }
        let hist: Vec<(LineId, Vec<Cell>, bool)> = (0..8)
            .map(|i| (100 + i as LineId, row_of(&format!("old{i}")), false))
            .collect();
        t.remote().prepend_history(&hist);

        // The keyframe re-delivers line 104 in its viewport; 105..=107 fell
        // into a gap. Only the named line may go.
        t.remote().drop_history(&[104, 108, 109, 110]);

        let held: Vec<LineId> =
            (0..t.grid().scrollback_len()).map(|i| t.grid().line(i).unwrap().id).collect();
        assert_eq!(
            held,
            vec![100, 101, 102, 103, 105, 106, 107],
            "history the keyframe never named was dropped on an id comparison"
        );
    }

    #[test]
    fn dropping_history_leaves_what_the_viewport_does_not_name() {
        // The common case is a no-op, and it has to be: an ordinary keyframe's
        // viewport is newer than everything in history, and a floor that swept
        // more than it should would quietly delete a client's scrollback on
        // every frame.
        let mut t = Terminal::new(10, 3, 100);
        for r in 0..3 {
            t.remote().write_row(r, r as LineId, &row_of("line"), false);
        }
        t.remote().scroll(0, 2, 3);
        let before = t.grid().scrollback_len();
        assert!(before >= 3, "the fixture built no history");

        t.remote().drop_history(&[90, 91, 92]);

        assert_eq!(t.grid().scrollback_len(), before, "history the keyframe never named was dropped");
    }
}

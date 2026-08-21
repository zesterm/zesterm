//! Row storage: a ring buffer with a rotating origin.
//!
//! # Why a ring
//!
//! Scrolling a terminal by one line is the single most common structural
//! operation there is — every line of output does it once the screen is full.
//! Done naively (`Vec::remove(0)` plus `push`) it is a memmove of the entire
//! scrollback per line, which is the difference between ~50 MB/s and ~500 MB/s
//! of throughput.
//!
//! Here, scrolling advances `origin` and recycles one row. That is O(1), and
//! because [`Row::reset`] keeps its `Vec<Cell>` allocation, steady-state
//! scrolling performs **zero allocations**.
//!
//! The idea is borrowed from `alacritty_terminal`'s `grid::storage` — read, not
//! copied.

use alloc::vec::Vec;

use crate::cell::{Cell, CellFlags, ExtraId, NO_EXTRA};

/// Monotonic identifier for a line, counted from the first line ever written.
///
/// Never reused, and survives scrollback eviction. Three later features depend
/// on this being stable rather than a viewport offset:
///
/// - selection that survives scrolling and eviction,
/// - OSC 133 command blocks, which mark a *range* of lines,
/// - the M3 remote protocol, which fetches scrollback by absolute range.
///
/// Getting this in now costs nothing; retrofitting it would mean reworking all
/// three.
pub type LineId = u64;

/// Extra per-cell data, for the rare cell that needs more than one `char`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CellExtra {
    /// Combining marks applied to the base character.
    pub zerowidth: Vec<char>,
    /// OSC 8 hyperlink id, if this cell is part of a link.
    pub hyperlink: Option<u16>,
}

/// One line of the terminal.
#[derive(Debug, Clone, Default)]
pub struct Row {
    cells: Vec<Cell>,
    /// Side table for cells whose `extra` is not [`NO_EXTRA`].
    extras: Vec<CellExtra>,
    /// Absolute id of this line.
    pub id: LineId,
}

impl Row {
    #[must_use]
    pub fn new(cols: usize, id: LineId) -> Self {
        Self {
            cells: alloc::vec![Cell::default(); cols],
            extras: Vec::new(),
            id,
        }
    }

    /// True if this row wrapped into the next rather than ending.
    ///
    /// Read from [`CellFlags::WRAPLINE`] on the last cell rather than stored
    /// beside the cells, deliberately. Kept as a second copy, the two halves
    /// drifted the moment a row was overwritten in place with no erase — the
    /// new last cell forgot the wrap while the copy went on claiming it, and
    /// the next reflow rejoined rows that were never one logical line. The
    /// cell is self-maintaining: whatever replaces it takes the flag with it.
    /// (#219; the erase half of the same drift was #200.)
    #[must_use]
    pub fn wrapped(&self) -> bool {
        self.cells.last().is_some_and(|c| c.flags.contains(CellFlags::WRAPLINE))
    }

    /// Record whether this row continues into the next.
    pub fn set_wrapped(&mut self, wrapped: bool) {
        if let Some(last) = self.cells.last_mut() {
            last.flags.set(CellFlags::WRAPLINE, wrapped);
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    #[must_use]
    pub fn cells(&self) -> &[Cell] {
        &self.cells
    }

    pub fn cells_mut(&mut self) -> &mut [Cell] {
        &mut self.cells
    }

    #[must_use]
    pub fn get(&self, col: usize) -> Option<&Cell> {
        self.cells.get(col)
    }

    pub fn get_mut(&mut self, col: usize) -> Option<&mut Cell> {
        self.cells.get_mut(col)
    }

    /// Resolve a cell's side-table entry, if it has one.
    #[must_use]
    pub fn extra(&self, cell: &Cell) -> Option<&CellExtra> {
        if cell.extra == NO_EXTRA {
            None
        } else {
            self.extras.get(cell.extra as usize)
        }
    }

    /// Attach a combining mark to the cell at `col`.
    ///
    /// Allocates a side-table slot on first use for that cell. Returns silently
    /// if the table is full — losing a combining mark is far better than
    /// failing to render a line, and `u16::MAX` entries in one row is already
    /// pathological.
    pub fn push_zerowidth(&mut self, col: usize, mark: char) {
        let Some(cell) = self.cells.get_mut(col) else { return };
        if cell.extra == NO_EXTRA {
            if self.extras.len() >= NO_EXTRA as usize {
                return;
            }
            cell.extra = self.extras.len() as ExtraId;
            self.extras.push(CellExtra::default());
        }
        if let Some(e) = self.extras.get_mut(cell.extra as usize) {
            e.zerowidth.push(mark);
        }
    }

    /// One cell together with whatever the side table holds for it.
    ///
    /// Reflow moves cells between rows, and `Cell::extra` is an index into the
    /// row that owns it — carrying the cell alone would leave every combining
    /// mark and hyperlink pointing into the wrong table.
    #[must_use]
    pub fn detach(&self, col: usize) -> Option<(Cell, Option<CellExtra>)> {
        let cell = *self.cells.get(col)?;
        Some((cell, self.extra(&cell).cloned()))
    }

    /// Put a detached cell at `col`, re-interning its side-table entry.
    pub fn attach(&mut self, col: usize, cell: Cell, extra: Option<CellExtra>) {
        let Some(slot) = self.cells.get_mut(col) else { return };
        *slot = cell;
        match extra {
            Some(e) if self.extras.len() < NO_EXTRA as usize => {
                let id = self.extras.len() as ExtraId;
                self.extras.push(e);
                if let Some(slot) = self.cells.get_mut(col) {
                    slot.extra = id;
                }
            }
            // No side data, or no room for it. Losing a combining mark is far
            // better than losing the line it was attached to.
            _ => {
                if let Some(slot) = self.cells.get_mut(col) {
                    slot.extra = NO_EXTRA;
                }
            }
        }
    }

    /// Blank the row, keeping its allocation. This is what makes steady-state
    /// scrolling allocation-free.
    pub fn reset(&mut self, template: &Cell, id: LineId) {
        let blank = Cell::blank_with(template);
        self.cells.fill(blank);
        self.extras.clear();
        self.id = id;
    }

    /// Grow or shrink to `cols`.
    ///
    /// Does not reflow — growing pads with blanks and shrinking truncates.
    /// The wrap fact goes with a truncated last cell (and a grow's fresh
    /// blank last cell reads as unwrapped). This is the alternate screen's
    /// path, which is never reflowed, so what that can cost is only the
    /// newline-joining of a copy taken between the resize and the repaint
    /// every full-screen program answers it with.
    pub fn resize(&mut self, cols: usize, template: &Cell) {
        self.cells.resize(cols, Cell::blank_with(template));
    }

    /// Trailing blanks are not worth transmitting or rendering; this is where
    /// the meaningful content ends.
    #[must_use]
    pub fn trimmed_len(&self) -> usize {
        self.cells
            .iter()
            .rposition(|c| !c.is_blank() || c.bg != Cell::default().bg)
            .map_or(0, |i| i + 1)
    }

    /// The row's text, ignoring attributes. Combining marks are included.
    #[must_use]
    pub fn text(&self) -> alloc::string::String {
        let mut s = alloc::string::String::with_capacity(self.cells.len());
        for cell in &self.cells[..self.trimmed_len()] {
            if cell.flags.contains(CellFlags::WIDE_SPACER) {
                continue;
            }
            s.push(cell.ch);
            if let Some(extra) = self.extra(cell) {
                s.extend(extra.zerowidth.iter().copied());
            }
        }
        s
    }
}

/// A ring of rows with a rotating origin.
#[derive(Debug)]
pub struct Storage {
    rows: Vec<Row>,
    /// Index into `rows` of the logical first line.
    origin: usize,
    /// Id to assign the next newly-exposed line.
    next_id: LineId,
}

impl Storage {
    #[must_use]
    pub fn new(len: usize, cols: usize) -> Self {
        let rows: Vec<Row> = (0..len).map(|i| Row::new(cols, i as LineId)).collect();
        Self { rows, origin: 0, next_id: len as LineId }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// The id the next exposed line will receive.
    #[must_use]
    pub fn next_id(&self) -> LineId {
        self.next_id
    }

    /// Adopt a counter decided elsewhere.
    ///
    /// Only for a grid being written from a remote host's state: its line ids
    /// are the host's, and a client counting its own would answer "which line
    /// is this" differently from the machine the shell is running on.
    /// Monotonic, so a late message cannot rewind it.
    pub fn set_next_id(&mut self, id: LineId) {
        self.next_id = self.next_id.max(id);
    }

    /// Insert rows above the oldest, as history that arrived after the fact.
    ///
    /// `resize_rows` only grows at the bottom, which is right for a viewport
    /// that got taller and wrong for scrollback fetched with
    /// `RequestScrollback` — that arrives *older* than everything held.
    pub fn prepend(&mut self, rows: &[(LineId, Vec<Cell>, bool)], cols: usize) {
        if rows.is_empty() {
            return;
        }
        // Rebuilt in logical order rather than rotated in place: `origin` makes
        // "insert before the first" a wrapping splice, and getting that subtly
        // wrong shows up as history in the wrong order much later.
        let mut out: Vec<Row> = Vec::with_capacity(self.rows.len() + rows.len());
        for (id, cells, wrapped) in rows {
            let mut row = Row::new(cols, *id);
            let dst = row.cells_mut();
            for (i, slot) in dst.iter_mut().enumerate() {
                *slot = cells.get(i).copied().unwrap_or_default();
            }
            // After the cells, which would overwrite the flag; and from the
            // caller's bool rather than trusting the copied cells, because the
            // encoder trims trailing blanks -- the cell that carried WRAPLINE
            // may never have been transmitted.
            row.set_wrapped(*wrapped);
            out.push(row);
        }
        for i in 0..self.rows.len() {
            out.push(self.rows[self.physical(i)].clone());
        }
        self.rows = out;
        self.origin = 0;
    }

    #[inline]
    fn physical(&self, logical: usize) -> usize {
        debug_assert!(logical < self.rows.len(), "row {logical} out of range");
        (self.origin + logical) % self.rows.len()
    }

    #[must_use]
    pub fn row(&self, logical: usize) -> &Row {
        &self.rows[self.physical(logical)]
    }

    pub fn row_mut(&mut self, logical: usize) -> &mut Row {
        let p = self.physical(logical);
        &mut self.rows[p]
    }

    /// Rotate the ring so that logical line `by` becomes line 0.
    ///
    /// This is the O(1) scroll: no rows move, only the origin.
    pub fn rotate_up(&mut self, by: usize) {
        if self.rows.is_empty() {
            return;
        }
        self.origin = (self.origin + by) % self.rows.len();
    }

    /// Rotate the other way, for reverse scroll and scrollback traversal.
    pub fn rotate_down(&mut self, by: usize) {
        if self.rows.is_empty() {
            return;
        }
        let n = self.rows.len();
        self.origin = (self.origin + n - (by % n)) % n;
    }

    /// Recycle the row at `logical`, assigning it a fresh absolute id.
    pub fn recycle(&mut self, logical: usize, template: &Cell) {
        let id = self.next_id;
        self.next_id += 1;
        let p = self.physical(logical);
        self.rows[p].reset(template, id);
    }

    /// Grow or shrink the ring.
    ///
    /// Normalizes the origin to 0 first: reallocating a rotated ring in place
    /// would scramble line order, and resize is rare enough that the memmove
    /// does not matter.
    pub fn resize_rows(&mut self, new_len: usize, cols: usize, template: &Cell) {
        if new_len == self.rows.len() {
            return;
        }
        self.normalize();
        if new_len > self.rows.len() {
            let extra = new_len - self.rows.len();
            for _ in 0..extra {
                let id = self.next_id;
                self.next_id += 1;
                let mut row = Row::new(cols, id);
                row.reset(template, id);
                self.rows.push(row);
            }
        } else {
            // Drop from the top -- the oldest scrollback -- not the bottom,
            // which holds the visible screen.
            self.rows.drain(..self.rows.len() - new_len);
        }
    }

    /// Drop `n` rows from the bottom — the newest, nearest the cursor.
    ///
    /// The counterpart of `resize_rows`, which drops from the top. Shrinking a
    /// viewport needs both: blank rows below the cursor are given up first, and
    /// only what cannot be found there is taken off the top, where it becomes
    /// scrollback.
    pub fn truncate_bottom(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        self.normalize();
        let keep = self.rows.len().saturating_sub(n);
        self.rows.truncate(keep.max(1));
    }

    /// Remove `len` rows starting at logical index `start`.
    ///
    /// The one op that takes rows out of the *middle*. `resize_rows` drops from
    /// the top and `truncate_bottom` from the bottom, and neither can express
    /// "these rows are a duplicate of rows further down", which is what a
    /// replica has after a keyframe re-delivers lines it already held as
    /// history. See `Grid::drop_scrollback_rows`.
    ///
    /// **Never empties the ring.** `physical` divides by `rows.len()` and
    /// `Grid::oldest_line_id` reads row 0 unconditionally, so a storage with no
    /// rows is not a degraded state but a panic waiting for the next frame —
    /// and it would land far from whatever asked for the impossible range.
    /// `truncate_bottom` keeps its last row for the same reason.
    ///
    /// `pub(super)` because only `Grid` may move the boundary this belongs to;
    /// a caller outside it holding a `Storage` has no way to keep
    /// `scrollback_len` honest.
    pub(super) fn remove_range(&mut self, start: usize, len: usize) {
        if len == 0 || start >= self.rows.len() {
            return;
        }
        self.normalize();
        let end = (start + len).min(self.rows.len());
        debug_assert!(
            end - start < self.rows.len(),
            "remove_range({start}, {len}) would empty a {}-row storage",
            self.rows.len()
        );
        if end - start >= self.rows.len() {
            return;
        }
        self.rows.drain(start..end);
    }

    pub fn resize_cols(&mut self, cols: usize, template: &Cell) {
        for row in &mut self.rows {
            row.resize(cols, template);
        }
    }

    /// Rotate the backing store so logical order matches physical order.
    fn normalize(&mut self) {
        if self.origin != 0 {
            self.rows.rotate_left(self.origin);
            self.origin = 0;
        }
    }

    pub fn iter(&self) -> impl Iterator<Item = &Row> {
        (0..self.rows.len()).map(move |i| self.row(i))
    }

    /// Replace every row, in logical order.
    pub fn replace_all(&mut self, rows: Vec<Row>) {
        self.rows = rows;
        self.origin = 0;
    }

    /// Rows in logical order, taken out.
    pub fn take_all(&mut self) -> Vec<Row> {
        self.normalize();
        core::mem::take(&mut self.rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(s: &Storage) -> Vec<LineId> {
        (0..s.len()).map(|i| s.row(i).id).collect()
    }

    #[test]
    fn rotation_is_order_preserving() {
        let mut s = Storage::new(4, 10);
        assert_eq!(ids(&s), vec![0, 1, 2, 3]);

        s.rotate_up(1);
        assert_eq!(ids(&s), vec![1, 2, 3, 0], "row 0 wrapped to the end");

        s.rotate_down(1);
        assert_eq!(ids(&s), vec![0, 1, 2, 3], "and back");
    }

    #[test]
    fn rotation_wraps_past_the_end() {
        let mut s = Storage::new(3, 4);
        s.rotate_up(7); // 7 % 3 == 1
        assert_eq!(ids(&s), vec![1, 2, 0]);
    }

    #[test]
    fn rotate_down_past_zero_does_not_underflow() {
        let mut s = Storage::new(3, 4);
        s.rotate_down(1);
        assert_eq!(ids(&s), vec![2, 0, 1]);
        s.rotate_down(5);
        assert_eq!(ids(&s), vec![0, 1, 2]);
    }

    #[test]
    fn scrolling_assigns_fresh_monotonic_ids() {
        let mut s = Storage::new(3, 4);
        let template = Cell::default();

        // Scroll once: the top row is recycled to the bottom with a new id.
        s.rotate_up(1);
        s.recycle(2, &template);
        assert_eq!(ids(&s), vec![1, 2, 3]);

        s.rotate_up(1);
        s.recycle(2, &template);
        assert_eq!(ids(&s), vec![2, 3, 4], "ids keep counting, never reused");
    }

    #[test]
    fn recycling_keeps_the_allocation() {
        let mut s = Storage::new(2, 8);
        let ptr_before = s.row(0).cells().as_ptr();
        s.recycle(0, &Cell::default());
        let ptr_after = s.row(0).cells().as_ptr();
        assert_eq!(ptr_before, ptr_after, "steady-state scrolling must not allocate");
    }

    #[test]
    fn growing_preserves_order_even_when_rotated() {
        let mut s = Storage::new(3, 4);
        s.rotate_up(2); // ids now [2, 0, 1]
        s.resize_rows(5, 4, &Cell::default());
        let got = ids(&s);
        assert_eq!(&got[..3], &[2, 0, 1], "existing rows keep their logical order");
        assert_eq!(got.len(), 5);
    }

    #[test]
    fn removing_a_range_takes_only_that_range() {
        // The degenerate inputs are no-ops rather than errors, because
        // `Grid::drop_scrollback_rows` computes a count that is legitimately
        // zero on almost every keyframe.
        //
        // Emptying the ring is *not* tested here, deliberately: it cannot be
        // asked for -- `scrollback_len` is always less than `storage.len()`,
        // since the viewport is at least one row -- so it is a caller bug, and
        // the `debug_assert` in `remove_range` is what says so. A test would
        // have to violate the contract to reach the release-mode guard behind
        // it, and would then be asserting that a bug degrades quietly.
        let mut s = Storage::new(4, 4);
        s.remove_range(1, 0);
        assert_eq!(s.len(), 4, "a zero-length removal took something");
        s.remove_range(9, 1);
        assert_eq!(s.len(), 4, "a removal past the end took something");

        s.remove_range(1, 2);
        assert_eq!(ids(&s), alloc::vec![0, 3], "the wrong rows were removed");
    }

    #[test]
    fn shrinking_drops_oldest_scrollback_not_the_screen() {
        let mut s = Storage::new(5, 4);
        s.resize_rows(2, 4, &Cell::default());
        assert_eq!(ids(&s), vec![3, 4], "the visible bottom survives");
    }

    #[test]
    fn trimmed_len_ignores_trailing_blanks() {
        let mut row = Row::new(10, 0);
        row.cells_mut()[0].ch = 'h';
        row.cells_mut()[1].ch = 'i';
        assert_eq!(row.trimmed_len(), 2);
        assert_eq!(row.text(), "hi");
    }

    #[test]
    fn combining_marks_go_in_the_side_table() {
        let mut row = Row::new(4, 0);
        row.cells_mut()[0].ch = 'e';
        row.push_zerowidth(0, '\u{0301}');

        // Decomposed, not precomposed: the terminal preserves exactly what was
        // written. Comparing against a precomposed "é" literal would fail, and
        // normalizing here would be wrong -- the renderer needs the original.
        assert_eq!(row.text(), "e\u{0301}", "base char plus combining acute");
        assert_ne!(row.cells()[0].extra, NO_EXTRA);
        assert_eq!(row.cells()[1].extra, NO_EXTRA, "other cells stay clean");
    }

    #[test]
    fn wide_spacers_are_skipped_in_text() {
        let mut row = Row::new(4, 0);
        row.cells_mut()[0].ch = '世';
        row.cells_mut()[0].flags = CellFlags::WIDE;
        row.cells_mut()[1].flags = CellFlags::WIDE_SPACER;
        row.cells_mut()[2].ch = 'x';
        assert_eq!(row.text(), "世x", "the spacer contributes no character");
    }
}

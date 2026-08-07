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
    /// True if this row wrapped into the next rather than ending.
    pub wrapped: bool,
}

impl Row {
    #[must_use]
    pub fn new(cols: usize, id: LineId) -> Self {
        Self {
            cells: alloc::vec![Cell::default(); cols],
            extras: Vec::new(),
            id,
            wrapped: false,
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

    /// Blank the row, keeping its allocation. This is what makes steady-state
    /// scrolling allocation-free.
    pub fn reset(&mut self, template: &Cell, id: LineId) {
        let blank = Cell::blank_with(template);
        self.cells.fill(blank);
        self.extras.clear();
        self.id = id;
        self.wrapped = false;
    }

    /// Grow or shrink to `cols`.
    ///
    /// M1 does not reflow — growing pads with blanks and shrinking truncates.
    /// `wrapped` is preserved so reflow can be added later without a data
    /// migration.
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
            row.wrapped = *wrapped;
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

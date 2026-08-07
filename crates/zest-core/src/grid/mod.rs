//! The character grid: viewport, scrollback, cursor, and scroll regions.

pub mod storage;

use alloc::vec::Vec;

use crate::cell::{Cell, CellFlags};
pub use storage::{CellExtra, LineId, Row, Storage};

/// A position in the grid, in viewport coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cursor {
    pub row: usize,
    pub col: usize,
    /// Pending wrap: the cursor is logically past the last column, but the
    /// wrap happens only when the *next* character arrives.
    ///
    /// This deferral is required by the spec and is what makes writing exactly
    /// `cols` characters not scroll the screen. Terminals that wrap eagerly
    /// produce a spurious blank line on every full-width row.
    pub pending_wrap: bool,
}

/// Saved cursor state for DECSC/DECRC.
#[derive(Debug, Clone, Copy, Default)]
pub struct SavedCursor {
    pub cursor: Cursor,
    pub template: Cell,
    pub origin_mode: bool,
}

/// The scrolling region set by DECSTBM, as inclusive viewport rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScrollRegion {
    pub top: usize,
    pub bottom: usize,
}

impl ScrollRegion {
    #[must_use]
    pub fn full(rows: usize) -> Self {
        Self { top: 0, bottom: rows.saturating_sub(1) }
    }

    #[must_use]
    pub fn contains(&self, row: usize) -> bool {
        row >= self.top && row <= self.bottom
    }
}

/// The grid: a scrollback ring plus a viewport onto it.
#[derive(Debug)]
pub struct Grid {
    storage: Storage,
    cols: usize,
    rows: usize,
    /// Maximum retained scrollback lines (not counting the viewport).
    scrollback_limit: usize,
    /// How many scrollback lines currently exist above the viewport.
    scrollback_len: usize,
    /// Viewport offset from the bottom, in lines. 0 means "at the bottom".
    display_offset: usize,
    pub cursor: Cursor,
    pub region: ScrollRegion,
}

impl Grid {
    #[must_use]
    pub fn new(cols: usize, rows: usize, scrollback_limit: usize) -> Self {
        let cols = cols.max(1);
        let rows = rows.max(1);
        Self {
            storage: Storage::new(rows, cols),
            cols,
            rows,
            scrollback_limit,
            scrollback_len: 0,
            display_offset: 0,
            cursor: Cursor::default(),
            region: ScrollRegion::full(rows),
        }
    }

    #[must_use]
    pub fn cols(&self) -> usize {
        self.cols
    }

    #[must_use]
    pub fn rows(&self) -> usize {
        self.rows
    }

    #[must_use]
    pub fn scrollback_len(&self) -> usize {
        self.scrollback_len
    }

    #[must_use]
    pub fn display_offset(&self) -> usize {
        self.display_offset
    }

    /// Total lines held, scrollback plus viewport.
    #[must_use]
    pub fn total_lines(&self) -> usize {
        self.storage.len()
    }

    /// Absolute id of the next line to be created. Used by command blocks and
    /// the remote protocol to name ranges.
    #[must_use]
    pub fn next_line_id(&self) -> LineId {
        self.storage.next_id()
    }

    /// Adopt a line-id counter decided by a remote host.
    ///
    /// See [`crate::RemoteWriter::sync_next_line_id`]; this is not for a grid
    /// that owns its own output.
    pub fn set_next_line_id(&mut self, id: LineId) {
        self.storage.set_next_id(id);
    }

    /// Prepend history fetched from a host, oldest first.
    ///
    /// Grows scrollback up to the configured limit and drops the excess from
    /// the *oldest* end — a client that asked for more history than it is
    /// willing to keep should end up holding the newest of it.
    pub fn push_history(&mut self, rows: &[(LineId, Vec<Cell>, bool)]) {
        if rows.is_empty() {
            return;
        }
        let room = self.scrollback_limit.saturating_sub(self.scrollback_len);
        if room == 0 {
            return;
        }
        let take = rows.len().min(room);
        let rows = &rows[rows.len() - take..];
        self.storage.prepend(rows, self.cols);
        self.scrollback_len += take;
    }

    /// Index into storage of the first visible row, honoring scroll position.
    #[inline]
    fn viewport_base(&self) -> usize {
        self.storage
            .len()
            .saturating_sub(self.rows + self.display_offset)
    }

    /// A visible row, 0 being the top of the viewport.
    #[must_use]
    pub fn row(&self, row: usize) -> &Row {
        self.storage.row(self.viewport_base() + row)
    }

    pub fn row_mut(&mut self, row: usize) -> &mut Row {
        let base = self.viewport_base();
        self.storage.row_mut(base + row)
    }

    /// A row by absolute index across scrollback plus viewport.
    #[must_use]
    pub fn line(&self, index: usize) -> Option<&Row> {
        (index < self.storage.len()).then(|| self.storage.row(index))
    }

    #[must_use]
    pub fn cell(&self, row: usize, col: usize) -> Option<&Cell> {
        self.row(row).get(col)
    }

    pub fn cell_mut(&mut self, row: usize, col: usize) -> Option<&mut Cell> {
        self.row_mut(row).get_mut(col)
    }

    // --- scrolling -------------------------------------------------------

    /// Scroll the region up by `n`, pushing lines into scrollback when the
    /// region is the whole screen.
    ///
    /// Only a full-screen region contributes to scrollback: a program that sets
    /// a scroll region is managing its own display area, and `tail -f` inside a
    /// `less` pane must not pollute the user's history.
    pub fn scroll_up(&mut self, n: usize, template: &Cell) {
        if n == 0 {
            return;
        }
        let region_height = self.region.bottom - self.region.top + 1;
        let n = n.min(region_height);

        let full_screen = self.region.top == 0 && self.region.bottom == self.rows - 1;
        if full_screen {
            self.scroll_screen_up(n, template);
        } else {
            self.scroll_region_up(n, template);
        }
    }

    /// The common case: rotate the ring and recycle the exposed rows. O(1) per
    /// line and allocation-free.
    fn scroll_screen_up(&mut self, n: usize, template: &Cell) {
        for _ in 0..n {
            if self.scrollback_len < self.scrollback_limit {
                // Growing into scrollback: add a row rather than recycling, so
                // history is retained.
                let len = self.storage.len();
                self.storage.resize_rows(len + 1, self.cols, template);
                self.scrollback_len += 1;
            } else {
                // At the limit: rotate, recycling the oldest line as the newest.
                self.storage.rotate_up(1);
                let last = self.storage.len() - 1;
                self.storage.recycle(last, template);
            }

            // A reader scrolled back stays on the text they are reading.
            //
            // `viewport_base` is measured from the *end* of storage, so leaving
            // the offset alone would slide the view forward by one line for
            // every line the program emits -- text crawling upward while a build
            // runs, which makes scrollback useless exactly when it is wanted.
            // The offset is what holds the view still; snapping to the bottom on
            // output is a separate, opt-in policy.
            if self.display_offset > 0 {
                let max = self.storage.len().saturating_sub(self.rows);
                self.display_offset = (self.display_offset + 1).min(max);
            }
        }
    }

    /// A scroll region smaller than the screen cannot use ring rotation --
    /// rotating would move rows outside the region too. Move within the region.
    fn scroll_region_up(&mut self, n: usize, template: &Cell) {
        let base = self.viewport_base();
        let (top, bottom) = (self.region.top, self.region.bottom);

        for row in top..=bottom {
            let src = row + n;
            if src <= bottom {
                let src_cells = self.storage.row(base + src).cells().to_vec();
                let dst = self.storage.row_mut(base + row);
                dst.cells_mut().copy_from_slice(&src_cells);
            } else {
                let id = self.storage.row(base + row).id;
                self.storage.row_mut(base + row).reset(template, id);
            }
        }
    }

    /// Scroll the region down (reverse index), inserting blank lines at the top.
    pub fn scroll_down(&mut self, n: usize, template: &Cell) {
        if n == 0 {
            return;
        }
        let (top, bottom) = (self.region.top, self.region.bottom);
        let n = n.min(bottom - top + 1);
        let base = self.viewport_base();

        for row in (top..=bottom).rev() {
            if row >= top + n {
                let src_cells = self.storage.row(base + row - n).cells().to_vec();
                let dst = self.storage.row_mut(base + row);
                dst.cells_mut().copy_from_slice(&src_cells);
            } else {
                let id = self.storage.row(base + row).id;
                self.storage.row_mut(base + row).reset(template, id);
            }
        }
    }

    // --- viewport scrolling ----------------------------------------------

    /// Move the viewport through scrollback. Positive scrolls back in history.
    pub fn scroll_display(&mut self, delta: isize) {
        let max = self.scrollback_len;
        let new = self.display_offset as isize + delta;
        self.display_offset = new.clamp(0, max as isize) as usize;
    }

    pub fn scroll_to_bottom(&mut self) {
        self.display_offset = 0;
    }

    // --- resize ----------------------------------------------------------

    /// Resize the viewport.
    ///
    /// **M1 does not reflow.** Growing pads with blanks, shrinking truncates,
    /// and scrollback is preserved as-is. Reflow is a multi-week problem that
    /// Alacritty took years to stabilize, and attempting it here would eat the
    /// milestone. The `wrapped` flag is recorded so it can be added later
    /// without a data migration.
    pub fn resize(&mut self, cols: usize, rows: usize, template: &Cell) {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return;
        }

        if cols != self.cols {
            self.storage.resize_cols(cols, template);
            self.cols = cols;
        }

        if rows != self.rows {
            // Shrinking gives up the blank rows *below the cursor* first.
            //
            // Taking everything off the top instead is what a viewport does
            // when the cursor is already at the bottom, and it is catastrophic
            // in the far more common case where it is not: a screen with five
            // lines of output, a prompt on row 5, and nine blank rows beneath
            // it loses the output *and the prompt*, keeping nine blank rows
            // that held nothing. Every resize of an idle terminal did this.
            //
            // xterm has always worked this way, and this is why.
            if rows < self.rows {
                let below = self.rows - 1 - self.cursor.row.min(self.rows - 1);
                let from_bottom = (self.rows - rows).min(below);
                self.storage.truncate_bottom(from_bottom);
            }

            // Whatever could not be found below the cursor comes off the top,
            // where it becomes scrollback rather than being lost. The cursor
            // clamp below lands on exactly the right row when that happens:
            // reaching here with rows still to drop means the cursor was at or
            // past the new height, and `new_rows - 1` is precisely where it
            // ends up after the content above it scrolls away.
            let target = self.scrollback_len + rows;
            self.storage.resize_rows(target, cols, template);
            self.rows = rows;
        }

        self.region = ScrollRegion::full(self.rows);
        self.cursor.row = self.cursor.row.min(self.rows - 1);
        self.cursor.col = self.cursor.col.min(self.cols - 1);
        self.cursor.pending_wrap = false;
        self.display_offset = self.display_offset.min(self.scrollback_len);
    }

    // --- editing ---------------------------------------------------------

    /// Erase a span of the current row, inclusive of both ends.
    pub fn erase_in_row(&mut self, row: usize, from: usize, to: usize, template: &Cell) {
        let cols = self.cols;
        let blank = Cell::blank_with(template);
        let r = self.row_mut(row);
        for col in from..=to.min(cols.saturating_sub(1)) {
            if let Some(c) = r.get_mut(col) {
                *c = blank;
            }
        }
    }

    pub fn erase_rows(&mut self, from: usize, to: usize, template: &Cell) {
        for row in from..=to.min(self.rows.saturating_sub(1)) {
            let cols = self.cols;
            self.erase_in_row(row, 0, cols.saturating_sub(1), template);
        }
    }

    /// Insert `n` blank cells at the cursor, shifting the rest of the row right.
    pub fn insert_cells(&mut self, n: usize, template: &Cell) {
        let (row, col, cols) = (self.cursor.row, self.cursor.col, self.cols);
        let blank = Cell::blank_with(template);
        let r = self.row_mut(row);
        let cells = r.cells_mut();
        let n = n.min(cols - col);
        cells[col..].rotate_right(n);
        for c in &mut cells[col..col + n] {
            *c = blank;
        }
    }

    /// Delete `n` cells at the cursor, shifting the rest of the row left.
    pub fn delete_cells(&mut self, n: usize, template: &Cell) {
        let (row, col, cols) = (self.cursor.row, self.cursor.col, self.cols);
        let blank = Cell::blank_with(template);
        let r = self.row_mut(row);
        let cells = r.cells_mut();
        let n = n.min(cols - col);
        cells[col..].rotate_left(n);
        for c in &mut cells[cols - n..] {
            *c = blank;
        }
    }

    /// Insert `n` blank lines at the cursor row, within the scroll region.
    pub fn insert_lines(&mut self, n: usize, template: &Cell) {
        if !self.region.contains(self.cursor.row) {
            return;
        }
        let saved = self.region;
        self.region.top = self.cursor.row;
        self.scroll_down(n, template);
        self.region = saved;
    }

    /// Delete `n` lines at the cursor row, within the scroll region.
    pub fn delete_lines(&mut self, n: usize, template: &Cell) {
        if !self.region.contains(self.cursor.row) {
            return;
        }
        let saved = self.region;
        self.region.top = self.cursor.row;
        self.scroll_up(n, template);
        self.region = saved;
    }

    /// Whole-grid reset, used when switching screen buffers.
    pub fn clear_all(&mut self, template: &Cell) {
        for row in 0..self.rows {
            let cols = self.cols;
            self.erase_in_row(row, 0, cols - 1, template);
        }
        self.cursor = Cursor::default();
        self.region = ScrollRegion::full(self.rows);
    }

    /// The viewport as plain text, one line per row, trailing blanks trimmed.
    #[must_use]
    pub fn screen_text(&self) -> alloc::string::String {
        let mut out = alloc::string::String::new();
        for row in 0..self.rows {
            out.push_str(&self.row(row).text());
            if row + 1 < self.rows {
                out.push('\n');
            }
        }
        out
    }

    /// Mark the current row as wrapped. Called when output runs past the last
    /// column, so copy and future reflow can tell a wrap from a newline.
    pub fn set_wrapped(&mut self, row: usize, wrapped: bool) {
        let cols = self.cols;
        let r = self.row_mut(row);
        r.wrapped = wrapped;
        if let Some(last) = r.get_mut(cols - 1) {
            last.flags.set(CellFlags::WRAPLINE, wrapped);
        }
    }

    /// Lines from the top of scrollback, for the remote protocol's paged fetch.
    #[must_use]
    pub fn lines_by_id(&self, from: LineId, count: usize) -> Vec<&Row> {
        self.storage
            .iter()
            .filter(|r| r.id >= from)
            .take(count)
            .collect()
    }
}

#[cfg(test)]
mod tests {

    /// The case that is true almost all the time: output at the top, the
    /// cursor just after it, blank rows below.
    #[test]
    fn shrinking_gives_up_the_blank_rows_below_the_cursor_first() {
        // Taking rows off the top instead loses the output *and* the prompt,
        // keeping blank rows that held nothing -- which is what every resize
        // of an idle terminal used to do.
        let mut g = Grid::new(40, 15, 100);
        for row in 0..6 {
            write_text(&mut g, row, &format!("line {row}"));
        }
        g.cursor.row = 5;

        g.resize(40, 8, &Cell::default());

        assert_eq!(g.rows(), 8);
        assert_eq!(g.row(5).text().trim_end(), "line 5", "the cursor's own row was dropped");
        assert_eq!(g.row(0).text().trim_end(), "line 0", "content above the cursor was dropped");
        assert_eq!(g.cursor.row, 5, "the cursor moved even though it did not have to");
        assert_eq!(g.scrollback_len(), 0, "nothing needed to become scrollback");
    }

    #[test]
    fn shrinking_past_the_cursor_scrolls_content_into_scrollback() {
        // When there is not enough blank space below the cursor, the rest has
        // to come off the top -- and the cursor must end up on the last row
        // rather than pointing outside the grid.
        let mut g = Grid::new(40, 10, 100);
        for row in 0..10 {
            write_text(&mut g, row, &format!("row {row}"));
        }
        g.cursor.row = 9;

        g.resize(40, 4, &Cell::default());

        assert_eq!(g.rows(), 4);
        assert_eq!(g.row(3).text().trim_end(), "row 9", "the cursor's row must stay visible");
        assert_eq!(g.cursor.row, 3, "the cursor must land on the last row");
    }

    #[test]
    fn a_shrink_and_a_grow_leave_the_cursor_line_where_it_was() {
        // Dragging a window smaller and back is one gesture, and losing the
        // prompt half way through it is what makes a terminal feel broken.
        let mut g = Grid::new(40, 20, 100);
        write_text(&mut g, 0, "important");
        g.cursor.row = 1;

        g.resize(40, 5, &Cell::default());
        g.resize(40, 20, &Cell::default());

        assert_eq!(g.row(0).text().trim_end(), "important", "content did not survive the round trip");
    }

    fn write_text(g: &mut Grid, row: usize, text: &str) {
        for (col, ch) in text.chars().enumerate() {
            if let Some(cell) = g.cell_mut(row, col) {
                cell.ch = ch;
            }
        }
    }
    use super::*;

    fn write(grid: &mut Grid, row: usize, text: &str) {
        for (i, ch) in text.chars().enumerate() {
            if let Some(c) = grid.cell_mut(row, i) {
                c.ch = ch;
            }
        }
    }

    #[test]
    fn scrolling_pushes_lines_into_scrollback() {
        let mut g = Grid::new(10, 3, 100);
        write(&mut g, 0, "one");
        write(&mut g, 1, "two");
        write(&mut g, 2, "three");

        g.scroll_up(1, &Cell::default());
        assert_eq!(g.scrollback_len(), 1);
        assert_eq!(g.row(0).text(), "two", "the screen moved up");
        assert_eq!(g.row(1).text(), "three");
        assert_eq!(g.row(2).text(), "", "a fresh blank line appeared");
    }

    #[test]
    fn scrollback_is_reachable_by_scrolling_the_display() {
        let mut g = Grid::new(10, 2, 100);
        write(&mut g, 0, "first");
        g.scroll_up(1, &Cell::default());
        write(&mut g, 1, "latest");

        assert_eq!(g.row(0).text(), "");
        g.scroll_display(1);
        assert_eq!(g.row(0).text(), "first", "scrolled back to the first line");
        g.scroll_to_bottom();
        assert_eq!(g.row(1).text(), "latest");
    }

    #[test]
    fn output_does_not_drag_the_view_out_from_under_a_reader() {
        // The failure this pins down is subtle to describe and obvious to see:
        // scroll back to read something, and every line a running build emits
        // slides your text up by one.
        let mut g = Grid::new(20, 2, 100);
        write(&mut g, 0, "anchor");
        for _ in 0..5 {
            g.scroll_up(1, &Cell::default());
        }
        g.scroll_display(5);
        assert_eq!(g.row(0).text(), "anchor");

        for _ in 0..20 {
            g.scroll_up(1, &Cell::default());
        }
        assert_eq!(g.row(0).text(), "anchor", "output moved the view");

        // And the bottom is still reachable afterwards.
        g.scroll_to_bottom();
        assert_eq!(g.display_offset(), 0);
    }

    #[test]
    fn a_view_falling_off_the_end_of_scrollback_pins_to_the_oldest_line() {
        // Once history is evicted there is nothing to hold the view on, so it
        // must clamp rather than run past the top of storage.
        let mut g = Grid::new(20, 2, 3);
        write(&mut g, 0, "doomed");
        for _ in 0..2 {
            g.scroll_up(1, &Cell::default());
        }
        g.scroll_display(2);
        assert_eq!(g.row(0).text(), "doomed");

        for _ in 0..50 {
            g.scroll_up(1, &Cell::default());
        }
        assert_eq!(g.display_offset(), g.scrollback_len(), "clamped to the oldest line held");
    }

    #[test]
    fn scrollback_stops_growing_at_the_limit() {
        let mut g = Grid::new(4, 2, 3);
        for _ in 0..10 {
            g.scroll_up(1, &Cell::default());
        }
        assert_eq!(g.scrollback_len(), 3, "bounded by the configured limit");
        assert_eq!(g.total_lines(), 5, "limit plus viewport");
    }

    #[test]
    fn line_ids_keep_counting_past_eviction() {
        let mut g = Grid::new(4, 2, 2);
        let first = g.row(0).id;
        for _ in 0..8 {
            g.scroll_up(1, &Cell::default());
        }
        assert!(g.next_line_id() > first + 8, "ids are monotonic across eviction");
    }

    #[test]
    fn a_scroll_region_does_not_touch_scrollback() {
        let mut g = Grid::new(10, 4, 100);
        write(&mut g, 0, "keep");
        g.region = ScrollRegion { top: 1, bottom: 2 };
        g.scroll_up(1, &Cell::default());

        assert_eq!(g.scrollback_len(), 0, "a program managing its own region owns no history");
        assert_eq!(g.row(0).text(), "keep", "rows outside the region are untouched");
    }

    #[test]
    fn scroll_region_moves_only_its_own_rows() {
        let mut g = Grid::new(10, 4, 100);
        write(&mut g, 0, "a");
        write(&mut g, 1, "b");
        write(&mut g, 2, "c");
        write(&mut g, 3, "d");

        g.region = ScrollRegion { top: 1, bottom: 2 };
        g.scroll_up(1, &Cell::default());

        assert_eq!(g.row(0).text(), "a");
        assert_eq!(g.row(1).text(), "c", "b scrolled off the top of the region");
        assert_eq!(g.row(2).text(), "");
        assert_eq!(g.row(3).text(), "d");
    }

    #[test]
    fn reverse_scroll_inserts_at_the_top() {
        let mut g = Grid::new(10, 3, 100);
        write(&mut g, 0, "a");
        write(&mut g, 1, "b");
        g.scroll_down(1, &Cell::default());
        assert_eq!(g.row(0).text(), "");
        assert_eq!(g.row(1).text(), "a");
        assert_eq!(g.row(2).text(), "b");
    }

    #[test]
    fn insert_and_delete_cells_shift_the_row() {
        let mut g = Grid::new(8, 1, 0);
        write(&mut g, 0, "abcdef");

        g.cursor.col = 2;
        g.insert_cells(2, &Cell::default());
        assert_eq!(g.row(0).text(), "ab  cdef");

        g.delete_cells(2, &Cell::default());
        assert_eq!(g.row(0).text(), "abcdef");
    }

    #[test]
    fn erase_paints_the_current_background() {
        let mut g = Grid::new(4, 1, 0);
        write(&mut g, 0, "abcd");
        let template = Cell { bg: crate::Color::Indexed(2), ..Default::default() };

        g.erase_in_row(0, 1, 2, &template);
        assert_eq!(g.cell(0, 1).unwrap().bg, crate::Color::Indexed(2));
        assert_eq!(g.cell(0, 0).unwrap().bg, crate::Color::Default, "outside the span is untouched");
    }

    #[test]
    fn resize_clamps_the_cursor_and_resets_the_region() {
        let mut g = Grid::new(20, 10, 50);
        g.cursor = Cursor { row: 9, col: 19, pending_wrap: true };
        g.region = ScrollRegion { top: 2, bottom: 8 };

        g.resize(10, 5, &Cell::default());

        assert_eq!(g.cols(), 10);
        assert_eq!(g.rows(), 5);
        assert_eq!(g.cursor.row, 4, "cursor clamped into the new viewport");
        assert_eq!(g.cursor.col, 9);
        assert!(!g.cursor.pending_wrap, "a pending wrap cannot survive a resize");
        assert_eq!(g.region, ScrollRegion::full(5), "DECSTBM is reset by a resize");
    }

    #[test]
    fn wrapline_is_recorded_on_the_last_cell() {
        let mut g = Grid::new(4, 2, 0);
        g.set_wrapped(0, true);
        assert!(g.row(0).wrapped);
        assert!(g.cell(0, 3).unwrap().flags.contains(CellFlags::WRAPLINE));
    }
}

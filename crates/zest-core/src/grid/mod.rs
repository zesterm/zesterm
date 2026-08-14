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

/// Where each line went when a width change renumbered them.
///
/// Returned by [`Grid::resize`], and empty for a height-only change — heights
/// do not rewrap, so nothing is renumbered. See [`Grid::reflow`] for why no
/// one-to-one mapping exists: every old row of a logical line maps to the
/// *first* new row of that line, because a line that was three rows and is now
/// one has nowhere else for the other two to point.
///
/// A lookup that returns `None` means the line was dropped by the scrollback
/// bound during the rewrap — which is eviction, and callers should treat it as
/// such rather than clamping to a neighbour.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Reindex {
    /// `(old, new)`, ascending by `old` — the order `reflow` walks storage in.
    map: Vec<(LineId, LineId)>,
}

impl Reindex {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// Build a mapping directly, for tests that need one without a rewrap.
    ///
    /// Not public: a real one only ever comes out of [`Grid::reflow`], and a
    /// caller able to invent one could hand [`crate::BlockIndex::reanchor`] a
    /// mapping no grid would ever produce.
    #[cfg(test)]
    pub(crate) fn from_pairs(pairs: &[(LineId, LineId)]) -> Self {
        Self { map: pairs.to_vec() }
    }

    /// The oldest id that still exists after the rewrap.
    #[must_use]
    pub fn oldest(&self) -> Option<LineId> {
        self.map.first().map(|&(_, new)| new)
    }

    /// Where a line went, or `None` if it did not survive.
    #[must_use]
    pub fn lookup(&self, old: LineId) -> Option<LineId> {
        self.map
            .binary_search_by_key(&old, |&(o, _)| o)
            .ok()
            .map(|i| self.map[i].1)
    }
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
    /// Rows that fell off the top, kept only if someone asked for them.
    ///
    /// **Off by default, and that is not a default worth changing.** Capturing
    /// means building a `String` per evicted line on the scroll path, which is
    /// otherwise allocation-free by construction — `rotate_up` recycles the
    /// oldest row rather than dropping and pushing one, which is what makes a
    /// 100MB `cat` cost nothing per line. A daemon that wants durable history
    /// opts in and pays for it; a window that does not, does not.
    ///
    /// (That property is structural rather than test-asserted. I wrote the
    /// opposite here first and checked: there is no allocation-counting test in
    /// this crate, only the recycling that makes one unnecessary.)
    evicted: Vec<Evicted>,
    capture_evicted: bool,
    /// How many scrollback lines currently exist above the viewport.
    scrollback_len: usize,
    /// Viewport offset from the bottom, in lines. 0 means "at the bottom".
    display_offset: usize,
    /// Whether something other than this grid has the last word on the viewport
    /// after a resize.
    ///
    /// Two things do, and they are the same argument one layer apart. ConPTY
    /// restates the whole viewport when it is resized, where a unix pty sends
    /// nothing back. And a *replica* — a grid deltas are applied into rather
    /// than parsed into — is about to be handed a keyframe that restates every
    /// visible row. Either way a grow must not pull history down into rows that
    /// are about to be overwritten; see the grow branch of [`Grid::resize`].
    ///
    /// A plain bool rather than a `cfg` because this crate must build for wasm
    /// and knows nothing about platforms. The host asks its transport and passes
    /// the answer on; a replica sets it by being resized through
    /// `Terminal::remote`.
    viewport_restated_elsewhere: bool,
    /// Rows a restating shrink pushed over the top that a matching grow owes
    /// back once the repaint has had its say. See [`Grid::settle_restate`].
    ///
    /// Owed only while those rows are still the ones immediately above the
    /// viewport, so anything that moves the content on cancels it — which is
    /// what stops `clear` followed by a grow from resurrecting the history the
    /// user just asked to be rid of.
    restate_debt: usize,
    /// What the grow asked for, waiting on the repaint that answers it.
    pending_restate: usize,
    /// Whether that repaint has been seen to begin (`CSI 8 ; rows ; cols t`).
    restating: bool,
    pub cursor: Cursor,
    pub region: ScrollRegion,
}

/// A row on its way out of the ring.
///
/// Text rather than cells, deliberately: what survives a restart is what was
/// printed, and keeping 16-byte cells for history that may never be read again
/// trades a lot of disk for attributes nothing currently renders from storage.
/// When history does need colour, this grows a field rather than changing shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evicted {
    pub id: LineId,
    pub text: alloc::string::String,
    pub wrapped: bool,
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
            evicted: Vec::new(),
            capture_evicted: false,
            scrollback_len: 0,
            display_offset: 0,
            viewport_restated_elsewhere: false,
            restate_debt: 0,
            pending_restate: 0,
            restating: false,
            cursor: Cursor::default(),
            region: ScrollRegion::full(rows),
        }
    }

    /// Tell the grid that something else has the last word on its viewport.
    ///
    /// See the field, and the grow branch of [`Self::resize`]. Off by default,
    /// which is the unix answer and the answer for a grid that is nobody's
    /// replica — a local session on a pty that says nothing back, or a test.
    pub fn set_viewport_restated_elsewhere(&mut self, yes: bool) {
        self.viewport_restated_elsewhere = yes;
    }

    #[must_use]
    pub fn cols(&self) -> usize {
        self.cols
    }

    #[must_use]
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Start keeping rows that fall off the top. See the `evicted` field.
    pub fn capture_evicted(&mut self, on: bool) {
        self.capture_evicted = on;
        if !on {
            self.evicted = Vec::new();
        }
    }

    /// Take what has been evicted since the last call, oldest first.
    ///
    /// Drained rather than borrowed: the caller is writing them somewhere
    /// durable, and a buffer that only grows is a leak with a nicer name.
    pub fn take_evicted(&mut self) -> Vec<Evicted> {
        core::mem::take(&mut self.evicted)
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
    ///
    /// **Display space.** This is where the *reader* is looking, which is not
    /// where the program is printing whenever they have scrolled back. Only
    /// rendering, hit-testing and selection may use it — see
    /// [`Self::active_base`], and the rule stated there.
    #[inline]
    fn viewport_base(&self) -> usize {
        self.storage
            .len()
            .saturating_sub(self.rows + self.display_offset)
    }

    /// Index into storage of the first row of the *live* screen.
    ///
    /// **Active space**, and the default: scrolling back must not move the rows
    /// a program prints onto. `scroll_screen_up` deliberately advances
    /// `display_offset` in step with storage so a reader's view holds still,
    /// which also freezes `viewport_base` — so every mutation that resolved
    /// through it landed on the rows being read, while the fresh rows at the
    /// tail stayed blank. Scroll up during a build and it overwrote your
    /// scrollback; scroll back down and the output was gone.
    ///
    /// The rule that keeps this fixed, by what the caller is *for* rather than
    /// by which file it lives in: **the VT parser, the wire encoder and the
    /// retention horizon are active space**. They speak for the session, and
    /// the session does not know anyone scrolled.
    ///
    /// Pointing is the other half and is display space on purpose — selection,
    /// hit-testing and `Terminal::abs_pos` translate where the *reader* clicked,
    /// which is exactly the row they are looking at. Both kinds live in
    /// `term.rs`, so the split is not a per-file rule.
    ///
    /// Every mutating accessor here resolves through this one regardless, so a
    /// write cannot reach scrollback even if someone forgets.
    #[inline]
    fn active_base(&self) -> usize {
        self.storage.len().saturating_sub(self.rows)
    }

    /// A visible row, 0 being the top of the viewport. Display space.
    #[must_use]
    pub fn row(&self, row: usize) -> &Row {
        self.storage.row(self.viewport_base() + row)
    }

    /// A row of the live screen, 0 being its top. Active space.
    #[must_use]
    pub fn active_row(&self, row: usize) -> &Row {
        self.storage.row(self.active_base() + row)
    }

    pub fn row_mut(&mut self, row: usize) -> &mut Row {
        let base = self.active_base();
        self.storage.row_mut(base + row)
    }

    /// The absolute storage index of a viewport row — the inverse of what
    /// [`Self::line`] consumes. Public for the fold row-map: a compacted
    /// view names rows absolutely, and the cursor's viewport row must be
    /// findable inside it. Display space.
    #[must_use]
    pub fn abs_index(&self, row: usize) -> usize {
        self.viewport_base() + row
    }

    /// [`Self::abs_index`] for the live screen. Active space.
    #[must_use]
    pub fn active_abs_index(&self, row: usize) -> usize {
        self.active_base() + row
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

        // The content has moved on, so the rows a shrink displaced are no longer
        // the ones above the viewport and are not owed back. See the field.
        self.cancel_restate_debt();

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
                // The oldest line is about to be recycled into the newest, so
                // this is the last moment its content exists anywhere.
                if self.capture_evicted {
                    if let Some(row) = self.storage.iter().next() {
                        let text: alloc::string::String =
                            row.cells().iter().map(|c| c.ch).collect();
                        self.evicted.push(Evicted {
                            id: row.id,
                            // Trailing blanks are padding, not content: storing
                            // them would triple a history of short lines.
                            text: text.trim_end().into(),
                            wrapped: row.wrapped,
                        });
                    }
                }
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
        let base = self.active_base();
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
        let base = self.active_base();

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
    /// A width change rewraps ([`Grid::reflow`]) and therefore renumbers line
    /// ids, so this returns a [`Reindex`] saying where each one went. Anything
    /// anchored to an absolute id — command blocks — must be re-anchored
    /// through it or it names different text afterwards.
    pub fn resize(&mut self, cols: usize, rows: usize, template: &Cell) -> Reindex {
        let cols = cols.max(1);
        let rows = rows.max(1);
        if cols == self.cols && rows == self.rows {
            return Reindex::default();
        }

        let mut reindex = Reindex::default();
        if cols != self.cols {
            if self.scrollback_limit == 0 {
                // The alternate screen. **Never reflow it.** A full-screen
                // program repaints on SIGWINCH and its frame is a picture, not
                // a paragraph: rewrapping it would mangle the box drawing that
                // is about to be redrawn anyway. `scrollback_limit == 0` is
                // exactly how the alt grid is built.
                self.storage.resize_cols(cols, template);
            } else {
                reindex = self.reflow(cols, template);
            }
            self.cols = cols;
        }

        if rows != self.rows {
            if rows < self.rows {
                // Shrinking gives up the blank rows *below the cursor* first.
                //
                // Taking everything off the top instead is what a viewport does
                // when the cursor is already at the bottom, and it is wrong in
                // the far more common case where it is not: a screen with five
                // lines of output, a prompt on row 5, and nine blank rows
                // beneath it loses the output *and the prompt*, keeping nine
                // blank rows that held nothing.
                //
                // xterm has always worked this way, and this is why.
                let below = self.rows - 1 - self.cursor.row.min(self.rows - 1);
                let from_bottom = (self.rows - rows).min(below);
                self.storage.truncate_bottom(from_bottom);

                // Whatever could not be found below the cursor leaves over the
                // *top*, and those rows are history: they must become
                // scrollback, not disappear.
                //
                // Growing `scrollback_len` is the whole of it. Without it the
                // drain below removes them from storage outright -- ten lines
                // of output shrunk to four rows left four lines in existence
                // and six deleted, unreachable by scrolling because as far as
                // the grid was concerned they had never been history. That is
                // what "the lines pushed up and were lost" was.
                let over_the_top = (self.rows - rows) - from_bottom;
                if over_the_top > 0 {
                    self.scrollback_len =
                        (self.scrollback_len + over_the_top).min(self.scrollback_limit);
                    // The cursor moved up with the content.
                    self.cursor.row = self.cursor.row.saturating_sub(over_the_top);
                    // A restating pty gets these rows back on the way out, once
                    // its repaint has stopped blanking things. See the grow
                    // branch below and `settle_restate`.
                    if self.viewport_restated_elsewhere {
                        self.restate_debt += over_the_top;
                    }
                }
            } else if self.viewport_restated_elsewhere {
                // Nothing comes back out of scrollback *yet*, because the pty is
                // about to restate this viewport and would blank whatever we
                // put in it.
                //
                // ConPTY's buffer is only as tall as the viewport, so shrinking
                // to a few rows discards everything else it held; growing back,
                // it repaints the rows it still has and *blanks the rest*. Pull
                // history down to meet that and the repaint erases it -- and it
                // is no longer in scrollback either, because this moved the
                // boundary past it. That is not history misplaced, it is
                // history destroyed, and it is what "I dragged the window's
                // height to nothing and back and every block is empty" was:
                // the blocks were intact the whole time and every row they
                // named had been blanked. (#200)
                //
                // Leaving the boundary alone gives the repaint fresh rows to
                // blank and keeps the history above it, which is also exactly
                // what ConPTY and Windows Terminal do with their own buffers.
                //
                // What this used to conclude — that the reversible drag below is
                // a *unix* property, unreachable here because the repaint always
                // has the last word — is half right. The repaint does have the
                // last word, so the pull cannot happen *before* it. It can
                // happen after: by then the tail of the viewport is blank rows
                // the repaint itself wrote, and history dropped into them lands
                // where nothing will overwrite it. Note what is owed and pay it
                // in `settle_restate`, when the repaint closes. (#247)
                //
                // Accumulated, never assigned: a drag is a stream of resizes,
                // so several grows can land before any repaint closes. Each one
                // moves its share of the debt into what is pending, and
                // overwriting instead would drop every share but the last --
                // shrink 10 -> 4 then grow 4 -> 6 -> 10 owes 6 rows and would
                // give back 4, so the gesture comes out *nearly* reversible,
                // which is the hardest kind of wrong to notice.
                let owed = (rows - self.rows).min(self.restate_debt);
                self.pending_restate += owed;
                self.restate_debt -= owed;
            } else {
                // Growing pulls rows back down out of scrollback before it
                // adds blank ones, so dragging a window smaller and back is one
                // reversible gesture rather than a way to lose the screen. The
                // rows are already in storage -- only the boundary moves.
                let from_scrollback = (rows - self.rows).min(self.scrollback_len);
                self.scrollback_len -= from_scrollback;
                self.cursor.row += from_scrollback;
            }

            // Everything beyond what scrollback is allowed to keep. `target`
            // is now the honest total, so this trims real excess instead of
            // silently eating history.
            let target = self.scrollback_len + rows;
            self.storage.resize_rows(target, cols, template);
            self.rows = rows;
        }

        self.region = ScrollRegion::full(self.rows);
        self.cursor.row = self.cursor.row.min(self.rows - 1);
        self.cursor.col = self.cursor.col.min(self.cols - 1);
        self.cursor.pending_wrap = false;
        self.display_offset = self.display_offset.min(self.scrollback_len);
        reindex
    }

    // --- the restating pty's repaint -------------------------------------

    /// The pty announced that it is restating the viewport (`CSI 8 ; r ; c t`).
    ///
    /// Only ConPTY sends this, and only as a *notification* that the repaint
    /// documented on [`Grid::resize`]'s grow branch is starting — it is not the
    /// XTWINOPS request of the same name, and nothing here obeys it. It matters
    /// because it opens the window in which [`Self::settle_restate`] may run.
    pub fn note_restatement_began(&mut self) {
        if self.pending_restate > 0 {
            self.restating = true;
        }
    }

    /// Whether a restatement is in progress and has not yet been settled.
    #[must_use]
    pub fn restating(&self) -> bool {
        self.restating
    }

    /// Give back what the grow owed, now that the repaint has had its last word.
    ///
    /// Returns whether anything moved — the caller needs to know, because the
    /// viewport/scrollback boundary moving is the one change deltas cannot
    /// describe (`docs/CONTRACTS.md`), so it costs a keyframe.
    ///
    /// The repaint leaves the restated content at the top of the viewport and
    /// blank rows below it, because ConPTY grows its own buffer at the bottom
    /// and has nothing to put there. Dropping those blank rows and moving the
    /// boundary down by the same number slides the viewport *up* over storage:
    /// history returns to the screen, the prompt returns to the bottom row, and
    /// `storage.len()` is unchanged, so `rows` never moves.
    ///
    /// Every bound here is load-bearing. `pending_restate` is what the shrink
    /// actually took, so a grow never invents history; the blank tail is how
    /// much of the viewport the repaint had nothing for, so a full screen
    /// settles to nothing; and `scrollback_len` is what there is to give.
    pub fn settle_restate(&mut self) -> bool {
        self.restating = false;
        let owed = core::mem::take(&mut self.pending_restate);
        let below_cursor = self.rows - 1 - self.cursor.row.min(self.rows - 1);
        let k = owed.min(self.blank_tail()).min(below_cursor).min(self.scrollback_len);
        if k == 0 {
            return false;
        }

        self.storage.truncate_bottom(k);
        self.scrollback_len -= k;
        self.cursor.row += k;
        // The rows just dropped are the blanks `resize_rows` minted at grow
        // time, and `truncate_bottom` does not rewind the counter. Left alone
        // the gap makes `oldest_retained_line` — `active_row(0).id -
        // scrollback_len`, in `subscribe.rs` and `term.rs` — name a line the
        // grid no longer holds, so the host offers clients scrollback it cannot
        // answer for.
        let last = self.storage.len() - 1;
        self.storage.set_next_id(self.storage.row(last).id + 1);
        // Storage lost `k` rows off the end, so holding a scrolled-back reader
        // on the same text means giving back the same `k`.
        self.display_offset = self.display_offset.saturating_sub(k).min(self.scrollback_len);
        true
    }

    /// How many rows at the bottom of the viewport are blank.
    fn blank_tail(&self) -> usize {
        let base = self.active_base();
        (0..self.rows)
            .rev()
            .take_while(|r| self.storage.row(base + r).trimmed_len() == 0)
            .count()
    }

    /// Forget what a restating grow owed.
    ///
    /// Called wherever the content moves on, because the debt is only ever owed
    /// while the displaced rows are still the ones immediately above the
    /// viewport. After a scroll or a screen erase they are not, and paying it
    /// would drag unrelated history onto the screen.
    pub(crate) fn cancel_restate_debt(&mut self) {
        self.restate_debt = 0;
        self.pending_restate = 0;
        self.restating = false;
    }

    /// Rewrap every logical line to a new width.
    ///
    /// A *logical line* is a run of rows joined by `wrapped`: what the user
    /// typed or the program printed, before the screen decided where to break
    /// it. Reflow joins each one back together and re-breaks it.
    ///
    /// Without this, narrowing a window destroys the columns past the new width
    /// and widening cannot bring them back, because they are simply gone.
    ///
    /// # Line ids are renumbered
    ///
    /// They have to be: rewrapping changes how many rows a logical line
    /// occupies, so a one-to-one mapping does not exist. The *first* row of the
    /// oldest line keeps its id and the rest follow consecutively, so ids stay
    /// monotonic top to bottom — which is what scroll detection and
    /// `lines_by_id` actually depend on. Anything holding an id across a column
    /// change must re-anchor, and the returned [`Reindex`] is how: it maps every
    /// old id to the new id of the logical line it belonged to. The selection
    /// is *cleared* rather than re-anchored because a selection names a column
    /// as well as a line and rewrapping moves both; a command block names only
    /// the line it began on, which survives.
    ///
    /// # A wide character is never split
    ///
    /// If one would land in the last column with its spacer past the edge, the
    /// column is left blank and the character starts the next row. Splitting it
    /// would produce a spacer with nothing to be the second half of.
    fn reflow(&mut self, new_cols: usize, template: &Cell) -> Reindex {
        let old_rows = self.storage.take_all();
        if old_rows.is_empty() {
            return Reindex::default();
        }

        // Where the cursor is, as an offset into its logical line -- the only
        // description that survives rewrapping.
        let cursor_abs = self
            .storage_index_of_cursor(old_rows.len())
            .min(old_rows.len().saturating_sub(1));

        let first_id = old_rows[0].id;
        let mut next_id = first_id;
        let mut out: Vec<Row> = Vec::with_capacity(old_rows.len());
        let mut cursor_target: Option<(usize, usize)> = None;
        let mut reindex = Reindex { map: Vec::with_capacity(old_rows.len()) };

        let mut i = 0;
        while i < old_rows.len() {
            // Collect one logical line.
            let mut cells: Vec<(Cell, Option<CellExtra>)> = Vec::new();
            let mut cursor_offset: Option<usize> = None;
            // Every old row of this logical line re-anchors to the *first* new
            // row of it. That is the only answer that survives: a logical line
            // that was three rows and is now one has nowhere else for rows two
            // and three to point.
            let line_start_id = next_id;
            loop {
                let row = &old_rows[i];
                reindex.map.push((row.id, line_start_id));
                // A wrapped row is full by definition; only the last row of a
                // logical line has trailing blanks worth dropping.
                //
                // And one cell further if that blank is a wide character's
                // spacer: `trimmed_len` sees a space with a default background
                // and calls it padding, which would leave the character it
                // belongs to alone at the end of the line with nothing to be
                // its second half.
                let mut end = if row.wrapped { row.len() } else { row.trimmed_len() };
                if end < row.len()
                    && row.cells()[end].flags.contains(CellFlags::WIDE_SPACER)
                {
                    end += 1;
                }
                if i == cursor_abs {
                    cursor_offset = Some(cells.len() + self.cursor.col.min(row.len()));
                }
                for col in 0..end {
                    if let Some(detached) = row.detach(col) {
                        cells.push(detached);
                    }
                }
                if !row.wrapped || i + 1 >= old_rows.len() {
                    break;
                }
                i += 1;
            }
            i += 1;

            // Re-break it.
            let mut col = 0;
            let mut row = Row::new(new_cols, next_id);
            for (index, (cell, extra)) in cells.iter().enumerate() {
                // Never split a wide character across the edge.
                if cell.flags.contains(CellFlags::WIDE) && col + 1 >= new_cols && new_cols > 1 {
                    row.wrapped = true;
                    out.push(core::mem::replace(&mut row, Row::new(new_cols, next_id + 1)));
                    next_id += 1;
                    col = 0;
                }
                if col >= new_cols {
                    row.wrapped = true;
                    out.push(core::mem::replace(&mut row, Row::new(new_cols, next_id + 1)));
                    next_id += 1;
                    col = 0;
                }
                if cursor_offset == Some(index) {
                    cursor_target = Some((out.len(), col));
                }
                row.attach(col, *cell, extra.clone());
                col += 1;
            }
            // The cursor can sit one past the last character -- at a prompt it
            // almost always does.
            if cursor_offset == Some(cells.len()) {
                let (r, c) = if col >= new_cols { (out.len() + 1, 0) } else { (out.len(), col) };
                cursor_target = Some((r, c));
            }
            out.push(row);
            next_id += 1;
        }

        // The viewport is the tail; everything above it is history.
        let rows = self.rows;
        while out.len() < rows {
            let mut blank = Row::new(new_cols, next_id);
            blank.reset(template, next_id);
            out.push(blank);
            next_id += 1;
        }
        let mut scrollback = out.len() - rows;
        if scrollback > self.scrollback_limit {
            let excess = scrollback - self.scrollback_limit;
            out.drain(..excess);
            scrollback = self.scrollback_limit;
        }

        // Rows the scrollback bound just dropped no longer exist, so pointing
        // at them would be worse than admitting they are gone: a caller that
        // re-anchors to a surviving id gets a wrong location, where one that
        // finds nothing knows to discard.
        if let Some(oldest) = out.first().map(|r| r.id) {
            reindex.map.retain(|&(_, new)| new >= oldest);
        }

        self.cols = new_cols;
        self.scrollback_len = scrollback;
        self.storage.set_next_id(next_id);
        self.storage.replace_all(out);

        if let Some((abs_row, abs_col)) = cursor_target {
            // Back into viewport coordinates, remembering rows may have been
            // dropped off the top by the scrollback limit.
            let base = self.storage.len().saturating_sub(rows);
            self.cursor.row = abs_row.saturating_sub(base).min(rows - 1);
            self.cursor.col = abs_col.min(new_cols - 1);
        } else {
            self.cursor.col = self.cursor.col.min(new_cols - 1);
        }
        self.cursor.pending_wrap = false;
        self.display_offset = self.display_offset.min(self.scrollback_len);
        reindex
    }

    /// Storage index of the row the cursor is on.
    fn storage_index_of_cursor(&self, total: usize) -> usize {
        // Active space: the cursor sits on the live screen, wherever the reader
        // has scrolled to. Reflow must carry *that* row across, or a resize
        // performed while scrolled back re-anchors the cursor into history.
        total.saturating_sub(self.rows) + self.cursor.row
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
        // An erase reaching the last column destroys the cell that carried
        // `CellFlags::WRAPLINE`, so the row's own flag has to go with it --
        // `set_wrapped` writes the two together and everything downstream
        // believes one or the other. `reflow` believes the flag, and a row
        // that still claims to continue into the next gets *rejoined* with it
        // at the next width change: two logical lines become one, the rows
        // below are dragged up, and every block anchored there names somebody
        // else's text.
        //
        // The path that made this matter rather than theoretical: a ConPTY
        // resize repaint terminates every row with `ESC[K` and overwrites in
        // place, never scrolling -- so `Row::reset`, the only other thing that
        // clears the flag, never runs. (#200)
        //
        // A partial erase correctly leaves it: the last cell survived, and so
        // did the wrap it records.
        if to >= cols.saturating_sub(1) {
            r.wrapped = false;
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
        // A cleared screen is all blank tail, so an unpaid restate debt would
        // pull history straight back onto the screen the user just cleared.
        self.cancel_restate_debt();
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
        // Scrollback only. `Storage` holds history *and* the viewport, so
        // iterating all of it handed a client rows that are still on screen --
        // which it then prepended to its own history, showing the current
        // screen duplicated above itself and growing its scrollback with rows
        // the host was about to send again as ordinary updates.
        self.storage
            .iter()
            .take(self.scrollback_len)
            .filter(|r| r.id >= from)
            .take(count)
            .collect()
    }
}

#[cfg(test)]
mod eviction_tests {
    use super::*;

    fn feed(g: &mut Grid, lines: usize) {
        for i in 0..lines {
            let text = alloc::format!("line {i}");
            for (col, ch) in text.chars().enumerate() {
                let cell = Cell { ch, ..Cell::default() };
                if let Some(c) = g.cell_mut(g.rows() - 1, col) {
                    *c = cell;
                }
            }
            g.scroll_up(1, &Cell::default());
        }
    }

    #[test]
    fn nothing_is_captured_unless_asked() {
        // The default has to stay free: capturing allocates a String per line on
        // the scroll path, and a 100MB `cat` scrolls a great many lines.
        let mut g = Grid::new(20, 3, 2);
        feed(&mut g, 12);
        assert!(g.take_evicted().is_empty(), "captured without being asked to");
    }

    #[test]
    fn rows_that_fall_off_the_top_come_back_in_order() {
        // The whole point: past the ring's limit the oldest row is recycled into
        // the newest, so this is the last moment its content exists anywhere.
        let mut g = Grid::new(20, 3, 2);
        g.capture_evicted(true);
        feed(&mut g, 12);

        let got = g.take_evicted();
        assert!(!got.is_empty(), "rows were evicted and none were captured");
        let ids: Vec<LineId> = got.iter().map(|e| e.id).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        assert_eq!(ids, sorted, "oldest first, or a client prepends history backwards");
    }

    #[test]
    fn taking_drains_rather_than_repeating() {
        // The caller is writing these somewhere durable. A buffer that only
        // grows is a leak, and one that repeats duplicates a session's history.
        let mut g = Grid::new(20, 3, 2);
        g.capture_evicted(true);
        feed(&mut g, 12);
        assert!(!g.take_evicted().is_empty());
        assert!(g.take_evicted().is_empty(), "the same rows came back twice");
    }

    #[test]
    fn trailing_blanks_are_not_stored() {
        // A row is `cols` cells wide whatever was printed into it. Storing the
        // padding would make a history of short lines cost a full-width row each.
        let mut g = Grid::new(80, 3, 1);
        g.capture_evicted(true);
        feed(&mut g, 8);
        for e in g.take_evicted() {
            assert!(e.text.len() < 80, "padding was stored: {:?}", e.text);
        }
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn narrowing_and_widening_restores_the_text() {
        // Without reflow, narrowing destroys the columns past the new width and
        // widening cannot bring them back, because they are gone. This is the
        // whole feature in one assertion.
        let mut t = crate::Terminal::new(40, 8, 500);
        t.advance(b"the quick brown fox jumps over the lazy dog\r\nsecond line\r\n");
        let before = t.screen_text();

        t.resize(20, 8);
        assert!(
            t.screen_text().contains("lazy"),
            "text past the new width was lost: {}",
            t.screen_text()
        );

        t.resize(40, 8);
        assert_eq!(t.screen_text(), before, "the round trip did not restore the text");
    }

    #[test]
    fn a_wrapped_line_is_rejoined_before_it_is_re_broken() {
        // The definition of a logical line: rows joined by `wrapped` are one
        // thing the program printed, and reflow works on that rather than on
        // rows.
        let mut t = crate::Terminal::new(10, 6, 500);
        t.advance(b"abcdefghijklmno");
        assert!(t.grid().row(0).wrapped, "the fixture did not wrap");

        t.resize(20, 6);
        assert_eq!(
            t.grid().row(0).text().trim_end(),
            "abcdefghijklmno",
            "the two halves were not rejoined"
        );
        assert!(!t.grid().row(0).wrapped, "the rejoined line is still marked wrapped");
    }

    #[test]
    fn a_wide_character_is_never_split_across_the_edge() {
        // Splitting one leaves a spacer with nothing to be the second half of,
        // which renders as a stray blank and breaks every column count after it.
        let mut t = crate::Terminal::new(10, 4, 500);
        t.advance("aaa世界世界".as_bytes());

        t.resize(5, 4);

        for row in 0..t.grid().rows() {
            for col in 0..t.grid().cols() {
                let cell = t.grid().cell(row, col).expect("cell");
                if cell.flags.contains(CellFlags::WIDE) {
                    let next = t.grid().cell(row, col + 1);
                    assert!(
                        next.is_some_and(|c| c.flags.contains(CellFlags::WIDE_SPACER)),
                        "a wide character at {row},{col} lost its spacer"
                    );
                }
            }
        }
    }

    #[test]
    fn the_cursor_follows_the_text_it_was_sitting_after() {
        // A prompt is a logical line with the cursor one past its last
        // character. If the cursor does not move with it, typing lands
        // somewhere else entirely.
        let mut t = crate::Terminal::new(40, 6, 500);
        t.advance(b"$ ");
        assert_eq!(t.cursor().col, 2);

        t.resize(20, 6);
        assert_eq!(t.cursor().col, 2, "the cursor left the end of the prompt");

        // And on a line long enough to wrap, the cursor stays on the *same
        // character* rather than at the same coordinates. Asserting the
        // character rather than the row is the point: whether the continuation
        // ends up at viewport row 1 or row 0 depends on how much scrolled into
        // scrollback, and neither answer is the property being tested.
        let mut t = crate::Terminal::new(40, 12, 500);
        t.advance(b"012345678901234567890123456789012345678Z");
        let under_cursor = t.grid().cell(t.cursor().row, t.cursor().col).expect("cell").ch;
        assert_eq!(under_cursor, 'Z', "the fixture did not leave the cursor on the last character");

        t.resize(20, 12);
        assert_eq!(
            t.grid().cell(t.cursor().row, t.cursor().col).expect("cell").ch,
            'Z',
            "the cursor left the character it was on"
        );
    }

    #[test]
    fn the_alternate_screen_is_never_reflowed() {
        // A full-screen program repaints on SIGWINCH, and its frame is a
        // picture rather than a paragraph -- rewrapping would mangle box
        // drawing that is about to be redrawn anyway.
        let mut t = crate::Terminal::new(20, 4, 500);
        t.advance(b"\x1b[?1049h");
        t.advance(b"\x1b[H\xe2\x94\x8c\xe2\x94\x80\xe2\x94\x80\xe2\x94\x90");

        t.resize(10, 4);

        // Truncated, not rewrapped: nothing moved onto a second row.
        assert_eq!(t.grid().rows(), 4);
        assert!(
            !t.grid().row(0).wrapped,
            "the alternate screen was rewrapped"
        );
    }

    #[test]
    fn combining_marks_survive_being_moved_between_rows() {
        // `Cell::extra` indexes the row that owns it, so carrying a cell
        // without its side-table entry would silently drop every accent.
        let mut t = crate::Terminal::new(6, 4, 500);
        t.advance("abcde\u{0301}f".as_bytes());
        let before = t.screen_text();

        t.resize(3, 4);
        t.resize(6, 4);

        assert_eq!(t.screen_text(), before, "a combining mark was lost in the move");
    }

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
    fn lines_by_id_returns_history_not_the_live_screen() {
        // `Storage` holds scrollback *and* the viewport. Iterating all of it
        // handed a remote client rows still on screen as "history", which it
        // prepended to its own scrollback -- so scrolling up showed the current
        // screen duplicated above itself.
        let mut g = Grid::new(20, 3, 100);
        for row in 0..3 {
            write_text(&mut g, row, &format!("visible {row}"));
        }
        g.cursor.row = 2;
        // Push two rows into history.
        g.scroll_up(2, &Cell::default());
        assert_eq!(g.scrollback_len(), 2);

        let history = g.lines_by_id(0, 100);
        assert_eq!(history.len(), 2, "returned {} rows for 2 of history", history.len());
        for row in &history {
            assert!(
                !row.text().contains("visible 2"),
                "a row still on screen was returned as history: {:?}",
                row.text()
            );
        }
    }

    #[test]
    fn rows_pushed_off_the_top_become_scrollback_rather_than_vanishing() {
        // The bug that was actually reported: "I resized it small and lines
        // pushed upwards and lost". They were deleted outright -- storage went
        // from holding ten lines to holding four, and the six that scrolled out
        // of the viewport were unreachable by scrolling, because as far as the
        // grid was concerned they had never been history at all.
        let mut g = Grid::new(30, 10, 500);
        for row in 0..10 {
            write_text(&mut g, row, &format!("line {row}"));
        }
        g.cursor.row = 9;
        assert_eq!(g.scrollback_len(), 0);

        g.resize(30, 4, &Cell::default());

        assert_eq!(g.rows(), 4);
        assert_eq!(g.scrollback_len(), 6, "the displaced rows did not become history");
        assert_eq!(g.total_lines(), 10, "rows were deleted rather than scrolled");
        assert_eq!(
            g.line(0).map(|r| r.text().trim_end().to_string()).as_deref(),
            Some("line 0"),
            "the oldest line must still be reachable by scrolling up"
        );
    }

    #[test]
    fn a_shrink_and_a_grow_restore_the_screen_exactly() {
        // Dragging a window smaller and back is one gesture. Growing pulls rows
        // back out of scrollback before adding blank ones, so it undoes the
        // shrink instead of leaving the content stranded above the viewport.
        let mut g = Grid::new(30, 10, 500);
        for row in 0..10 {
            write_text(&mut g, row, &format!("line {row}"));
        }
        g.cursor.row = 9;

        g.resize(30, 4, &Cell::default());
        g.resize(30, 10, &Cell::default());

        assert_eq!(g.scrollback_len(), 0, "history was not pulled back down");
        for row in 0..10 {
            assert_eq!(
                g.row(row).text().trim_end(),
                format!("line {row}"),
                "row {row} did not survive the round trip"
            );
        }
        assert_eq!(g.cursor.row, 9, "the cursor did not come back with the content");
    }

    #[test]
    fn a_restating_pty_keeps_its_history_in_scrollback_across_a_grow() {
        // The same gesture as the test above, against a pty that repaints. The
        // pull-back is *wrong* here and the opposite of harmless: ConPTY's
        // buffer is only as tall as the viewport, so shrinking to four rows
        // discards the rest, and growing back it repaints what it still has and
        // blanks everything else. Rows pulled down to meet that are erased --
        // and they are no longer in scrollback either, because the pull moved
        // the boundary past them. History destroyed, not misplaced. (#200)
        let mut g = Grid::new(30, 10, 500);
        g.set_viewport_restated_elsewhere(true);
        for row in 0..10 {
            write_text(&mut g, row, &format!("line {row}"));
        }
        g.cursor.row = 9;

        g.resize(30, 4, &Cell::default());
        let after_shrink = g.scrollback_len();
        assert_eq!(after_shrink, 6, "six rows went over the top and became history");

        g.resize(30, 10, &Cell::default());
        assert_eq!(
            g.scrollback_len(),
            6,
            "history stayed above the viewport, where the repaint cannot reach it"
        );

        // And it is still real text, reachable as history, rather than six rows
        // the repaint is about to blank.
        let history = g.lines_by_id(0, 6);
        assert_eq!(history.len(), 6, "six rows of history are readable");
        for (row, line) in history.iter().enumerate() {
            assert_eq!(line.text().trim_end(), format!("line {row}"));
        }
    }

    /// Rebuild what the repaint leaves behind, without a ConPTY.
    ///
    /// It restates from home, so the rows it still has land at the *top* and it
    /// blanks the rest — which is the whole shape of the bug: content high in a
    /// tall window with the prompt stranded above a blank half. `tests/vt.rs`
    /// drives the literal bytes; here the question is only what the boundary
    /// does afterwards, so the bytes would be ceremony.
    fn conpty_grow_repaint(g: &mut Grid, kept: usize) {
        let rows = g.rows();
        // Active space throughout, never `row()`: the repaint writes the live
        // screen, and a reader who is scrolled back must not change what ConPTY
        // is taken to have said.
        let texts: Vec<String> =
            (0..kept).map(|r| g.active_row(r).text().trim_end().to_string()).collect();
        for row in 0..rows {
            let cols = g.cols();
            g.erase_in_row(row, 0, cols - 1, &Cell::default());
        }
        for (row, text) in texts.iter().enumerate() {
            write_text(g, row, text);
        }
        g.cursor.row = kept - 1;
        g.settle_restate();
    }

    #[test]
    fn a_restating_grow_gives_the_history_back_once_the_repaint_has_landed() {
        // The other half of the test above, and the bug that was reported: the
        // history was safe, and it stayed above the viewport for ever. What the
        // user saw was a window dragged short and back with two lines of output
        // and a prompt jammed against the top of an otherwise empty pane.
        //
        // The pull is not wrong -- its *timing* was. Before the repaint it
        // hands ConPTY rows to blank; after it, the tail of the viewport is
        // blank rows the repaint itself wrote and nothing will write again.
        let mut g = Grid::new(30, 10, 500);
        g.set_viewport_restated_elsewhere(true);
        for row in 0..10 {
            write_text(&mut g, row, &format!("line {row}"));
        }
        g.cursor.row = 9;

        g.resize(30, 4, &Cell::default());
        g.resize(30, 10, &Cell::default());
        assert_eq!(
            g.scrollback_len(),
            6,
            "history must still be above the viewport until the repaint has spoken (#200)"
        );

        conpty_grow_repaint(&mut g, 4);

        assert_eq!(g.scrollback_len(), 0, "the grow never paid back what the shrink took");
        assert_eq!(g.cursor.row, 9, "the prompt did not come back to the bottom row");
        for row in 0..10 {
            assert_eq!(
                g.row(row).text().trim_end(),
                format!("line {row}"),
                "row {row} did not survive the drag"
            );
        }
    }

    #[test]
    fn several_grows_before_one_repaint_still_give_back_everything() {
        // A drag is a stream of resizes, not two: `ResizeObserver` and the
        // window server both fire throughout one gesture, so several grows
        // landing before any repaint closes is the common case rather than the
        // corner. What is pending therefore accumulates -- assigning it instead
        // drops every share but the last, and the drag comes out *nearly*
        // reversible, which is the hardest kind of wrong to notice.
        let mut g = Grid::new(30, 10, 500);
        g.set_viewport_restated_elsewhere(true);
        for row in 0..10 {
            write_text(&mut g, row, &format!("line {row}"));
        }
        g.cursor.row = 9;

        g.resize(30, 4, &Cell::default());
        g.resize(30, 6, &Cell::default());
        g.resize(30, 10, &Cell::default());
        conpty_grow_repaint(&mut g, 4);

        assert_eq!(g.scrollback_len(), 0, "the intermediate grow's share of the debt was dropped");
        assert_eq!(g.cursor.row, 9, "the prompt did not come back to the bottom row");
        for row in 0..10 {
            assert_eq!(g.row(row).text().trim_end(), format!("line {row}"));
        }
    }

    #[test]
    fn a_restating_grow_gives_nothing_back_if_the_screen_scrolled_in_between() {
        // The debt is only owed while the displaced rows are still the ones
        // immediately above the viewport. Once anything scrolls they are not,
        // and paying it would drag unrelated history down onto the screen --
        // most visibly after a `clear`, where every row below the cursor is
        // blank and the pull would undo exactly what the user asked for.
        let mut g = Grid::new(30, 10, 500);
        g.set_viewport_restated_elsewhere(true);
        for row in 0..10 {
            write_text(&mut g, row, &format!("line {row}"));
        }
        g.cursor.row = 9;

        g.resize(30, 4, &Cell::default());
        g.scroll_up(1, &Cell::default());
        let before = g.scrollback_len();
        g.resize(30, 10, &Cell::default());

        conpty_grow_repaint(&mut g, 1);

        assert_eq!(g.scrollback_len(), before, "history was pulled down after a scroll cancelled it");
    }

    #[test]
    fn a_settled_restate_leaves_no_gap_in_the_line_numbering() {
        // `truncate_bottom` drops the newest rows without rewinding the id
        // counter, and the rows the settle drops are the blanks the grow minted.
        // A gap makes `oldest_retained_line` -- computed as `active_row(0).id -
        // scrollback_len`, in `subscribe.rs` and `term.rs` -- name a line older
        // than any the grid holds, so the host offers clients scrollback it
        // cannot answer for.
        let mut g = Grid::new(30, 10, 500);
        g.set_viewport_restated_elsewhere(true);
        for row in 0..10 {
            write_text(&mut g, row, &format!("line {row}"));
        }
        g.cursor.row = 9;

        g.resize(30, 4, &Cell::default());
        g.resize(30, 10, &Cell::default());
        conpty_grow_repaint(&mut g, 4);

        let ids: Vec<LineId> = (0..g.total_lines()).map(|i| g.line(i).unwrap().id).collect();
        for pair in ids.windows(2) {
            assert_eq!(pair[1], pair[0] + 1, "the ids jumped: {ids:?}");
        }
        let oldest = g.active_row(0).id - g.scrollback_len() as LineId;
        assert!(
            g.lines_by_id(oldest, 1).len() == 1 || g.scrollback_len() == 0,
            "the retention horizon names a line the grid does not hold"
        );
    }

    #[test]
    fn settling_holds_a_scrolled_back_reader_on_the_same_text() {
        // The settle drops rows off the end of storage, and `viewport_base` is
        // measured from that end -- so leaving the offset alone slides the view
        // forward by exactly the rows it gave back. Nothing else in the resize
        // path asserts on `display_offset` at all, which is how this would have
        // shipped as "scrolling back jumps after you resize".
        // Deep enough history that the reader is genuinely scrolled back past
        // what the settle gives away. Scrolled back *less* than that and the
        // view cannot hold still -- the viewport swallows the whole of storage
        // and there is nowhere left to be scrolled to, which is a clamp rather
        // than a jump.
        let mut g = Grid::new(30, 10, 500);
        for i in 0..30 {
            if i > 0 {
                g.scroll_up(1, &Cell::default());
            }
            write_text(&mut g, 9, &format!("line {i}"));
        }
        g.cursor.row = 9;
        // Only now, so the scrolling above does not cancel the debt it creates.
        g.set_viewport_restated_elsewhere(true);

        g.resize(30, 4, &Cell::default());
        g.scroll_display(10);
        let reading = g.row(0).text().trim_end().to_string();
        assert_eq!(reading, "line 16", "the fixture is not where this test thinks it is");

        g.resize(30, 10, &Cell::default());
        conpty_grow_repaint(&mut g, 4);

        assert_eq!(
            g.row(0).text().trim_end(),
            reading,
            "the text under the reader moved when the grow settled"
        );
    }

    #[test]
    fn history_beyond_the_scrollback_limit_is_still_trimmed() {
        // The counterpart: turning displaced rows into scrollback must not let
        // a resize grow storage past what the user asked to keep.
        let mut g = Grid::new(30, 20, 5);
        for row in 0..20 {
            write_text(&mut g, row, &format!("line {row}"));
        }
        g.cursor.row = 19;

        g.resize(30, 2, &Cell::default());

        assert_eq!(g.scrollback_len(), 5, "scrollback exceeded its limit");
        assert_eq!(g.total_lines(), 7, "storage grew past the limit plus the viewport");
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
    fn output_written_while_scrolled_back_lands_on_the_live_screen() {
        // The other half of the test above, and the one that was missing: the
        // view holding still must not hold the *writes* still with it. Every
        // mutating accessor went through `viewport_base`, so while a reader was
        // scrolled up a running build printed over the very rows they were
        // reading -- and the fresh rows at the tail stayed blank, so the output
        // was gone when they scrolled back down.
        let mut g = Grid::new(20, 3, 100);
        write(&mut g, 0, "keep me");
        for _ in 0..3 {
            g.scroll_up(1, &Cell::default());
        }
        g.scroll_display(3);
        assert_eq!(g.row(0).text(), "keep me", "scrolled onto the line to read");

        // A program prints while the reader is up here.
        let last = g.rows() - 1;
        for i in 0..3 {
            g.scroll_up(1, &Cell::default());
            write(&mut g, last, &format!("line {i}"));
        }

        assert_eq!(g.row(0).text(), "keep me", "output overwrote what was being read");

        g.scroll_to_bottom();
        assert_eq!(
            (g.row(0).text(), g.row(1).text(), g.row(2).text()),
            ("line 0".into(), "line 1".into(), "line 2".into()),
            "output written while scrolled back never reached the live screen"
        );
    }

    #[test]
    fn editing_while_scrolled_back_leaves_the_reader_untouched() {
        // Erase, insert and delete all reach the row through `row_mut` too, so
        // they had the same reach into scrollback that printing did.
        let mut g = Grid::new(20, 3, 100);
        write(&mut g, 0, "precious");
        for _ in 0..3 {
            g.scroll_up(1, &Cell::default());
        }
        g.scroll_display(3);
        assert_eq!(g.row(0).text(), "precious");

        g.erase_rows(0, 2, &Cell::default());
        assert_eq!(g.row(0).text(), "precious", "an erase reached into scrollback");

        g.cursor.row = 0;
        g.cursor.col = 0;
        g.insert_cells(4, &Cell::default());
        assert_eq!(g.row(0).text(), "precious", "an insert reached into scrollback");
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

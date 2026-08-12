//! Applying deltas into a real [`Terminal`].
//!
//! The reference decoder for **Rust** clients, and the thing that makes
//! `docs/CONTRACTS.md`'s promise true — that a remote session keeps a real
//! local `Terminal`, so the renderer's path is identical at both ends of the
//! mesh. [`GridView`](crate::GridView) is the reference for *TypeScript*
//! clients and rebuilds a flat list of rows; it stays exactly as it is, because
//! a browser has no `Terminal` to write into.
//!
//! Two of them, rather than one shared implementation, is deliberate: they are
//! checked against each other and against the host's own `Terminal` in
//! `tests/conformance.rs`, and two independent readings of the same wire format
//! disagreeing is exactly the signal that format is ambiguous.
//!
//! # Cell widths are never recomputed
//!
//! A wide character arrives as *two runs* — one carrying the character with
//! `CellFlags::WIDE`, then one carrying no text with `WIDE_SPACER` — because
//! the encoder splits runs on attribute changes and the width flags live in the
//! attribute. So the rule is mechanical and needs no `wcwidth` at all: emit
//! `run.cells` cells, take characters from `run.text` in order, and use a space
//! once the text runs out.

use std::collections::HashMap;

use zest_core::{Cell, CellFlags, Modes, Terminal};

use crate::delta::{AttrDef, AttrId, CursorState, Delta, DeltaOp, RowPayload};
use crate::encode::Keyframe;

/// What the applier could not do.
///
/// Returned rather than swallowed: a grid that has silently diverged from its
/// host looks exactly like a working one, and stays wrong forever.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum Applied {
    Ok,
    /// Applied as far as possible; the caller must ask for a keyframe.
    ///
    /// Reached when an update's `base` is not the sequence this client holds,
    /// when a row lands outside the grid, or when a run names an attribute that
    /// was never defined. The middle one is the subtle one: there is no
    /// `DeltaOp::Resize`, so when another client attaches at a different size
    /// the host resizes the pty and the next delta describes a grid taller than
    /// this one. Growing to fit — which is right for a flat row list, and what
    /// `GridView` does — would leave this client and the shell permanently
    /// disagreeing about the size of the screen.
    NeedsKeyframe,
}

/// Applies wire deltas into a [`Terminal`].
///
/// One per attached session, holding the interning table, which is cumulative
/// for the session exactly as the encoder's is.
#[derive(Debug, Default)]
pub struct Applier {
    attrs: HashMap<AttrId, AttrDef>,
    /// Reused per row, so a 60Hz delta stream does not allocate per row.
    scratch: Vec<Cell>,
    /// The sequence this client actually holds.
    ///
    /// The host names it as `base` on every update. Keeping it here rather
    /// than in the caller's reader loop is deliberate: "discard an update whose
    /// base you do not hold" is the entire resync mechanism, and a rule that
    /// every transport has to remember to apply is a rule that one of them
    /// eventually will not.
    applied: u64,
}

impl Applier {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace everything with a complete state.
    pub fn apply_keyframe(&mut self, term: &mut Terminal, k: &Keyframe, seq: u64) {
        for a in &k.attrs {
            self.attrs.insert(a.id, *a);
        }

        let (cols, rows) = (usize::from(k.cols), usize::from(k.rows));
        if term.grid().cols() != cols || term.grid().rows() != rows {
            term.remote().resize(cols, rows);
        }

        // Modes before rows, for the same reason the encoder emits them first:
        // ALT_SCREEN decides which of the two grids these rows belong to.
        term.remote().set_modes(k.modes);

        let mut max_line = i64::MIN;
        for (i, payload) in k.rows_data.iter().enumerate() {
            self.write_row(term, i, payload);
            max_line = max_line.max(payload.line);
        }
        self.sync_lines(term, max_line);
        self.apply_cursor(term, k.cursor);

        // Blocks after the rows, always: they name absolute line ids, and
        // `sync_lines` above is what makes this grid's numbering the host's.
        //
        // Trim first: a keyframe is a complete state from `blocks_from` up, so
        // anything the client holds there that the host no longer has is gone
        // rather than merely unchanged. Upserting alone could never express a
        // removal, and a block destroyed by `cls` stayed on the client for
        // ever, painting a stale header over the live prompt.
        term.remote().drop_blocks_from(zest_core::BlockId(k.blocks_from));
        for b in &k.blocks {
            term.remote().upsert_block(b.to_block());
        }

        // Empty means "untitled on the wire" (it travels as absent), so an
        // older host's keyframe leaves whatever title the client had rather
        // than blanking it.
        if !k.title.is_empty() {
            term.remote().set_title(&k.title);
        }

        term.remote().mark_full();
        term.remote().set_seq(seq);
        self.applied = seq;
    }

    /// The sequence this client holds, and would acknowledge.
    #[must_use]
    pub fn applied(&self) -> u64 {
        self.applied
    }

    /// Apply one batch, atomically as far as the grid is concerned.
    ///
    /// `base` is the sequence the host built this delta against. If it is not
    /// what this client holds, **nothing is applied** and the caller must fetch
    /// a keyframe: a delta is a difference from a specific state, and applying
    /// one to a different state produces a grid that is wrong with no symptom.
    pub fn apply_delta(&mut self, term: &mut Terminal, d: &Delta, base: u64, seq: u64) -> Applied {
        if base != self.applied {
            return Applied::NeedsKeyframe;
        }

        // Attributes first: a run may name one defined in this same batch.
        for a in &d.attrs {
            self.attrs.insert(a.id, *a);
        }

        // A `Scroll` in this batch has already moved the displaced rows into
        // scrollback, with their real ids. Applying `SbPush` as well would push
        // them a second time, and every `oldest_line` after that is wrong.
        //
        // The natural reading of the op says "append this", which is why this
        // is a guard with a comment and a test rather than a subtlety someone
        // is expected to notice.
        let scrolled = d.ops.iter().any(|o| matches!(o, DeltaOp::Scroll { .. }));

        let mut outcome = Applied::Ok;
        let mut max_line = i64::MIN;

        for op in &d.ops {
            match op {
                DeltaOp::Scroll { top, bottom, lines } => {
                    term.remote().scroll(
                        usize::from(*top),
                        usize::from(*bottom),
                        i32::from(*lines),
                    );
                }
                DeltaOp::Row { row, payload } => {
                    let row = usize::from(*row);
                    if row >= term.grid().rows() {
                        outcome = Applied::NeedsKeyframe;
                        continue;
                    }
                    if !self.write_row(term, row, payload) {
                        outcome = Applied::NeedsKeyframe;
                    }
                    max_line = max_line.max(payload.line);
                }
                DeltaOp::Erase { top, left, bottom, right, attr } => {
                    let Some(def) = self.attrs.get(attr).copied() else {
                        outcome = Applied::NeedsKeyframe;
                        continue;
                    };
                    let template = Cell { ch: ' ', fg: def.fg, bg: def.bg, flags: def.flags, ..Cell::default() };
                    term.remote().erase(
                        usize::from(*top),
                        usize::from(*left),
                        usize::from(*bottom),
                        usize::from(*right),
                        &template,
                    );
                }
                DeltaOp::Cursor { cursor } => self.apply_cursor(term, *cursor),
                DeltaOp::SbPush { payload } => {
                    if scrolled {
                        continue;
                    }
                    let cells = self.expand(payload);
                    term.remote()
                        .prepend_history(&[(line_id(payload.line), cells, payload.wrapped)]);
                }
                DeltaOp::AltScreen { active } => term.remote().set_alt_screen(*active),
                DeltaOp::Modes { bits } => {
                    term.remote().set_modes(Modes::from_bits_truncate(*bits));
                }
                DeltaOp::Title { title } => term.remote().set_title(title),
            }
        }

        self.sync_lines(term, max_line);

        // Blocks are applied after every op, not interleaved with them, because
        // they name absolute line ids that the rows in this same batch have just
        // established. Order among themselves does not matter — they are keyed
        // upserts, which is why they are a field rather than a `DeltaOp`.
        for b in &d.blocks {
            term.remote().upsert_block(b.to_block());
        }

        term.remote().set_seq(seq);
        // Only advance on a clean apply. A partially applied batch is a state
        // the host cannot compute a delta against, so the next one must be
        // refused too rather than layered on top of a hole.
        if outcome == Applied::Ok {
            self.applied = seq;
        }
        outcome
    }

    /// Learn attributes that arrived outside a delta.
    ///
    /// `HostMessage::Scrollback` carries its own attribute definitions, because
    /// history is prepended rather than diffed and no later delta will define
    /// the ids it names. Without this the rows render in whatever style the
    /// client last held.
    pub fn absorb_attrs(&mut self, attrs: &[AttrDef]) {
        for a in attrs {
            self.attrs.insert(a.id, *a);
        }
    }

    /// History fetched with `RequestScrollback`, oldest first.
    pub fn apply_scrollback(&mut self, term: &mut Terminal, rows: &[RowPayload]) {
        let expanded: Vec<(u64, Vec<Cell>, bool)> =
            rows.iter().map(|p| (line_id(p.line), self.expand(p), p.wrapped)).collect();
        term.remote().prepend_history(&expanded);
    }

    /// Turn runs into cells. Returns false if a run named an unknown attribute.
    fn write_row(&mut self, term: &mut Terminal, row: usize, payload: &RowPayload) -> bool {
        let known = self.fill(payload);
        // `scratch` is borrowed from self, so it is moved aside for the call.
        let cells = std::mem::take(&mut self.scratch);
        term.remote().write_row(row, line_id(payload.line), &cells, payload.wrapped);
        self.scratch = cells;

        // Marks go on after the row, because writing the row resets the cells.
        let mut base = 0usize;
        for run in &payload.runs {
            for m in &run.marks {
                term.remote().push_marks(row, base + usize::from(m.at), &m.marks);
            }
            base += usize::from(run.cells);
        }
        known
    }

    /// Expand a payload into an owned row, for the history paths.
    fn expand(&mut self, payload: &RowPayload) -> Vec<Cell> {
        self.fill(payload);
        self.scratch.clone()
    }

    /// Fill `scratch` from a payload. Returns false on an unknown attribute.
    fn fill(&mut self, payload: &RowPayload) -> bool {
        self.scratch.clear();
        let mut known = true;
        for run in &payload.runs {
            let def = self.attrs.get(&run.attr).copied();
            if def.is_none() {
                known = false;
            }
            let (fg, bg, flags) = def.map_or_else(
                || (zest_core::Color::Default, zest_core::Color::Default, CellFlags::empty()),
                |d| (d.fg, d.bg, d.flags),
            );

            // The host already decided how many cells this occupies. Taking
            // characters in order and padding with spaces reproduces a wide
            // character's spacer without ever asking how wide anything is.
            let mut chars = run.text.chars();
            for _ in 0..run.cells {
                let ch = chars.next().unwrap_or(' ');
                self.scratch.push(Cell { ch, fg, bg, flags, ..Cell::default() });
            }
        }
        known
    }

    fn apply_cursor(&self, term: &mut Terminal, cursor: CursorState) {
        term.remote().set_cursor(usize::from(cursor.row), usize::from(cursor.col));
        let mut modes = term.modes();
        modes.set(Modes::SHOW_CURSOR, cursor.visible);
        term.remote().set_modes(modes);
        term.remote()
            .set_cursor_style(zest_core::CursorStyle::from_decscusr(u16::from(cursor.shape)));
    }

    /// Keep the client's line counter level with the host's.
    fn sync_lines(&self, term: &mut Terminal, max_line: i64) {
        if max_line > i64::MIN {
            let next = u64::try_from(max_line.saturating_add(1)).unwrap_or(0);
            term.remote().sync_next_line_id(next);
        }
    }
}

/// The wire carries a line id as `i64`; the grid holds `u64`.
///
/// The gap is not cosmetic: the encoder pads its shadow with `i64::MIN` as a
/// "row I have never seen" placeholder. Those never reach a `Row` op, but a
/// stray negative must clamp rather than wrap into a line id near `u64::MAX`,
/// which would put a row absurdly far in the future and break every ordering
/// comparison after it.
fn line_id(line: i64) -> u64 {
    u64::try_from(line).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::delta::Run;
    use crate::encode::Encoder;

    fn cursor() -> CursorState {
        CursorState { row: 0, col: 0, visible: true, shape: 0 }
    }

    /// Host and client, driven from the same VT bytes.
    struct Pair {
        host: Terminal,
        client: Terminal,
        enc: Encoder,
        app: Applier,
        seq: u64,
    }

    impl Pair {
        fn new(cols: usize, rows: usize) -> Self {
            let mut p = Self {
                host: Terminal::new(cols, rows, 100),
                client: Terminal::new(cols, rows, 100),
                enc: Encoder::new(),
                app: Applier::new(),
                seq: 0,
            };
            let k = p.enc.keyframe(p.host.grid(), cursor(), p.host.modes(), "", p.host.blocks());
            p.app.apply_keyframe(&mut p.client, &k, 0);
            p
        }

        /// Drive both sides from the same bytes.
        ///
        /// Asserts the apply succeeded rather than returning it: every caller
        /// here feeds ordinary VT into a correctly sized grid, so anything but
        /// `Ok` means the applier refused something it should have taken, and
        /// a test that quietly ignored that would still pass while asserting
        /// against a grid that was never updated.
        fn feed(&mut self, bytes: &[u8]) {
            self.host.advance(bytes);
            self.seq += 1;
            let base = self.app.applied();
            let d = self.enc.delta(self.host.grid(), cursor(), self.host.modes(), "", self.host.blocks());
            assert_eq!(
                self.app.apply_delta(&mut self.client, &d, base, self.seq),
                Applied::Ok,
                "the applier refused an ordinary delta"
            );
        }

        fn assert_same(&self, why: &str) {
            assert_eq!(
                self.client.screen_text(),
                self.host.screen_text(),
                "{why}\nclient:\n{}\nhost:\n{}",
                self.client.screen_text(),
                self.host.screen_text()
            );
        }
    }

    #[test]
    fn a_block_the_host_destroyed_leaves_the_client_too() {
        // The half that made this bug survive the host-side fix: the window is
        // a client of its own daemon, so a block dropped on the host was still
        // drawn in the window. `diff_blocks` cannot say "removed" and the
        // applier only upserted, so even a keyframe left the stale block --
        // and a stale block with an `output_line` outranks the live prompt in
        // the header pass, which is the opaque band over the row being typed.
        let mut p = Pair::new(20, 4);
        p.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07ls\x1b]133;C\x07\r\nout\r\n\x1b]133;D;0\x07");
        assert_eq!(p.client.blocks().blocks().len(), 1, "the block crossed");

        // Advance the host alone, then ask -- the order the daemon uses, and
        // it has to be that way round: `diff_blocks` overwrites the encoder's
        // shadow, so a predicate consulted after encoding a delta has already
        // lost the evidence.
        p.host.advance(b"\x1b[2J\x1b[H");
        assert!(p.host.blocks().blocks().is_empty(), "the host dropped what it erased");
        assert!(
            p.enc.blocks_need_keyframe(p.host.blocks()),
            "the encoder must ask for a resync when a block is destroyed"
        );

        p.feed(b"");
        assert_eq!(
            p.client.blocks().blocks().len(),
            1,
            "a delta cannot express a removal -- this is why a keyframe is forced"
        );

        let k = p.enc.keyframe(p.host.grid(), cursor(), p.host.modes(), "", p.host.blocks());
        p.seq += 1;
        p.app.apply_keyframe(&mut p.client, &k, p.seq);
        assert!(p.client.blocks().blocks().is_empty(), "the keyframe carried the removal");
    }

    #[test]
    fn a_clear_removes_only_what_it_erased_from_the_client() {
        // The floor is a boundary, not a reset: the command whose output is
        // already in scrollback was not erased and must survive on both sides.
        let mut p = Pair::new(20, 3);
        p.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07old\x1b]133;C\x07\r\nout\r\n\x1b]133;D;0\x07");
        for _ in 0..6 {
            p.feed(b"filler\r\n");
        }
        p.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07new\x1b]133;C\x07\r\nout\r\n\x1b]133;D;0\x07");
        assert_eq!(p.client.blocks().blocks().len(), 2, "both commands crossed");

        p.host.advance(b"\x1b[2J\x1b[H");
        let k = p.enc.keyframe(p.host.grid(), cursor(), p.host.modes(), "", p.host.blocks());
        p.seq += 1;
        p.app.apply_keyframe(&mut p.client, &k, p.seq);

        let kept: Vec<&str> =
            p.client.blocks().blocks().iter().map(|b| b.command.as_str()).collect();
        assert_eq!(kept, ["old"], "only the erased block goes");
    }

    #[test]
    fn eviction_alone_never_forces_a_keyframe() {
        // The distinction the predicate has to make. Blocks falling off the
        // oldest end are deliberately silent -- a client configured to keep
        // more history than the host keeps it -- so a trim from the front must
        // not cost a full repaint on a long-lived session.
        let mut p = Pair::new(20, 3);
        for i in 0..30 {
            p.feed(format!("\x1b]133;A\x07$ \x1b]133;B\x07c{i}\x1b]133;C\x07\r\n").as_bytes());
            p.feed(format!("out {i}\r\n\x1b]133;D;0\x07").as_bytes());
            assert!(
                !p.enc.blocks_need_keyframe(p.host.blocks()),
                "step {i}: ordinary command traffic must not force a keyframe"
            );
        }
    }

    #[test]
    fn plain_output_reaches_the_client() {
        let mut p = Pair::new(20, 3);
        p.feed(b"hello world");
        p.assert_same("plain text");
    }

    #[test]
    fn a_wide_character_occupies_two_cells_without_recomputing_width() {
        // The applier never calls wcwidth. If this passes, the two-runs rule
        // reproduces the spacer purely from `Run::cells`.
        let mut p = Pair::new(20, 3);
        p.feed("世界".as_bytes());
        p.assert_same("wide characters");
        assert!(
            p.client.grid().cell(0, 1).unwrap().flags.contains(CellFlags::WIDE_SPACER),
            "the second cell of a wide character must be a spacer"
        );
    }

    #[test]
    fn scrolling_keeps_the_client_level_with_the_host() {
        let mut p = Pair::new(20, 3);
        for i in 0..10 {
            p.feed(format!("line {i}\r\n").as_bytes());
        }
        p.assert_same("after scrolling past the viewport");
        assert_eq!(
            p.client.grid().row(0).id,
            p.host.grid().row(0).id,
            "absolute line ids must match, or blocks and selections name different rows"
        );
    }

    #[test]
    fn a_client_scrolled_back_still_applies_onto_its_live_screen() {
        // A client is a reader with a grid of its own, so it is the *client*
        // that is scrolled while the host streams. `write_row` resolved its row
        // through the display, so an applied row landed in the reader's
        // scrollback -- and, worse, stamped the host's fresh `LineId` onto it,
        // breaking the ordering `BlockIndex::upsert`, `block_at` and
        // `evict_before` all document as a precondition.
        let mut p = Pair::new(20, 3);
        for i in 0..6 {
            p.feed(format!("early {i}\r\n").as_bytes());
        }
        p.client.scroll_display(4);
        let reading = p.client.screen_text();

        for i in 0..4 {
            p.feed(format!("late {i}\r\n").as_bytes());
        }
        assert_eq!(p.client.screen_text(), reading, "applying moved the reader's view");

        let ids: Vec<u64> = (0..p.client.grid().total_lines())
            .filter_map(|i| p.client.grid().line(i))
            .map(|r| r.id)
            .collect();
        assert!(
            ids.windows(2).all(|w| w[0] < w[1]),
            "line ids must ascend across storage, got {ids:?}"
        );

        p.client.scroll_to_bottom();
        p.assert_same("after the reader scrolled back to the bottom");
    }

    #[test]
    fn entering_the_alternate_screen_does_not_corrupt_the_primary_one() {
        // The regression this whole ordering rule exists for. Before the fix
        // the rows describing the alt screen were written into the primary
        // grid, so leaving vim showed vim's first frame in the scrollback.
        let mut p = Pair::new(20, 3);
        p.feed(b"SHELL OUTPUT\r\n");
        p.feed(b"\x1b[?1049h\x1b[HVIM");
        p.assert_same("inside the alternate screen");
        assert!(
            !p.client.screen_text().contains("SHELL OUTPUT"),
            "the alt screen must not show the primary one: {}",
            p.client.screen_text()
        );

        p.feed(b"\x1b[?1049l");
        p.assert_same("after leaving the alternate screen");
        assert!(
            p.client.screen_text().contains("SHELL OUTPUT"),
            "the primary screen must survive a trip through the alt screen: {}",
            p.client.screen_text()
        );
        assert!(
            !p.client.screen_text().contains("VIM"),
            "vim's frame must not be stamped into the primary grid: {}",
            p.client.screen_text()
        );
    }

    #[test]
    fn combining_marks_reach_the_client() {
        // They did not, until `Run::marks` existed: the encoder wrote
        // `Cell::ch` and never touched the side table, so `e` + U+0301 arrived
        // as a bare `e`. Silent text corruption for anyone whose input method
        // produces decomposed Unicode, which on macOS is not rare.
        let mut p = Pair::new(20, 3);
        p.feed("naïve café".as_bytes());
        p.assert_same("precomposed");

        // And the decomposed form, which is the case that was broken.
        let mut p = Pair::new(20, 3);
        p.feed("cafe\u{0301}".as_bytes());
        assert_eq!(
            p.client.screen_text().trim_end(),
            p.host.screen_text().trim_end(),
            "a combining mark did not survive the wire"
        );
        assert!(
            p.client.screen_text().contains('\u{0301}'),
            "the mark is missing from the client: {:?}",
            p.client.screen_text()
        );
    }

    #[test]
    fn marks_land_on_the_right_cell_after_several_runs() {
        // `CellMarks::at` is an offset *within its run*, and runs split on
        // attribute changes -- so an accent after a colour change lands on the
        // wrong character if the run base is not added back.
        let mut p = Pair::new(30, 3);
        p.feed("\x1b[31mred\x1b[0m cafe\u{0301}!".as_bytes());
        assert_eq!(
            p.client.screen_text().trim_end(),
            p.host.screen_text().trim_end(),
            "a mark landed on the wrong cell across a run boundary"
        );
    }

    #[test]
    fn modes_reach_the_client() {
        // Without this the client cannot encode its own keystrokes.
        let mut p = Pair::new(20, 3);
        p.feed(b"\x1b[?1h\x1b[?2004h");
        assert!(p.client.modes().contains(Modes::APP_CURSOR));
        assert!(p.client.modes().contains(Modes::BRACKETED_PASTE));
        assert_eq!(p.client.modes(), p.host.modes());
    }

    #[test]
    fn the_kitty_keyboard_flags_reach_the_client() {
        // The client is where keystrokes are encoded, so flags that stay on the
        // host are flags nobody applies.
        //
        // This covers encode and apply, *not* the decision to send: `feed`
        // encodes unconditionally, while the daemon asks `update_for(seq)`
        // first. `changing_the_flags_bumps_the_sequence` in zest-core is what
        // guards that half, and it is the one that catches the real bug --
        // verified by removing the fix and watching only that test fail.
        let mut p = Pair::new(20, 3);
        p.feed(b"\x1b[>9u");
        assert_eq!(p.client.modes().kitty_flags(), 9, "pushed flags did not cross the wire");
        assert_eq!(p.client.modes(), p.host.modes());

        p.feed(b"\x1b[<u");
        assert_eq!(p.client.modes().kitty_flags(), 0, "the pop did not cross either");
        assert_eq!(p.client.modes(), p.host.modes());
    }

    #[test]
    fn a_late_client_is_told_the_keyboard_flags_in_its_keyframe() {
        // Attaching to a session where `nvim` is already running must not mean
        // encoding the legacy way until the next push.
        let mut p = Pair::new(20, 3);
        p.feed(b"\x1b[>9u");

        let mut late = Terminal::new(20, 3, 100);
        let k = p.enc.keyframe(p.host.grid(), cursor(), p.host.modes(), "", p.host.blocks());
        p.app.apply_keyframe(&mut late, &k, 1);
        assert_eq!(late.modes().kitty_flags(), 9);
    }

    #[test]
    fn a_keyframe_title_reaches_a_late_client() {
        // A tab attaching to a session already titled shows that title
        // immediately, not on the next retitle.
        let mut p = Pair::new(20, 3);
        let k = p.enc.keyframe(p.host.grid(), cursor(), p.host.modes(), "vim", p.host.blocks());
        p.app.apply_keyframe(&mut p.client, &k, 1);
        assert_eq!(p.client.title(), "vim");

        // An older host's keyframe carries no title (empty travels as
        // absent); it must leave the client's title alone, not blank it.
        let k = p.enc.keyframe(p.host.grid(), cursor(), p.host.modes(), "", p.host.blocks());
        p.app.apply_keyframe(&mut p.client, &k, 2);
        assert_eq!(p.client.title(), "vim", "an absent title must not erase a known one");
    }

    #[test]
    fn a_row_past_the_end_asks_for_a_keyframe() {
        // What happens when another client attaches at a different size: the
        // host resizes and the next delta describes a taller grid.
        let mut p = Pair::new(20, 3);
        let d = Delta {
            blocks: Vec::new(),
            attrs: vec![AttrDef {
                id: AttrId(0),
                fg: zest_core::Color::Default,
                bg: zest_core::Color::Default,
                flags: CellFlags::empty(),
            }],
            ops: vec![DeltaOp::Row {
                row: 99,
                payload: RowPayload {
                    line: 99,
                    runs: vec![Run { attr: AttrId(0), cells: 1, text: "x".into(), marks: Vec::new() }],
                    wrapped: false,
                },
            }],
        };
        let base = p.app.applied();
        assert_eq!(p.app.apply_delta(&mut p.client, &d, base, 1), Applied::NeedsKeyframe);
        assert_eq!(p.client.grid().rows(), 3, "the grid must not have grown");
    }

    #[test]
    fn a_run_naming_an_undefined_attribute_asks_for_a_keyframe() {
        let mut p = Pair::new(20, 3);
        let d = Delta {
            blocks: Vec::new(),
            attrs: Vec::new(),
            ops: vec![DeltaOp::Row {
                row: 0,
                payload: RowPayload {
                    line: 0,
                    runs: vec![Run { attr: AttrId(777), cells: 1, text: "x".into(), marks: Vec::new() }],
                    wrapped: false,
                },
            }],
        };
        let base = p.app.applied();
        assert_eq!(p.app.apply_delta(&mut p.client, &d, base, 1), Applied::NeedsKeyframe);
    }

    #[test]
    fn a_scroll_and_an_sbpush_together_do_not_double_push() {
        // The encoder sends both, because a client that coalesced the scroll
        // away still needs to know what left. Applying both here would put the
        // same line into history twice.
        let mut p = Pair::new(20, 3);
        for i in 0..6 {
            p.feed(format!("row {i}\r\n").as_bytes());
        }
        assert_eq!(
            p.client.grid().scrollback_len(),
            p.host.grid().scrollback_len(),
            "history length diverged, which double-pushing would cause"
        );
    }

    #[test]
    fn erase_paints_the_attribute_it_names() {
        let mut p = Pair::new(20, 3);
        p.feed(b"\x1b[41m          \x1b[0m");
        let bg = zest_core::Color::Indexed(1);
        let d = Delta {
            blocks: Vec::new(),
            attrs: vec![AttrDef {
                id: AttrId(200),
                fg: zest_core::Color::Default,
                bg,
                flags: CellFlags::empty(),
            }],
            ops: vec![DeltaOp::Erase { top: 1, left: 0, bottom: 1, right: 4, attr: AttrId(200) }],
        };
        let base = p.app.applied();
        assert_eq!(p.app.apply_delta(&mut p.client, &d, base, 99), Applied::Ok);
        assert_eq!(p.client.grid().cell(1, 0).unwrap().bg, bg, "erase must paint its attribute");
    }

    #[test]
    fn the_sequence_follows_the_host() {
        let mut p = Pair::new(20, 3);
        p.feed(b"x");
        assert_eq!(p.client.seq(), p.seq, "the client's seq is what it acknowledges");
    }
}

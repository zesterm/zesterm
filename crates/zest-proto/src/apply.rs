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
    /// Shadow of [`Keyframe::history_clears`]: the last announced-destruction
    /// count this client has honoured. Only ever raised — an alt-screen
    /// keyframe carries the alt grid's 0 and must not re-arm. (#314)
    history_clears: u32,
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

        // An advanced counter means an ED 3 destroyed the session's scrollback
        // since this client last honoured one — drop ours too, including any
        // history the host never held. Before the dedupe below and the row
        // writes: with history gone there is nothing to deduplicate against,
        // and the rows this keyframe carries are the whole surviving session.
        // Never lowered: an alt-screen keyframe carries that grid's 0. (#314)
        let cleared = k.history_clears > self.history_clears;
        if cleared {
            term.remote().clear_history();
            self.history_clears = k.history_clears;
        }

        // History this keyframe is about to re-deliver stops being history.
        //
        // The inverse of the banking a shrink does: when the host gives back
        // rows a shrink displaced, its viewport starts earlier than it did and
        // every row it re-sends is one this replica still holds above the
        // boundary. Left there the same line exists twice in one grid and every
        // walk by id shows it twice -- the listing duplicated, a block spanning
        // both copies drawing on its own. Cheap and a no-op in the common case,
        // where the keyframe's first line is newer than anything in history.
        // (#291)
        // Exactly the lines the keyframe *names* -- the rule all three readers
        // share now, stated first in `grid-view.ts`: ids have gaps, so "at or
        // above the first" is a strictly larger set, and the difference is the
        // client's only copy of rows the host destroyed. (#313)
        // Negatives filtered, not clamped: the encoder pads with `i64::MIN` for
        // a row it has never seen, and `line_id` turns that into 0 — which
        // named here would drop a history line that never existed, and as a
        // floor would have dropped the client's entire history on any keyframe
        // carrying one blank row.
        let named: Vec<zest_core::LineId> =
            k.rows_data.iter().map(|r| r.line).filter(|&l| l >= 0).map(line_id).collect();
        // The two directions are exclusive by construction, exactly as in
        // `grid-view.ts` and `decode.rs`: a keyframe starting *later* than
        // rows this replica still shows (the host stranded a restore, #341)
        // banks them before they are overwritten; one re-delivering rows held
        // as history takes the stale copies back (#291/#313). Suppressed on a
        // clearing keyframe — those rows are pre-destruction (#314).
        if let Some(&first) = named.iter().min() {
            let displaced = if cleared {
                0
            } else {
                let g = term.grid();
                let last_held = (g.scrollback_len() > 0)
                    .then(|| g.line(g.scrollback_len() - 1).map(|r| r.id))
                    .flatten();
                (0..g.rows())
                    .map(|r| g.active_row(r).id)
                    .take_while(|&id| id < first && last_held.is_none_or(|l| id > l))
                    .count()
            };
            if displaced > 0 {
                term.remote().bank_displaced(displaced);
            } else {
                term.remote().drop_history(&named);
            }
        }

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
            let cur = self.live_cursor();
            let d = self.enc.delta(self.host.grid(), cur, self.host.modes(), "", self.host.blocks());
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

    impl Pair {
        /// The host's *real* cursor, not the harness's constant.
        ///
        /// It matters wherever a resize is involved: `Grid::resize` gives up
        /// the blank rows below the cursor before taking any over the top, so a
        /// client told its cursor is on row 0 banks nothing and a fixture built
        /// on `cursor()` cannot reach the code this is about.
        fn live_cursor(&self) -> CursorState {
            let c = self.host.cursor();
            CursorState {
                row: c.row as u16,
                col: c.col as u16,
                // Read, not assumed. A hard-coded `true` would be this whole
                // class of bug again: ConPTY's repaint hides the cursor and
                // shows it again, so a harness that always reports it visible
                // is a stand-in kinder than the thing it stands for, and a
                // cursor-visibility regression would pass straight through.
                visible: self.host.modes().contains(Modes::SHOW_CURSOR),
                shape: self.host.cursor_style().to_decscusr() as u8,
            }
        }

        /// Push the client a keyframe with no other change, as the daemon does
        /// in answer to `TermEvent::HistoryCleared` (or any resync).
        fn keyframe(&mut self) {
            self.seq += 1;
            let cur = self.live_cursor();
            let k = self.enc.keyframe(
                self.host.grid(),
                cur,
                self.host.modes(),
                "",
                self.host.blocks(),
            );
            self.app.apply_keyframe(&mut self.client, &k, self.seq);
        }

        /// Resize the host and push it a keyframe, as `reconcile_size` does.
        fn resize(&mut self, cols: usize, rows: usize) {
            self.host.resize(cols, rows);
            self.seq += 1;
            let cur = self.live_cursor();
            let k = self.enc.keyframe(
                self.host.grid(),
                cur,
                self.host.modes(),
                "",
                self.host.blocks(),
            );
            self.app.apply_keyframe(&mut self.client, &k, self.seq);
        }

        /// What ConPTY sends back: hide, home, restate what it kept, show.
        /// No size announcement, because a *grow* does not carry one (#271).
        fn conpty_grow_repaint(&mut self, kept: &[&str]) {
            let rows = self.host.grid().rows();
            let mut out = String::from("\x1b[?25l\x1b[H");
            for r in 0..rows {
                if r > 0 {
                    out.push_str("\r\n");
                }
                out.push_str(kept.get(r).copied().unwrap_or(""));
                out.push_str("\x1b[K");
            }
            out.push_str(&format!("\x1b[{};1H\x1b[?25h", kept.len().max(1)));
            self.host.advance(out.as_bytes());
            self.seq += 1;
            let cur = self.live_cursor();
            let k = self.enc.keyframe(
                self.host.grid(),
                cur,
                self.host.modes(),
                "",
                self.host.blocks(),
            );
            self.app.apply_keyframe(&mut self.client, &k, self.seq);
        }

        fn client_line_ids(&self) -> Vec<u64> {
            (0..self.client.grid().total_lines())
                .map(|i| self.client.grid().line(i).unwrap().id)
                .collect()
        }
    }

    #[test]
    fn a_settled_grow_does_not_leave_the_client_holding_the_listing_twice() {
        // The whole path, host to client: a Windows host settles a grow, its
        // keyframe's viewport starts earlier than it did, and every row it
        // re-sends is one the client already holds as history.
        //
        // Reported as "the listing is duplicated and a block renders on its
        // own" after #281 made the settle fire for the first time. Before that
        // a grow keyframe carried the same short viewport it had before, so
        // nothing overlapped and the missing dedupe never showed. (#291)
        let mut p = Pair::new(20, 8);
        p.host.set_pty_restates_viewport(true);
        p.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07ls\x1b]133;C\x07\r\n");
        for i in 0..6 {
            p.feed(format!("entry {i}\r\n").as_bytes());
        }

        p.resize(20, 2);
        p.conpty_grow_repaint(&["entry 5", ""]);
        assert_eq!(p.client.grid().scrollback_len(), 6, "the client banked nothing, so this proves nothing");

        p.resize(20, 8);
        p.conpty_grow_repaint(&["entry 5", ""]);
        assert_eq!(p.host.grid().scrollback_len(), 0, "the host never settled, so this proves nothing");

        let ids = p.client_line_ids();
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "the client holds a line twice: {ids:?}");
        assert_eq!(
            p.client.grid().scrollback_len(),
            p.host.grid().scrollback_len(),
            "the client and the host disagree about where history ends"
        );
        p.assert_same("the client's screen drifted from the host's across a settled drag");
    }

    #[test]
    fn cls_after_a_drag_clears_the_client_too() {
        // The loudest of the three symptoms reported: after one `ls` and a
        // drag, `cls` looked like it did nothing. Checked here rather than
        // assumed to be a consequence of the duplication, because "the screen
        // still shows the listing" is what a client holding two copies of it
        // looks like *and* what a genuinely unhandled clear looks like, and
        // those want different fixes. (#291)
        let mut p = Pair::new(20, 8);
        p.host.set_pty_restates_viewport(true);
        p.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07ls\x1b]133;C\x07\r\n");
        for i in 0..6 {
            p.feed(format!("entry {i}\r\n").as_bytes());
        }
        p.resize(20, 2);
        p.conpty_grow_repaint(&["entry 5", ""]);
        p.resize(20, 8);
        p.conpty_grow_repaint(&["entry 5", ""]);

        // `cls`: erase the screen, home the cursor, print a prompt.
        p.feed(b"\x1b[2J\x1b[H\x1b]133;A\x07$ ");

        p.assert_same("the client's screen still shows what cls erased");
        let ids = p.client_line_ids();
        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "the client holds a line twice after cls: {ids:?}");
        assert!(
            !p.client.screen_text().contains("entry 0"),
            "cls left the listing on the client's screen:\n{}",
            p.client.screen_text()
        );
    }

    #[test]
    fn a_keyframe_naming_little_does_not_cost_the_client_the_history_it_kept() {
        // Hand-built rather than through `Pair`, deliberately: the Pair
        // encoder can only produce healthy keyframes, whose named lines and
        // "everything at or above the first" are the same set. The rules
        // diverge across id gaps — `truncate_bottom` destroys ids without
        // rewinding the counter — and a degenerate keyframe is precisely the
        // state a host-side bug hands a client, which is when deleting on an
        // id comparison costs the only surviving copy. (#313)
        let row_payload = |line: i64, text: &str| RowPayload {
            line,
            runs: vec![Run {
                attr: AttrId(0),
                cells: text.chars().count() as u16,
                text: text.into(),
                marks: Vec::new(),
            }],
            wrapped: false,
        };
        let mut client = Terminal::new(10, 4, 100);
        for r in 0..4 {
            client.remote().write_row(r, 108 + r as u64, &[Cell::default(); 10], false);
        }
        let hist: Vec<(zest_core::LineId, Vec<Cell>, bool)> = (0..8)
            .map(|i| (100 + i as u64, vec![Cell::default(); 10], false))
            .collect();
        client.remote().prepend_history(&hist);

        let k = Keyframe {
            cols: 10,
            rows: 4,
            rows_data: vec![
                row_payload(104, "back"),
                row_payload(110, "live"),
                row_payload(111, "live"),
                row_payload(112, "live"),
            ],
            attrs: Vec::new(),
            cursor: cursor(),
            modes: Modes::empty(),
            blocks: Vec::new(),
            blocks_from: 0,
            title: String::new(),
            history_clears: 0,
        };
        Applier::new().apply_keyframe(&mut client, &k, 1);

        let held: Vec<u64> = (0..client.grid().scrollback_len())
            .map(|i| client.grid().line(i).unwrap().id)
            .collect();
        assert_eq!(
            held,
            vec![100, 101, 102, 103, 105, 106, 107],
            "history the keyframe never named was dropped on an id comparison"
        );
    }

    #[test]
    fn cls_with_ed3_clears_every_replicas_history_too() {
        // The reported gesture, host to client: `cls` in pwsh is `2J 3J H`
        // (measured — Clear-Host under ConPTY emits the 3J explicitly), the
        // host destroys its scrollback, and the keyframe the daemon owes
        // everyone carries `history_clears` so the replica drops its own.
        // Without it the client keeps the history and scrolling up shows
        // everything the user just asked to be rid of. (#314)
        let mut p = Pair::new(20, 4);
        p.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07ls\x1b]133;C\x07\r\n");
        for i in 0..8 {
            p.feed(format!("entry {i}\r\n").as_bytes());
        }
        assert!(p.client.grid().scrollback_len() > 0, "the client holds no history, so this proves nothing");

        p.feed(b"\x1b[2J\x1b[3J\x1b[H");
        assert_eq!(p.host.grid().scrollback_len(), 0, "the host kept what ED 3 destroyed");

        // The daemon answers `TermEvent::HistoryCleared` with a keyframe.
        p.keyframe();
        assert_eq!(
            p.client.grid().scrollback_len(),
            0,
            "the client still holds history the session no longer has"
        );
        p.assert_same("the client's screen drifted across a cls");
    }

    #[test]
    fn a_reattaching_client_drops_history_cleared_while_it_was_away() {
        // The counter is level-triggered on the keyframe, not edge-triggered on
        // the event, for exactly this client: one that was not attached when
        // the `cls` happened. Its next keyframe carries the advanced counter
        // and its history — including any the host never held — goes too.
        let mut p = Pair::new(20, 4);
        p.feed(b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\r\n");
        assert!(p.client.grid().scrollback_len() > 0, "the client holds no history, so this proves nothing");

        // The cls happens while nobody watches: the host clears, and the next
        // thing the client sees is an ordinary keyframe.
        p.host.advance(b"\x1b[2J\x1b[3J\x1b[H");
        p.keyframe();

        assert_eq!(
            p.client.grid().scrollback_len(),
            0,
            "a keyframe carrying an advanced counter left the client's history alone"
        );
    }

    #[test]
    fn a_keyframe_with_an_unmoved_counter_keeps_the_clients_longer_history() {
        // The other half, which pins that this does not regress deliberate
        // asymmetry: eviction is silent, and a client keeping more history
        // than the host keeps it. Only announced destruction reaches across.
        let mut p = Pair::new(20, 4);
        p.feed(b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\r\n");
        let held = p.client.grid().scrollback_len();
        assert!(held > 0, "the client holds no history, so this proves nothing");

        p.keyframe();
        assert_eq!(
            p.client.grid().scrollback_len(),
            held,
            "an ordinary keyframe evicted the client's history"
        );
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
    fn the_next_prompt_does_not_hide_the_removal() {
        // The real sequence, and the one a live daemon caught after the tests
        // above passed. pwsh emits its prompt straight after `Clear-Host`, so
        // by the time anyone asks, the host holds a *newer* block and its
        // oldest id sits above everything the clear destroyed. Measured
        // against that id the removal looks like ordinary eviction, no
        // keyframe is sent, and the client keeps drawing a dead command over
        // the row being typed on. `authoritative_from` is what tells them
        // apart.
        let mut p = Pair::new(20, 4);
        p.feed(b"\x1b]133;A\x07$ \x1b]133;B\x07ls\x1b]133;C\x07\r\nout\r\n\x1b]133;D;0\x07");
        assert_eq!(p.client.blocks().blocks().len(), 1);

        // Clear *and* the prompt that follows it, in one advance.
        p.host.advance(b"\x1b[2J\x1b[H\x1b]133;A\x07$ \x1b]133;B\x07");
        assert_eq!(p.host.blocks().blocks().len(), 1, "the host has a fresh prompt block");
        assert!(
            p.enc.blocks_need_keyframe(p.host.blocks()),
            "a newer block must not disguise the destroyed one as evicted"
        );

        let k = p.enc.keyframe(p.host.grid(), cursor(), p.host.modes(), "", p.host.blocks());
        p.seq += 1;
        p.app.apply_keyframe(&mut p.client, &k, p.seq);
        let ids: Vec<u32> = p.client.blocks().blocks().iter().map(|b| b.id.0).collect();
        assert_eq!(ids, [1], "only the live prompt block survives");
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
    fn a_command_run_after_total_eviction_reaches_a_fresh_client() {
        // Issue #204's checklist item. Losing the last block lifts the
        // watermark to where the next block will appear, and the two must not
        // cross: a floor above a live block's id would tell every client the
        // block sits in un-vouched-for territory.
        let mut host = Terminal::new(20, 3, 4);
        host.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07one\x1b]133;C\x07\r\nout\r\n\x1b]133;D;0\x07");
        for _ in 0..20 {
            host.advance(b"filler\r\n");
        }
        assert!(host.blocks().blocks().is_empty(), "every block fell out of scrollback");

        host.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07two\x1b]133;C\x07\r\nout2\r\n\x1b]133;D;0\x07");
        let live = host.blocks().last().expect("the new command has a block").id.0;
        assert!(
            host.blocks().authoritative_from() <= live,
            "the watermark left by total eviction must not overshoot the next real block"
        );

        let mut enc = Encoder::new();
        let k = enc.keyframe(host.grid(), cursor(), host.modes(), "", host.blocks());
        let mut client = Terminal::new(20, 3, 100);
        let mut app = Applier::new();
        app.apply_keyframe(&mut client, &k, 1);
        assert_eq!(
            client.blocks().blocks().iter().map(|b| b.id.0).collect::<Vec<_>>(),
            [live],
            "a client attaching after the session's whole history was evicted \
             still sees the command that ran afterwards"
        );
    }

    #[test]
    fn eviction_cannot_disguise_a_clear_as_history_falling_away() {
        // The overshoot #204 is about. `cls` destroys the on-screen blocks and
        // lowers the watermark so the next keyframe announces it; if the
        // scrollback then evicts the last surviving block before the encoder
        // looks, the floor used to jump to `next_id` — over the destroyed ids —
        // and the announcement never went out. Every attached client kept
        // painting headers for commands whose rows no longer exist.
        let mut host = Terminal::new(20, 4, 4);
        // Block 0, pushed wholly into scrollback by the filler...
        host.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07old\x1b]133;C\x07\r\nout\r\n\x1b]133;D;0\x07");
        host.advance(b"f1\r\nf2\r\nf3\r\n");
        // ...and blocks 1 and 2 on the live screen.
        host.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07one\x1b]133;C\x07\r\n\x1b]133;D;0\x07");
        host.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07two\x1b]133;C\x07\r\n\x1b]133;D;0\x07");
        assert_eq!(host.blocks().blocks().len(), 3, "three commands ran");

        let mut enc = Encoder::new();
        let k = enc.keyframe(host.grid(), cursor(), host.modes(), "", host.blocks());
        let mut client = Terminal::new(20, 4, 100);
        let mut app = Applier::new();
        app.apply_keyframe(&mut client, &k, 1);
        assert_eq!(client.blocks().blocks().len(), 3, "the client holds all three");

        // One burst, before the encoder looks again: the clear destroys blocks
        // 1 and 2, and the output after it scrolls block 0 out entirely.
        host.advance(b"\x1b[2J\x1b[H");
        for _ in 0..12 {
            host.advance(b"x\r\n");
        }
        assert!(host.blocks().blocks().is_empty(), "destroyed and evicted, nothing left");

        assert!(
            enc.blocks_need_keyframe(host.blocks()),
            "the destroyed blocks still need announcing — eviction emptying the \
             index afterwards must not disguise them as evicted"
        );
        let k = enc.keyframe(host.grid(), cursor(), host.modes(), "", host.blocks());
        app.apply_keyframe(&mut client, &k, 2);
        assert_eq!(
            client.blocks().blocks().iter().map(|b| b.id.0).collect::<Vec<_>>(),
            [0],
            "the destroyed blocks go; the merely-evicted one stays with the client"
        );
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

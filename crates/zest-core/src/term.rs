//! The terminal: a VT interpreter driving a [`Grid`].
//!
//! Bytes in, grid mutations out. No I/O, no rendering, no allocation on the
//! steady-state path.

use alloc::string::String;
use alloc::vec::Vec;

use unicode_width::UnicodeWidthChar;

use crate::blocks::BlockIndex;
use crate::cell::{Cell, CellFlags};
use crate::color::Color;
use crate::grid::{Cursor, Grid, LineId, SavedCursor};
use crate::modes::{CursorStyle, Modes};
use crate::palette::{Palette, PaletteSnapshot, Rgb};

/// How deep a kitty keyboard flag stack goes before the oldest entry is evicted.
///
/// The protocol leaves this to the terminal and asks only that it be bounded,
/// because an unbounded push is a denial of service one escape sequence long.
/// Real programs push once on entry and pop once on exit; kitty itself uses the
/// same order of magnitude.
const KITTY_STACK_DEPTH: usize = 8;

/// Things the terminal needs the host to do, which it cannot do itself.
///
/// Returned rather than performed because `zest-core` has no I/O: the host owns
/// the PTY. Collecting them keeps the crate testable and lets the M3 daemon
/// route replies over a socket instead of a pipe without any change here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TermEvent {
    /// Write these bytes back to the PTY (a DSR/DA/OSC-query reply).
    Reply(Vec<u8>),
    /// OSC 0/2: the window title changed.
    Title(String),
    /// The bell rang.
    Bell,
    /// The cursor's shape or blink changed (DECSCUSR).
    CursorStyle(CursorStyle),
    /// A synchronized-output block began or ended (DEC 2026). While begun, the
    /// frontend should withhold frames so the repaint appears atomically.
    SyncUpdate(bool),
    /// OSC 8: a hyperlink was opened (`Some`) or closed (`None`).
    Hyperlink(Option<String>),
    /// A restating pty's repaint closed and the viewport/scrollback boundary
    /// moved to give back what its grow displaced (`Grid::settle_restate`).
    ///
    /// Every subscriber needs a keyframe: rows that were history are on screen
    /// now, and there is no delta that says so — `docs/CONTRACTS.md` has why
    /// there is no `DeltaOp::Resize`, and this is the same argument. (#247)
    ViewportRebased,
    /// ED 3 destroyed the primary grid's scrollback (`Grid::clear_history`).
    ///
    /// The `ViewportRebased` argument one notch further: the rows are not
    /// damaged, they are *gone*, and no delta can say so. Every subscriber is
    /// owed a keyframe — and the keyframe carries `Grid::history_clears` too,
    /// so a client that misses this one (a reconnect, a phone that slept)
    /// still learns, and a client deliberately holding more history than the
    /// host still drops it. Announced destruction, as opposed to eviction,
    /// which stays silent on purpose (`docs/CONTRACTS.md`). (#314)
    HistoryCleared,
}

/// Damage accumulated since the last frame.
///
/// The point of tracking this is *deciding whether to render at all*. An idle
/// terminal must use 0% GPU, which is a hard requirement rather than a nicety,
/// and it is what separates a real terminal from a demo.
#[derive(Debug, Clone, Default)]
pub struct Damage {
    /// Anything at all changed.
    pub dirty: bool,
    /// The full screen needs repainting (scroll, resize, screen switch).
    pub full: bool,
}

impl Damage {
    pub(crate) fn mark(&mut self) {
        self.dirty = true;
    }
    pub(crate) fn mark_full(&mut self) {
        self.dirty = true;
        self.full = true;
    }
}

/// A VT-parsing terminal.
pub struct Terminal {
    pub(crate) parser: vte::Parser,
    pub(crate) state: TermState,
}

/// The mutable terminal state, separated from the parser so `vte::Perform` can
/// borrow it mutably while the parser drives.
pub struct TermState {
    pub(crate) grid: Grid,
    /// The alternate screen, created lazily -- most sessions never enter it.
    pub(crate) alt_grid: Option<Grid>,
    /// Kept here as well as on each grid because the alt grid is built later:
    /// a flag set once at spawn has to reach a screen that does not exist yet.
    /// See [`crate::grid::Grid::set_viewport_restated_elsewhere`].
    pub(crate) viewport_restated_elsewhere: bool,
    /// Current SGR state, used as the template for newly written cells.
    pub(crate) template: Cell,
    pub(crate) saved_cursor: SavedCursor,
    pub(crate) saved_cursor_alt: SavedCursor,
    /// The kitty keyboard flag stack for the main screen.
    ///
    /// Two stacks, one per screen, because the protocol requires it: a
    /// full-screen program pushes its flags on entry and pops them on exit, and
    /// sharing one stack would leave the shell holding whatever `nvim` was
    /// using after a crash. The same reason `saved_cursor_alt` exists.
    ///
    /// The stack is host state and never crosses the wire — only the top
    /// matters to an encoder, and that rides in `modes`.
    pub(crate) kitty_stack: Vec<u8>,
    pub(crate) kitty_stack_alt: Vec<u8>,
    pub(crate) modes: Modes,
    pub(crate) palette: Palette,
    pub(crate) tabs: Vec<bool>,
    pub(crate) events: Vec<TermEvent>,
    pub(crate) damage: Damage,
    /// Bumped on every mutation.
    ///
    /// M3 computes grid deltas against a per-subscriber baseline sequence; this
    /// is that sequence. Free to maintain now, awkward to add later.
    pub(crate) seq: u64,
    pub(crate) title: String,
    pub(crate) cursor_style: CursorStyle,
    /// Whether a program has set the cursor style with DECSCUSR.
    ///
    /// Tracked explicitly rather than inferred from `cursor_style !=
    /// default_cursor_style`, which is the same value for two different
    /// situations: a program that asks for exactly the shape the default
    /// already is (`CSI 1 SP q` against a blinking block) looks untouched by
    /// equality, and a config reload would then quietly take its cursor away.
    /// Provenance is not recoverable from a value.
    pub(crate) cursor_style_from_program: bool,
    /// What `DECSCUSR 0` resets to.
    ///
    /// `cursor.shape` in the settings tree, which the schema documents as the
    /// shape used *"unless the program sets one with DECSCUSR"*. Reset is what
    /// makes that true rather than merely initial: a program that sets a bar
    /// and then resets on exit must hand the terminal back to the user's
    /// choice, not to whatever this file happened to hardcode.
    pub(crate) default_cursor_style: CursorStyle,
    /// The active selection, if any.
    ///
    /// Lives on the terminal rather than the app because text extraction needs
    /// the grid, and because a future daemon has to serialize it to remote
    /// clients alongside everything else.
    pub(crate) selection: Option<crate::Selection>,
    /// The active OSC 8 hyperlink, if any.
    pub(crate) current_hyperlink: Option<u16>,
    pub(crate) next_hyperlink_id: u16,
    /// Command blocks, as the shell has reported them (OSC 133).
    ///
    /// A side table rather than cell data, for the reasons in
    /// [`crate::blocks`]. It lives here rather than on the app because a block
    /// is a fact about the session, and the daemon has to put it on the wire.
    pub(crate) blocks: BlockIndex,
    /// Working directory, from OSC 7. Stamped onto the next block.
    ///
    /// Held rather than pushed as an event because OSC 7 arrives *before* the
    /// prompt marker that consumes it, and because the daemon reports it in
    /// `SessionInfo` for a session nobody is attached to.
    pub(crate) cwd: String,
    /// Where the typed command starts, recorded at OSC 133;B.
    ///
    /// Transient parser state, not part of the index: `B` says "the prompt ends
    /// here", and only `C` knows whether anything was actually submitted. The
    /// column matters because a prompt does not end at column zero.
    pub(crate) prompt_end: Option<(LineId, usize)>,
    /// The command line as OSC 633;E stated it, awaiting the `C` that runs it.
    ///
    /// Preferred over reading the grid back, because it is what the shell will
    /// actually execute rather than what the screen happens to show — they
    /// differ whenever the prompt redraws, which `zsh`'s autosuggestions and
    /// syntax highlighting both do on every keystroke.
    pub(crate) pending_command: Option<String>,
    /// The embedder's wall clock, milliseconds since the Unix epoch, for
    /// stamping blocks. This crate has no clock (`no_std`), so whoever feeds
    /// bytes refreshes this first; stale by at most one read burst, which is
    /// precision to spare for "51.2s".
    pub(crate) now_ms: Option<u64>,
}

impl Terminal {
    #[must_use]
    pub fn new(cols: usize, rows: usize, scrollback: usize) -> Self {
        Self {
            parser: vte::Parser::new(),
            state: TermState::new(cols, rows, scrollback),
        }
    }

    /// Feed PTY output.
    ///
    /// Chunk boundaries are irrelevant: the parser is a state machine, so an
    /// escape sequence split across two reads is handled correctly.
    pub fn advance(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.state, bytes);
    }

    /// Whether the alternate screen is active.
    ///
    /// The blocks UI hides there: the alt screen is a separate grid whose
    /// line ids restart at zero, so a block anchored in the primary grid
    /// would overlay a full-screen program's rows at whatever ids happen to
    /// collide.
    #[must_use]
    pub fn in_alt_screen(&self) -> bool {
        self.state.alt_grid.is_some()
    }

    /// Tell the terminal what time it is, for stamping command blocks.
    ///
    /// Callers set this before [`Self::advance`]; the parser cannot ask an OS
    /// it is not allowed to know about. Never required — blocks simply carry
    /// no times when nobody says.
    pub fn set_now_ms(&mut self, ms: u64) {
        self.state.now_ms = Some(ms);
    }

    #[must_use]
    pub fn grid(&self) -> &Grid {
        self.state.grid()
    }

    #[must_use]
    pub fn modes(&self) -> Modes {
        self.state.modes
    }

    #[must_use]
    pub fn seq(&self) -> u64 {
        self.state.seq
    }

    #[must_use]
    pub fn title(&self) -> &str {
        &self.state.title
    }

    #[must_use]
    pub fn cursor(&self) -> Cursor {
        self.state.grid().cursor
    }

    #[must_use]
    pub fn cursor_style(&self) -> CursorStyle {
        self.state.cursor_style
    }

    /// Set the shape `DECSCUSR 0` resets to, and adopt it now if no program
    /// has asked for something else yet.
    ///
    /// Adopting immediately is what makes the setting apply to a session that
    /// is already open; a terminal that only honoured it on the next launch
    /// would be one more setting that does not apply. A session where a
    /// program *has* set a style keeps it — that program is still running and
    /// still means it.
    pub fn set_default_cursor_style(&mut self, style: CursorStyle) {
        self.state.default_cursor_style = style;
        if !self.state.cursor_style_from_program {
            self.state.cursor_style = style;
        }
    }

    #[must_use]
    pub fn palette(&self) -> &PaletteSnapshot {
        self.state.palette.live()
    }

    /// Re-seed the palette from a theme.
    pub fn set_palette(&mut self, seed: PaletteSnapshot) {
        self.state.palette.reseed(seed);
        self.state.damage.mark_full();
    }

    /// Take the pending host actions. The caller must perform them.
    pub fn take_events(&mut self) -> Vec<TermEvent> {
        core::mem::take(&mut self.state.events)
    }

    pub fn take_damage(&mut self) -> Damage {
        core::mem::take(&mut self.state.damage)
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        self.state.resize(cols, rows);
    }

    /// Tell the terminal its pty restates the viewport after a resize.
    ///
    /// Asked of the transport (`PtyTransport::restates_on_resize`) and passed
    /// on at spawn. Named after the pty deliberately, where the grid's
    /// [`crate::grid::Grid::set_viewport_restated_elsewhere`] is not: this is
    /// the only caller that has a pty in hand, and the grid has two callers
    /// with nothing in common but the consequence. A replica sets the same flag
    /// through `Terminal::remote`, and a name that described a pty would have
    /// gone on hiding that one — which is how it stayed hidden. (#247)
    pub fn set_pty_restates_viewport(&mut self, yes: bool) {
        self.state.set_viewport_restated_elsewhere(yes);
    }

    /// The visible screen as text. The workhorse of the test suite.
    #[must_use]
    pub fn screen_text(&self) -> String {
        self.state.grid().screen_text()
    }

    pub fn scroll_display(&mut self, delta: isize) {
        self.state.grid_mut().scroll_display(delta);
        self.state.damage.mark_full();
    }

    pub fn scroll_to_bottom(&mut self) {
        self.state.grid_mut().scroll_to_bottom();
        self.state.damage.mark_full();
    }

    /// The command blocks the shell has reported.
    ///
    /// Empty unless the shell emits OSC 133 — which is what shell integration
    /// installs. A terminal without it is not degraded, it simply has no
    /// semantic view of its own scrollback.
    ///
    /// Reachable identically for a local session and one running on another
    /// machine: a remote `Terminal` has these applied into it from the wire, so
    /// `SessionSource::terminal().lock().blocks()` answers at both ends of the
    /// mesh.
    #[must_use]
    pub fn blocks(&self) -> &BlockIndex {
        &self.state.blocks
    }

    /// The shell's working directory, from OSC 7.
    ///
    /// Empty until a shell reports one. Guessing from the child's `/proc` entry
    /// or `lsof` would be answering a different question — where the *process*
    /// is, not where the next command will run, which differ the moment a
    /// subshell is involved.
    #[must_use]
    pub fn cwd(&self) -> &str {
        &self.state.cwd
    }

    #[must_use]
    pub fn selection(&self) -> Option<crate::Selection> {
        self.state.selection
    }

    pub fn set_selection(&mut self, selection: Option<crate::Selection>) {
        if self.state.selection != selection {
            self.state.selection = selection;
            self.state.damage.mark_full();
        }
    }

    /// The selected text, or `None` when nothing is selected.
    ///
    /// `None` rather than an empty string so a stray click cannot clobber the
    /// clipboard.
    #[must_use]
    pub fn selection_text(&self) -> Option<String> {
        let sel = self.state.selection?;
        let text = self.state.grid().selection_text(&sel);
        (!text.is_empty()).then_some(text)
    }

    /// Translate a viewport cell to an absolute position.
    ///
    /// Returns `None` for a row outside the viewport, which happens when a drag
    /// leaves the window.
    #[must_use]
    pub fn abs_pos(&self, row: usize, col: usize) -> Option<crate::AbsPos> {
        let grid = self.state.grid();
        grid.line_id_at(row.min(grid.rows().saturating_sub(1)))
            .map(|line| crate::AbsPos::new(line, col.min(grid.cols().saturating_sub(1))))
    }

    /// Expand a position to the word around it, for double-click.
    #[must_use]
    pub fn word_at(&self, pos: crate::AbsPos) -> (crate::AbsPos, crate::AbsPos) {
        self.state.grid().word_at(pos)
    }

    /// Wrap pasted text in bracketed-paste markers when the program asked for
    /// them.
    ///
    /// Without this, pasting multi-line text into a shell executes every line,
    /// and pasting into vim in insert mode triggers autoindent on each one.
    /// Programs enable mode 2004 precisely so they can tell a paste from typing.
    #[must_use]
    pub fn encode_paste(&self, text: &str) -> Vec<u8> {
        // Normalize line endings: a pty expects CR, and pasted text routinely
        // carries CRLF from a Windows clipboard.
        let normalized: String = text.replace("\r\n", "\r").replace('\n', "\r");

        if self.state.modes.contains(Modes::BRACKETED_PASTE) {
            let mut out = Vec::with_capacity(normalized.len() + 12);
            out.extend_from_slice(b"\x1b[200~");
            out.extend_from_slice(normalized.as_bytes());
            out.extend_from_slice(b"\x1b[201~");
            out
        } else {
            normalized.into_bytes()
        }
    }
}

impl TermState {
    pub(crate) fn new(cols: usize, rows: usize, scrollback: usize) -> Self {
        let mut palette_seed = PaletteSnapshot {
            colors: [Rgb::default(); 256],
            foreground: Rgb::new(0xd7, 0xdc, 0xea),
            background: Rgb::new(0x0b, 0x0f, 0x1a),
            cursor: Rgb::new(0x6e, 0xa8, 0xff),
        };
        palette_seed.fill_standard_extended();

        Self {
            grid: Grid::new(cols, rows, scrollback),
            alt_grid: None,
            viewport_restated_elsewhere: false,
            template: Cell::default(),
            saved_cursor: SavedCursor::default(),
            saved_cursor_alt: SavedCursor::default(),
            kitty_stack: Vec::new(),
            kitty_stack_alt: Vec::new(),
            modes: Modes::initial(),
            palette: Palette::new(palette_seed),
            tabs: default_tabs(cols),
            events: Vec::new(),
            damage: Damage::default(),
            seq: 0,
            title: String::new(),
            cursor_style: CursorStyle::default(),
            default_cursor_style: CursorStyle::default(),
            cursor_style_from_program: false,
            selection: None,
            current_hyperlink: None,
            next_hyperlink_id: 0,
            blocks: BlockIndex::new(),
            cwd: String::new(),
            prompt_end: None,
            pending_command: None,
            now_ms: None,
        }
    }

    pub(crate) fn grid(&self) -> &Grid {
        self.alt_grid.as_ref().unwrap_or(&self.grid)
    }

    pub(crate) fn grid_mut(&mut self) -> &mut Grid {
        self.alt_grid.as_mut().unwrap_or(&mut self.grid)
    }

    pub(crate) fn touch(&mut self) {
        self.seq += 1;
        self.damage.mark();
    }

    pub(crate) fn touch_full(&mut self) {
        self.seq += 1;
        self.damage.mark_full();
    }

    /// Set the flag on every grid this terminal has, and on the ones it has not
    /// built yet. See the field.
    pub(crate) fn set_viewport_restated_elsewhere(&mut self, yes: bool) {
        self.viewport_restated_elsewhere = yes;
        self.grid.set_viewport_restated_elsewhere(yes);
        if let Some(alt) = self.alt_grid.as_mut() {
            alt.set_viewport_restated_elsewhere(yes);
        }
    }

    pub(crate) fn resize(&mut self, cols: usize, rows: usize) {
        // A width change rewraps, which renumbers lines -- so a selection
        // anchored to the old ids now names different text. Clearing it is what
        // every terminal does, and is far better than highlighting whatever
        // happens to be at those coordinates afterwards.
        if cols != self.grid.cols() {
            self.selection = None;
        }
        let template = self.template;
        let reindex = self.grid.resize(cols, rows, &template);
        if let Some(alt) = self.alt_grid.as_mut() {
            // The alternate screen never reflows and carries no blocks, so its
            // reindex is always empty. Dropped rather than merged.
            let _ = alt.resize(cols, rows, &template);
        }
        // Blocks are re-anchored rather than cleared, unlike the selection
        // above. Losing the block for a build because the window was widened
        // while it ran is exactly the case blocks exist for -- and a block
        // names only the line it began on, which the rewrap can still answer
        // for, where a selection names a column too.
        if !reindex.is_empty() {
            self.blocks.reanchor(&reindex);
            self.prompt_end = None;
        }
        self.tabs = default_tabs(cols);
        self.touch_full();
    }

    /// Pay back what a restating pty's grow owed, now that its repaint has
    /// closed. See [`crate::grid::Grid::settle_restate`].
    ///
    /// The blocks need no re-anchoring: a height change renumbers nothing, so
    /// the rows coming back into the viewport carry the very ids the blocks
    /// anchored on before the drag started, and they name their own output
    /// again by arriving.
    ///
    /// The *active* grid, matching where the latch was armed. Settling the
    /// primary unconditionally instead reads as harmless — the alt screen has no
    /// scrollback to give back — and is not: with a full-screen program up, the
    /// alt grid's latch is never cleared, so every DECTCEM change afterwards
    /// retries the settle, and it retries it against the primary grid, whose
    /// debt belongs to a resize the repaint in hand knows nothing about.
    pub(crate) fn settle_restate(&mut self) {
        if !self.grid_mut().settle_restate() {
            return;
        }
        // The viewport/scrollback boundary moved, which is the one change a
        // delta cannot describe -- there is no `DeltaOp::Resize`, on purpose
        // (`docs/CONTRACTS.md`). Subscribers need a whole new picture.
        self.events.push(TermEvent::ViewportRebased);
        self.touch_full();
    }

    /// End the restore before ordinary output lands on it.
    ///
    /// Called from every content op the parser can drive — a print, a
    /// linefeed, a cursor move — and a no-op everywhere but the one moment it
    /// exists for: the first output after a settle, when the restater still
    /// addresses the screen in its own coordinates, offset by the pull. See
    /// [`crate::grid::Grid::strand_settled`]. The restatement's own opening
    /// home never reaches this: the bracket opens first, and an open bracket
    /// is not ordinary output. (#341)
    pub(crate) fn strand_if_diverged(&mut self) {
        if !self.grid().needs_strand() {
            return;
        }
        let t = self.template;
        if self.grid_mut().strand_settled(&t) {
            // The boundary moved: the same keyframe debt as a settle.
            self.events.push(TermEvent::ViewportRebased);
            self.touch_full();
        }
    }

    // --- writing ---------------------------------------------------------

    pub(crate) fn write_char(&mut self, ch: char) {
        self.strand_if_diverged();
        let width = ch.width().unwrap_or(0);

        // Zero-width: a combining mark attaches to the previous cell rather
        // than occupying one of its own.
        if width == 0 {
            let (row, col) = (self.grid().cursor.row, self.grid().cursor.col);
            if col > 0 {
                self.grid_mut().row_mut(row).push_zerowidth(col - 1, ch);
                self.touch();
            }
            return;
        }

        let cols = self.grid().cols();

        // Resolve a deferred wrap now that a character has actually arrived.
        if self.grid().cursor.pending_wrap && self.modes.contains(Modes::AUTO_WRAP) {
            let row = self.grid().cursor.row;
            self.grid_mut().set_wrapped(row, true);
            self.linefeed();
            self.grid_mut().cursor.col = 0;
            self.grid_mut().cursor.pending_wrap = false;
        }

        // A double-width character will not straddle the right edge; wrap first.
        if width == 2 && self.grid().cursor.col + 1 >= cols {
            if self.modes.contains(Modes::AUTO_WRAP) {
                let row = self.grid().cursor.row;
                self.grid_mut().set_wrapped(row, true);
                self.linefeed();
                self.grid_mut().cursor.col = 0;
            } else {
                return;
            }
        }

        if self.modes.contains(Modes::INSERT) {
            let template = self.template;
            self.grid_mut().insert_cells(width, &template);
        }

        let (row, col) = (self.grid().cursor.row, self.grid().cursor.col);
        let mut cell = self.template;
        cell.ch = ch;
        if width == 2 {
            cell.flags |= CellFlags::WIDE;
        }
        if let Some(hl) = self.current_hyperlink {
            let _ = hl; // wired to the side table in M2, when links become clickable
        }

        if let Some(dst) = self.grid_mut().cell_mut(row, col) {
            *dst = cell;
        }
        if width == 2 {
            let mut spacer = self.template;
            spacer.ch = ' ';
            spacer.flags |= CellFlags::WIDE_SPACER;
            if let Some(dst) = self.grid_mut().cell_mut(row, col + 1) {
                *dst = spacer;
            }
        }

        // Advance, deferring the wrap until the next character.
        let next = col + width;
        if next >= cols {
            self.grid_mut().cursor.col = cols - 1;
            self.grid_mut().cursor.pending_wrap = true;
        } else {
            self.grid_mut().cursor.col = next;
        }
        self.touch();
    }

    /// The oldest line still held, scrollback included.
    ///
    /// The same expression as [`crate::ChangeSource::oldest_line`], but read
    /// off the *primary* grid deliberately: blocks are numbered there, and
    /// while a full-screen program is up `grid()` answers for the alternate
    /// screen, whose ids restart at zero.
    fn oldest_retained_line(&self) -> LineId {
        // Read off the oldest row rather than counted back from the top of the
        // live screen. Counting is only right while the ids are contiguous, and
        // `truncate_bottom` leaves gaps — see `Grid::oldest_line_id`. It also
        // makes the display irrelevant, which the count needed active space to
        // achieve: the oldest row held is the oldest row held wherever anyone
        // happens to be looking.
        self.grid.oldest_line_id()
    }

    /// Drop blocks whose lines have all fallen out of scrollback.
    ///
    /// Lives here rather than in `Grid` because the grid cannot see the index —
    /// eviction inside the ring is silent by design, and nothing else needs to
    /// be told about it.
    ///
    /// **The early return is the point.** This runs once per scrolled line, and
    /// a `Vec::retain` for every row a `cargo build` prints would be a linear
    /// scan of the index per line of output. Only the *oldest* block can be the
    /// first to become evictable, so the common case is one comparison.
    pub(crate) fn evict_blocks(&mut self) {
        let oldest = self.oldest_retained_line();
        let evictable = self
            .blocks
            .blocks()
            .first()
            .is_some_and(|b| b.end_line.is_some_and(|end| end < oldest));
        if evictable {
            self.blocks.evict_before(oldest);
        }
    }

    pub(crate) fn linefeed(&mut self) {
        self.strand_if_diverged();
        let bottom = self.grid().region.bottom;
        if self.grid().cursor.row == bottom {
            let template = self.template;
            self.grid_mut().scroll_up(1, &template);
            self.evict_blocks();
            self.touch_full();
        } else if self.grid().cursor.row + 1 < self.grid().rows() {
            self.grid_mut().cursor.row += 1;
            self.touch();
        }
        self.grid_mut().cursor.pending_wrap = false;
    }

    pub(crate) fn carriage_return(&mut self) {
        self.grid_mut().cursor.col = 0;
        self.grid_mut().cursor.pending_wrap = false;
        self.touch();
    }

    pub(crate) fn backspace(&mut self) {
        let col = self.grid().cursor.col;
        if col > 0 {
            self.grid_mut().cursor.col = col - 1;
        }
        self.grid_mut().cursor.pending_wrap = false;
        self.touch();
    }

    pub(crate) fn tab(&mut self, count: usize) {
        let cols = self.grid().cols();
        let mut col = self.grid().cursor.col;
        for _ in 0..count.max(1) {
            col = (col + 1..cols).find(|&c| self.tabs.get(c).copied().unwrap_or(false))
                .unwrap_or(cols - 1);
        }
        self.grid_mut().cursor.col = col;
        self.grid_mut().cursor.pending_wrap = false;
        self.touch();
    }

    // --- cursor ----------------------------------------------------------

    /// Move the cursor, honoring origin mode (DECOM), which makes addressing
    /// relative to the scroll region.
    pub(crate) fn goto(&mut self, row: usize, col: usize) {
        self.strand_if_diverged();
        let (rows, cols) = (self.grid().rows(), self.grid().cols());
        let (min_row, max_row) = if self.modes.contains(Modes::ORIGIN) {
            (self.grid().region.top, self.grid().region.bottom)
        } else {
            (0, rows - 1)
        };
        let row = (row + min_row).clamp(min_row, max_row);
        self.grid_mut().cursor.row = row;
        self.grid_mut().cursor.col = col.min(cols - 1);
        self.grid_mut().cursor.pending_wrap = false;
        self.touch();
    }

    pub(crate) fn move_up(&mut self, n: usize) {
        let top = if self.modes.contains(Modes::ORIGIN) { self.grid().region.top } else { 0 };
        let row = self.grid().cursor.row.saturating_sub(n.max(1)).max(top);
        self.grid_mut().cursor.row = row;
        self.grid_mut().cursor.pending_wrap = false;
        self.touch();
    }

    pub(crate) fn move_down(&mut self, n: usize) {
        let bottom = if self.modes.contains(Modes::ORIGIN) {
            self.grid().region.bottom
        } else {
            self.grid().rows() - 1
        };
        let row = (self.grid().cursor.row + n.max(1)).min(bottom);
        self.grid_mut().cursor.row = row;
        self.grid_mut().cursor.pending_wrap = false;
        self.touch();
    }

    pub(crate) fn move_left(&mut self, n: usize) {
        let col = self.grid().cursor.col.saturating_sub(n.max(1));
        self.grid_mut().cursor.col = col;
        self.grid_mut().cursor.pending_wrap = false;
        self.touch();
    }

    pub(crate) fn move_right(&mut self, n: usize) {
        let cols = self.grid().cols();
        let col = (self.grid().cursor.col + n.max(1)).min(cols - 1);
        self.grid_mut().cursor.col = col;
        self.grid_mut().cursor.pending_wrap = false;
        self.touch();
    }

    pub(crate) fn save_cursor(&mut self) {
        let saved = SavedCursor {
            cursor: self.grid().cursor,
            template: self.template,
            origin_mode: self.modes.contains(Modes::ORIGIN),
        };
        if self.alt_grid.is_some() {
            self.saved_cursor_alt = saved;
        } else {
            self.saved_cursor = saved;
        }
    }

    pub(crate) fn restore_cursor(&mut self) {
        let saved = if self.alt_grid.is_some() { self.saved_cursor_alt } else { self.saved_cursor };
        self.template = saved.template;
        self.modes.set(Modes::ORIGIN, saved.origin_mode);
        let (rows, cols) = (self.grid().rows(), self.grid().cols());
        self.grid_mut().cursor = Cursor {
            row: saved.cursor.row.min(rows - 1),
            col: saved.cursor.col.min(cols - 1),
            pending_wrap: false,
        };
        self.touch();
    }

    // --- screen switching -------------------------------------------------

    pub(crate) fn set_alt_screen(&mut self, enable: bool) {
        if enable == self.alt_grid.is_some() {
            return;
        }
        if enable {
            let (cols, rows) = (self.grid.cols(), self.grid.rows());
            // The alternate screen has no scrollback -- by design. Programs
            // using it own the whole display and their content is not history.
            let mut alt = Grid::new(cols, rows, 0);
            alt.clear_all(&self.template);
            // Whoever restates the primary grid's viewport restates this one
            // too. It changes nothing today -- with no scrollback there is
            // nothing for a grow to pull back, so both branches coincide -- and
            // depending on that is how the two grids quietly diverge the first
            // time the alt screen gains history.
            alt.set_viewport_restated_elsewhere(self.viewport_restated_elsewhere);
            self.alt_grid = Some(alt);
            self.modes |= Modes::ALT_SCREEN;
        } else {
            self.alt_grid = None;
            self.modes -= Modes::ALT_SCREEN;
        }
        // The two screens keep separate keyboard flags, so switching screens
        // switches which ones are live. Without this the shell inherits the
        // flags of the program that just exited and every keystroke encodes
        // wrongly until something resets them.
        self.sync_kitty_modes();
        self.touch_full();
    }

    // --- the Kitty keyboard flag stack ------------------------------------

    /// The flag stack belonging to the screen currently displayed.
    fn kitty_stack_mut(&mut self) -> &mut Vec<u8> {
        if self.alt_grid.is_some() {
            &mut self.kitty_stack_alt
        } else {
            &mut self.kitty_stack
        }
    }

    /// Copy the top of the active stack into the mode word.
    ///
    /// The stack is the truth; `modes` is the projection of it that reaches a
    /// client. Every mutation ends here so the two cannot disagree.
    ///
    /// **The `touch` is load-bearing and its absence is invisible locally.**
    /// Deltas are computed against `seq` (`subscribe::update_for`), so flags
    /// that change without bumping it never reach an attached client, which
    /// then keeps encoding every keystroke the legacy way at a program that has
    /// stopped expecting it. A window driving a pty in this process reads
    /// `modes` off the terminal directly and looks perfectly correct.
    fn sync_kitty_modes(&mut self) {
        let flags = if self.alt_grid.is_some() {
            self.kitty_stack_alt.last().copied()
        } else {
            self.kitty_stack.last().copied()
        }
        .unwrap_or(0);
        let next = self.modes.with_kitty_flags(flags);
        // Conditional so a bare `CSI ? u` query -- which programs send on
        // startup and change nothing -- does not wake every subscriber.
        if next != self.modes {
            self.modes = next;
            self.touch();
        }
    }

    /// The flags in force, as the protocol numbers them.
    pub(crate) fn kitty_flags(&self) -> u8 {
        self.modes.kitty_flags()
    }

    pub(crate) fn kitty_push(&mut self, flags: u8) {
        let stack = self.kitty_stack_mut();
        // A program that pushes and never pops is a program that leaks memory
        // in someone else's process, so the depth is bounded and the oldest
        // entry is evicted. The protocol asks for exactly this.
        if stack.len() == KITTY_STACK_DEPTH {
            stack.remove(0);
        }
        stack.push(flags & Modes::KITTY_SUPPORTED);
        self.sync_kitty_modes();
    }

    pub(crate) fn kitty_pop(&mut self, count: usize) {
        let stack = self.kitty_stack_mut();
        for _ in 0..count {
            if stack.pop().is_none() {
                break;
            }
        }
        self.sync_kitty_modes();
    }

    /// `CSI = flags ; mode u` — modify the flags in force without pushing.
    pub(crate) fn kitty_set(&mut self, flags: u8, mode: u16) {
        let flags = flags & Modes::KITTY_SUPPORTED;
        let current = self.kitty_flags();
        let next = match mode {
            2 => current | flags,
            3 => current & !flags,
            // Mode 1 is the default, and so is anything unrecognized: the
            // protocol defines 1..=3 and says nothing about the rest.
            _ => flags,
        };
        let stack = self.kitty_stack_mut();
        // Setting with an empty stack has to create the entry it modifies, or
        // the flags a program asked for vanish the moment it asks for them.
        match stack.last_mut() {
            Some(top) => *top = next,
            None => stack.push(next),
        }
        self.sync_kitty_modes();
    }

    /// Drop both stacks, for RIS.
    pub(crate) fn kitty_reset(&mut self) {
        self.kitty_stack.clear();
        self.kitty_stack_alt.clear();
        self.sync_kitty_modes();
    }

    // --- SGR --------------------------------------------------------------

    pub(crate) fn apply_sgr(&mut self, params: &vte::Params) {
        let mut iter = params.iter();
        while let Some(param) = iter.next() {
            match param {
                [0] => self.template = Cell::default(),
                [1] => self.template.flags |= CellFlags::BOLD,
                [2] => self.template.flags |= CellFlags::DIM,
                [3] => self.template.flags |= CellFlags::ITALIC,
                // `4:3` is undercurl; bare `4` is a plain underline. The
                // subparameter form is why the parser must support `:`.
                [4] => self.template.flags |= CellFlags::UNDERLINE,
                [4, 0] => {
                    self.template.flags -= CellFlags::UNDERLINE
                        | CellFlags::DOUBLE_UNDERLINE
                        | CellFlags::UNDERCURL;
                }
                [4, 1] => self.template.flags |= CellFlags::UNDERLINE,
                [4, 2] => self.template.flags |= CellFlags::DOUBLE_UNDERLINE,
                [4, 3] => self.template.flags |= CellFlags::UNDERCURL,
                [4, _] => self.template.flags |= CellFlags::UNDERLINE,
                [5] | [6] => self.template.flags |= CellFlags::BLINK,
                [7] => self.template.flags |= CellFlags::INVERSE,
                [8] => self.template.flags |= CellFlags::HIDDEN,
                [9] => self.template.flags |= CellFlags::STRIKEOUT,
                [21] => self.template.flags |= CellFlags::DOUBLE_UNDERLINE,
                [22] => self.template.flags -= CellFlags::BOLD | CellFlags::DIM,
                [23] => self.template.flags -= CellFlags::ITALIC,
                [24] => {
                    self.template.flags -= CellFlags::UNDERLINE
                        | CellFlags::DOUBLE_UNDERLINE
                        | CellFlags::UNDERCURL;
                }
                [25] => self.template.flags -= CellFlags::BLINK,
                [27] => self.template.flags -= CellFlags::INVERSE,
                [28] => self.template.flags -= CellFlags::HIDDEN,
                [29] => self.template.flags -= CellFlags::STRIKEOUT,
                [n @ 30..=37] => self.template.fg = Color::Indexed(*n as u8 - 30),
                [n @ 40..=47] => self.template.bg = Color::Indexed(*n as u8 - 40),
                // 90-97 and 100-107 are the bright variants, which live at
                // palette indices 8-15.
                [n @ 90..=97] => self.template.fg = Color::Indexed(*n as u8 - 90 + 8),
                [n @ 100..=107] => self.template.bg = Color::Indexed(*n as u8 - 100 + 8),
                [39] => self.template.fg = Color::Default,
                [49] => self.template.bg = Color::Default,
                // Extended color. Both the `38;2;r;g;b` and `38:2::r:g:b`
                // spellings occur in the wild.
                [38, rest @ ..] => {
                    if let Some(c) = parse_extended_color(rest, &mut iter) {
                        self.template.fg = c;
                    }
                }
                [48, rest @ ..] => {
                    if let Some(c) = parse_extended_color(rest, &mut iter) {
                        self.template.bg = c;
                    }
                }
                // 58/59 set the underline color; parsed and ignored for now so
                // the sequence does not corrupt the following parameters.
                [58, rest @ ..] => {
                    let _ = parse_extended_color(rest, &mut iter);
                }
                [59] => {}
                _ => {}
            }
        }
        self.touch();
    }
}

/// Parse the tail of an SGR 38/48/58 extended-color parameter.
///
/// Handles the two spellings that occur in practice: subparameters
/// (`38:2::r:g:b`, `38:5:n`) where everything arrives in one group, and the
/// legacy semicolon form (`38;2;r;g;b`) where the parts arrive as separate
/// groups and must be pulled from the iterator.
fn parse_extended_color(rest: &[u16], iter: &mut vte::ParamsIter<'_>) -> Option<Color> {
    let mut next = |head: Option<u16>| -> Option<u16> {
        head.or_else(|| iter.next().and_then(|p| p.first().copied()))
    };

    match next(rest.first().copied())? {
        2 => {
            // `38:2::r:g:b` carries an empty colorspace slot; skip a leading
            // zero-from-empty when there are enough components for it.
            let (r, g, b) = if rest.len() >= 5 {
                (rest[2], rest[3], rest[4])
            } else if rest.len() == 4 {
                (rest[1], rest[2], rest[3])
            } else {
                (next(None)?, next(None)?, next(None)?)
            };
            Some(Color::Rgb(r as u8, g as u8, b as u8))
        }
        5 => {
            let idx = if rest.len() >= 2 { rest[1] } else { next(None)? };
            Some(Color::Indexed(idx as u8))
        }
        _ => None,
    }
}

fn default_tabs(cols: usize) -> Vec<bool> {
    (0..cols).map(|c| c > 0 && c % 8 == 0).collect()
}


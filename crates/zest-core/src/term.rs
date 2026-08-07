//! The terminal: a VT interpreter driving a [`Grid`].
//!
//! Bytes in, grid mutations out. No I/O, no rendering, no allocation on the
//! steady-state path.

use alloc::string::String;
use alloc::vec::Vec;

use unicode_width::UnicodeWidthChar;

use crate::cell::{Cell, CellFlags};
use crate::color::Color;
use crate::grid::{Cursor, Grid, SavedCursor};
use crate::modes::{CursorStyle, Modes};
use crate::palette::{Palette, PaletteSnapshot, Rgb};

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
    /// Current SGR state, used as the template for newly written cells.
    pub(crate) template: Cell,
    pub(crate) saved_cursor: SavedCursor,
    pub(crate) saved_cursor_alt: SavedCursor,
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
    /// The active selection, if any.
    ///
    /// Lives on the terminal rather than the app because text extraction needs
    /// the grid, and because a future daemon has to serialize it to remote
    /// clients alongside everything else.
    pub(crate) selection: Option<crate::Selection>,
    /// The active OSC 8 hyperlink, if any.
    pub(crate) current_hyperlink: Option<u16>,
    pub(crate) next_hyperlink_id: u16,
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
            template: Cell::default(),
            saved_cursor: SavedCursor::default(),
            saved_cursor_alt: SavedCursor::default(),
            modes: Modes::initial(),
            palette: Palette::new(palette_seed),
            tabs: default_tabs(cols),
            events: Vec::new(),
            damage: Damage::default(),
            seq: 0,
            title: String::new(),
            cursor_style: CursorStyle::default(),
            selection: None,
            current_hyperlink: None,
            next_hyperlink_id: 0,
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

    pub(crate) fn resize(&mut self, cols: usize, rows: usize) {
        let template = self.template;
        self.grid.resize(cols, rows, &template);
        if let Some(alt) = self.alt_grid.as_mut() {
            alt.resize(cols, rows, &template);
        }
        self.tabs = default_tabs(cols);
        self.touch_full();
    }

    // --- writing ---------------------------------------------------------

    pub(crate) fn write_char(&mut self, ch: char) {
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

    pub(crate) fn linefeed(&mut self) {
        let bottom = self.grid().region.bottom;
        if self.grid().cursor.row == bottom {
            let template = self.template;
            self.grid_mut().scroll_up(1, &template);
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
            self.alt_grid = Some(alt);
            self.modes |= Modes::ALT_SCREEN;
        } else {
            self.alt_grid = None;
            self.modes -= Modes::ALT_SCREEN;
        }
        self.touch_full();
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


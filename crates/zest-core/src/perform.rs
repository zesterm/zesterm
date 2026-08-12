//! The `vte::Perform` dispatch: parsed escape sequences to grid operations.

use alloc::string::String;

use crate::cell::Cell;
use crate::grid::ScrollRegion;
use crate::modes::{CursorStyle, Modes};
use crate::palette::Rgb;
use crate::term::{TermEvent, TermState};

impl vte::Perform for TermState {
    fn print(&mut self, ch: char) {
        self.write_char(ch);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x07 => self.events.push(TermEvent::Bell),
            0x08 => self.backspace(),
            0x09 => self.tab(1),
            // LF, VT and FF all move down a line. LNM additionally returns the
            // carriage; without it, `\n` alone must not reset the column, which
            // is why raw-mode programs emit `\r\n`.
            0x0a..=0x0c => {
                self.linefeed();
                if self.modes.contains(Modes::LINE_FEED_NEW_LINE) {
                    self.carriage_return();
                }
            }
            0x0d => self.carriage_return(),
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &vte::Params, intermediates: &[u8], ignore: bool, c: char) {
        if ignore {
            return;
        }
        let arg = |n: usize, default: usize| -> usize {
            params
                .iter()
                .nth(n)
                .and_then(|p| p.first().copied())
                .filter(|&v| v != 0)
                .map_or(default, |v| v as usize)
        };
        let private = intermediates.first() == Some(&b'?');

        match (c, intermediates.first()) {
            // --- cursor movement ---
            ('A', _) => self.move_up(arg(0, 1)),
            ('B', _) | ('e', _) => self.move_down(arg(0, 1)),
            ('C', _) | ('a', _) => self.move_right(arg(0, 1)),
            ('D', _) => self.move_left(arg(0, 1)),
            ('E', _) => {
                self.move_down(arg(0, 1));
                self.carriage_return();
            }
            ('F', _) => {
                self.move_up(arg(0, 1));
                self.carriage_return();
            }
            ('G', _) | ('`', _) => {
                let col = arg(0, 1) - 1;
                let row = self.grid().cursor.row;
                self.goto_absolute(row, col);
            }
            ('d', _) => {
                let row = arg(0, 1) - 1;
                let col = self.grid().cursor.col;
                self.goto(row, col);
            }
            ('H', _) | ('f', _) => self.goto(arg(0, 1) - 1, arg(1, 1) - 1),
            ('I', _) => self.tab(arg(0, 1)),
            ('Z', _) => self.back_tab(arg(0, 1)),

            // --- erasing ---
            ('J', _) => self.erase_in_display(arg(0, 0)),
            ('K', _) => self.erase_in_line(arg(0, 0)),
            ('X', _) => self.erase_chars(arg(0, 1)),

            // --- insert / delete ---
            ('@', _) => {
                let t = self.template;
                self.grid_mut().insert_cells(arg(0, 1), &t);
                self.touch();
            }
            ('P', _) => {
                let t = self.template;
                self.grid_mut().delete_cells(arg(0, 1), &t);
                self.touch();
            }
            ('L', _) => {
                let t = self.template;
                self.grid_mut().insert_lines(arg(0, 1), &t);
                self.touch_full();
            }
            ('M', _) => {
                let t = self.template;
                self.grid_mut().delete_lines(arg(0, 1), &t);
                self.touch_full();
            }

            // --- scrolling ---
            ('S', _) => {
                let t = self.template;
                self.grid_mut().scroll_up(arg(0, 1), &t);
                self.evict_blocks();
                self.touch_full();
            }
            ('T', _) => {
                let t = self.template;
                self.grid_mut().scroll_down(arg(0, 1), &t);
                self.touch_full();
            }

            // --- modes ---
            ('h', _) if private => self.set_private_modes(params, true),
            ('l', _) if private => self.set_private_modes(params, false),
            ('h', _) => self.set_ansi_modes(params, true),
            ('l', _) => self.set_ansi_modes(params, false),

            // --- appearance ---
            ('m', _) => self.apply_sgr(params),
            // DECSCUSR is `CSI Ps SP q` -- distinguished only by the space
            // intermediate, which is why intermediates are matched on.
            ('q', Some(b' ')) => {
                let style = CursorStyle::from_decscusr(arg(0, 0) as u16);
                self.cursor_style = style;
                self.events.push(TermEvent::CursorStyle(style));
                self.touch();
            }

            // --- scroll region ---
            ('r', _) if !private => {
                let rows = self.grid().rows();
                let top = arg(0, 1) - 1;
                let bottom = arg(1, rows) - 1;
                if top < bottom && bottom < rows {
                    self.grid_mut().region = ScrollRegion { top, bottom };
                } else {
                    self.grid_mut().region = ScrollRegion::full(rows);
                }
                // DECSTBM homes the cursor, which programs rely on.
                self.goto(0, 0);
                self.touch_full();
            }

            // --- tab stops ---
            ('g', _) => match arg(0, 0) {
                0 => {
                    let col = self.grid().cursor.col;
                    if let Some(t) = self.tabs.get_mut(col) {
                        *t = false;
                    }
                }
                3 => self.tabs.iter_mut().for_each(|t| *t = false),
                _ => {}
            },

            // --- reports ---
            // DA1: identify as a VT220 with the usual extensions.
            ('c', _) => self.reply(b"\x1b[?62;1;6;22c"),
            ('n', _) => match arg(0, 0) {
                5 => self.reply(b"\x1b[0n"), // "I am OK"
                6 => {
                    // CPR. Origin mode makes this relative to the region.
                    let (row, col) = (self.grid().cursor.row, self.grid().cursor.col);
                    let row = if self.modes.contains(Modes::ORIGIN) {
                        row - self.grid().region.top
                    } else {
                        row
                    };
                    let mut s = String::new();
                    core::fmt::Write::write_fmt(
                        &mut s,
                        format_args!("\x1b[{};{}R", row + 1, col + 1),
                    )
                    .ok();
                    self.reply(s.as_bytes());
                }
                _ => {}
            },

            ('s', _) => self.save_cursor(),

            // --- the Kitty keyboard protocol ---
            //
            // These must precede the SCORC arm below, which matches `u` with
            // any intermediate. It used to match these too, so every query and
            // every push moved the cursor instead -- silently, because SCORC
            // restores a position that is usually where the cursor already is.
            ('u', Some(b'?')) => {
                let mut s = String::new();
                core::fmt::Write::write_fmt(&mut s, format_args!("\x1b[?{}u", self.kitty_flags()))
                    .ok();
                self.reply(s.as_bytes());
            }
            // `arg` substitutes the default for an explicit zero, which is
            // harmless here only because zero *is* the default for a push and
            // for a set's flags. `CSI > 0 u` means "push no flags" and gets it.
            ('u', Some(b'>')) => self.kitty_push(arg(0, 0) as u8),
            ('u', Some(b'<')) => self.kitty_pop(arg(0, 1)),
            ('u', Some(b'=')) => self.kitty_set(arg(0, 0) as u8, arg(1, 1) as u16),

            // `None`, not `_`: answering SCORC for an intermediate this
            // terminal does not know is how the kitty sequences above came to
            // be executed as cursor restores in the first place.
            ('u', None) => self.restore_cursor(),
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        match (byte, intermediates.first()) {
            (b'7', _) => self.save_cursor(),
            (b'8', None) => self.restore_cursor(),
            (b'D', _) => self.linefeed(),
            (b'E', _) => {
                self.linefeed();
                self.carriage_return();
            }
            // RI: reverse index, scrolling down if already at the top.
            (b'M', _) => self.reverse_index(),
            (b'H', _) => {
                let col = self.grid().cursor.col;
                if let Some(t) = self.tabs.get_mut(col) {
                    *t = true;
                }
            }
            (b'c', _) => self.full_reset(),
            // DECALN: fill the screen with 'E'. Used by vttest to check
            // alignment, and cheap to support.
            (b'8', Some(b'#')) => self.decaln(),
            _ => {}
        }
    }

    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        let Some(&code) = params.first() else { return };
        let num = core::str::from_utf8(code).ok().and_then(|s| s.parse::<u16>().ok());

        match num {
            // 0 sets icon name and title, 2 sets the title.
            Some(0) | Some(2) => {
                if let Some(title) = params.get(1).and_then(|b| core::str::from_utf8(b).ok()) {
                    self.title = String::from(title);
                    self.events.push(TermEvent::Title(String::from(title)));
                }
            }
            // 4: set or query an indexed palette entry.
            Some(4) => self.osc_palette(params),
            // 8: hyperlink. `OSC 8 ; params ; uri` opens, empty uri closes.
            Some(8) => {
                let uri = params.get(2).and_then(|b| core::str::from_utf8(b).ok()).unwrap_or("");
                if uri.is_empty() {
                    self.current_hyperlink = None;
                    self.events.push(TermEvent::Hyperlink(None));
                } else {
                    self.current_hyperlink = Some(self.next_hyperlink_id);
                    self.next_hyperlink_id = self.next_hyperlink_id.wrapping_add(1);
                    self.events.push(TermEvent::Hyperlink(Some(String::from(uri))));
                }
            }
            // 10/11/12: query or set default fg / bg / cursor.
            //
            // The query form matters more than it looks: many TUIs ask for the
            // background to decide whether they are on a light or dark
            // terminal, and render badly if nobody answers.
            Some(n @ 10..=12) => self.osc_dynamic_color(n, params),
            // 104/110/111/112: reset those back to the theme's seed.
            Some(104) => {
                let idx = params
                    .get(1)
                    .and_then(|b| core::str::from_utf8(b).ok())
                    .and_then(|s| s.parse::<u8>().ok());
                self.palette.reset_indexed(idx);
                self.touch_full();
            }
            Some(110) => {
                self.palette.reset_foreground();
                self.touch_full();
            }
            Some(111) => {
                self.palette.reset_background();
                self.touch_full();
            }
            Some(112) => {
                self.palette.reset_cursor();
                self.touch_full();
            }
            // 7: the shell's working directory, as a `file://` URL. Stamped
            // onto the next block, and reported for a session nobody is
            // attached to.
            Some(7) => self.osc_cwd(params),
            // 133: semantic prompt markers -- the shell saying where a command
            // begins and ends. See [`crate::blocks`].
            Some(133) => self.osc_prompt(params),
            // 633: VS Code's dialect of 133. Handled because a great many
            // people already have its shell integration installed and would
            // otherwise get no blocks here while getting them there.
            Some(633) => self.osc_vscode(params),
            _ => {}
        }
    }
}

impl TermState {
    fn reply(&mut self, bytes: &[u8]) {
        self.events.push(TermEvent::Reply(bytes.to_vec()));
    }

    /// Absolute addressing that ignores origin mode, for column moves.
    fn goto_absolute(&mut self, row: usize, col: usize) {
        let cols = self.grid().cols();
        self.grid_mut().cursor.row = row;
        self.grid_mut().cursor.col = col.min(cols - 1);
        self.grid_mut().cursor.pending_wrap = false;
        self.touch();
    }

    fn back_tab(&mut self, count: usize) {
        let mut col = self.grid().cursor.col;
        for _ in 0..count.max(1) {
            col = (0..col).rev().find(|&c| self.tabs.get(c).copied().unwrap_or(false)).unwrap_or(0);
        }
        self.grid_mut().cursor.col = col;
        self.touch();
    }

    fn reverse_index(&mut self) {
        let top = self.grid().region.top;
        if self.grid().cursor.row == top {
            let t = self.template;
            self.grid_mut().scroll_down(1, &t);
            self.touch_full();
        } else if self.grid().cursor.row > 0 {
            self.grid_mut().cursor.row -= 1;
            self.touch();
        }
    }

    fn erase_in_display(&mut self, mode: usize) {
        let t = self.template;
        let (row, col) = (self.grid().cursor.row, self.grid().cursor.col);
        let (rows, cols) = (self.grid().rows(), self.grid().cols());
        match mode {
            0 => {
                self.grid_mut().erase_in_row(row, col, cols - 1, &t);
                if row + 1 < rows {
                    self.grid_mut().erase_rows(row + 1, rows - 1, &t);
                }
            }
            1 => {
                if row > 0 {
                    self.grid_mut().erase_rows(0, row - 1, &t);
                }
                self.grid_mut().erase_in_row(row, 0, col, &t);
            }
            // 2 clears the screen; 3 also clears scrollback. Treated alike here
            // because scrollback trimming is a separate concern from painting.
            2 | 3 => self.grid_mut().erase_rows(0, rows - 1, &t),
            _ => {}
        }
        self.touch_full();
    }

    fn erase_in_line(&mut self, mode: usize) {
        let t = self.template;
        let (row, col) = (self.grid().cursor.row, self.grid().cursor.col);
        let cols = self.grid().cols();
        match mode {
            0 => self.grid_mut().erase_in_row(row, col, cols - 1, &t),
            1 => self.grid_mut().erase_in_row(row, 0, col, &t),
            2 => self.grid_mut().erase_in_row(row, 0, cols - 1, &t),
            _ => {}
        }
        self.touch();
    }

    fn erase_chars(&mut self, n: usize) {
        let t = self.template;
        let (row, col) = (self.grid().cursor.row, self.grid().cursor.col);
        let cols = self.grid().cols();
        let end = (col + n).min(cols) - 1;
        self.grid_mut().erase_in_row(row, col, end, &t);
        self.touch();
    }

    fn decaln(&mut self) {
        let (rows, cols) = (self.grid().rows(), self.grid().cols());
        for row in 0..rows {
            for col in 0..cols {
                if let Some(c) = self.grid_mut().cell_mut(row, col) {
                    c.ch = 'E';
                }
            }
        }
        self.touch_full();
    }

    fn full_reset(&mut self) {
        let t = Cell::default();
        self.template = t;
        self.modes = Modes::initial();
        // Before `set_alt_screen`, which re-reads the main stack to decide what
        // the flags become. Clearing after works only because the clear then
        // overwrites it, which is a right answer by way of a wrong order.
        //
        // Both stacks, not just the flags: a surviving stack would put the old
        // flags back on the next pop, after a reset that promised otherwise.
        self.kitty_reset();
        self.set_alt_screen(false);
        self.grid_mut().clear_all(&t);
        self.palette.reset_indexed(None);
        self.palette.reset_foreground();
        self.palette.reset_background();
        self.palette.reset_cursor();
        // RIS clears the screen the blocks index, so the index describes
        // nothing. Keeping it would leave every block pointing at lines that
        // have been blanked -- folding one would hide someone else's output.
        self.blocks = crate::blocks::BlockIndex::new();
        self.prompt_end = None;
        self.pending_command = None;
        self.touch_full();
    }

    fn set_private_modes(&mut self, params: &vte::Params, enable: bool) {
        for p in params.iter() {
            let Some(&mode) = p.first() else { continue };
            match mode {
                1 => self.modes.set(Modes::APP_CURSOR, enable),
                6 => {
                    self.modes.set(Modes::ORIGIN, enable);
                    // DECOM homes the cursor within the new origin.
                    self.goto(0, 0);
                }
                7 => self.modes.set(Modes::AUTO_WRAP, enable),
                25 => self.modes.set(Modes::SHOW_CURSOR, enable),
                66 => self.modes.set(Modes::APP_KEYPAD, enable),
                1000 => self.modes.set(Modes::MOUSE_CLICK, enable),
                1002 => self.modes.set(Modes::MOUSE_DRAG, enable),
                1003 => self.modes.set(Modes::MOUSE_MOTION, enable),
                1004 => self.modes.set(Modes::FOCUS_REPORTING, enable),
                1006 => self.modes.set(Modes::MOUSE_SGR, enable),
                // 47 and 1047 switch buffers; 1049 also saves/restores the
                // cursor, which is what almost everything actually uses.
                47 | 1047 => self.set_alt_screen(enable),
                1048 => {
                    if enable {
                        self.save_cursor();
                    } else {
                        self.restore_cursor();
                    }
                }
                1049 => {
                    if enable {
                        self.save_cursor();
                        self.set_alt_screen(true);
                    } else {
                        self.set_alt_screen(false);
                        self.restore_cursor();
                    }
                }
                2004 => self.modes.set(Modes::BRACKETED_PASTE, enable),
                2026 => {
                    self.modes.set(Modes::SYNC_UPDATE, enable);
                    self.events.push(TermEvent::SyncUpdate(enable));
                }
                9001 => self.modes.set(Modes::WIN32_INPUT, enable),
                _ => {}
            }
        }
        self.touch();
    }

    fn set_ansi_modes(&mut self, params: &vte::Params, enable: bool) {
        for p in params.iter() {
            match p.first() {
                Some(4) => self.modes.set(Modes::INSERT, enable),
                Some(20) => self.modes.set(Modes::LINE_FEED_NEW_LINE, enable),
                _ => {}
            }
        }
        self.touch();
    }

    // --- command blocks (OSC 133, 7, 633) --------------------------------
    //
    // The markers say *where*, never *what*: the text between `B` and `C` is
    // the command, and the grid is the only place it exists. That is why these
    // live in the parser rather than in a shell-integration layer -- only the
    // parser knows which line the cursor was on when the marker arrived.

    /// `OSC 133 ; A|B|C|D [; exit]`.
    fn osc_prompt(&mut self, params: &[&[u8]]) {
        // The first byte, not the whole segment: shells append their own
        // key-value tails (`A;special_key=1`, `D;0;aid=3`) and every one of
        // them is still an `A` or a `D`.
        let Some(kind) = params.get(1).and_then(|p| p.first().copied()) else { return };
        match kind {
            b'A' => self.block_prompt_start(),
            b'B' => self.block_prompt_end(),
            b'C' => self.block_output_start(None),
            b'D' => {
                let exit = params.get(2).and_then(|p| parse_exit_status(p));
                self.block_finish(exit);
            }
            _ => {}
        }
    }

    /// `OSC 633 ; ...` -- VS Code's dialect.
    ///
    /// `A`/`B`/`C`/`D` are its 133 equivalents. `E` is the one genuine
    /// addition: it carries the command line *explicitly*, which beats reading
    /// it back off the grid because it is what the shell will actually run
    /// rather than what the screen shows.
    fn osc_vscode(&mut self, params: &[&[u8]]) {
        let Some(kind) = params.get(1).and_then(|p| p.first().copied()) else { return };
        match kind {
            b'A' => self.block_prompt_start(),
            b'B' => self.block_prompt_end(),
            b'C' => self.block_output_start(None),
            b'D' => {
                let exit = params.get(2).and_then(|p| parse_exit_status(p));
                self.block_finish(exit);
            }
            // `E ; <command> [; nonce]`. vte caps an OSC payload at 1024 bytes
            // (`MAX_OSC_RAW`), so a very long command line arrives truncated --
            // which is a shorter command, never a wrong one, and still better
            // than a grid readback that has to guess where the prompt ended.
            b'E' => {
                if let Some(cmd) = params.get(2).and_then(|b| core::str::from_utf8(b).ok()) {
                    self.pending_command = Some(unescape_vscode(cmd));
                }
            }
            // `P ; Cwd=<path>` -- a plain path, not the `file://` URL of OSC 7,
            // but escaped the same way `E` is. That is not decoration on
            // Windows: VS Code's own hook sends `C:\x5cDev`, so reading the
            // value literally put a cwd of `C:\x5cDev\x5czesterm` in the status
            // bar for everyone using the integration it is here to support.
            // Measured, not guessed -- see the 633 fixture.
            b'P' => {
                if let Some(rest) = params.get(2).and_then(|b| core::str::from_utf8(b).ok()) {
                    if let Some(path) = rest.strip_prefix("Cwd=") {
                        self.cwd = unescape_vscode(path);
                    }
                }
            }
            _ => {}
        }
    }

    /// `OSC 7 ; file://<host>/<path>`.
    fn osc_cwd(&mut self, params: &[&[u8]]) {
        let Some(url) = params.get(1).and_then(|b| core::str::from_utf8(b).ok()) else { return };
        // The host part is deliberately discarded rather than compared against
        // this machine's name: over ssh it names the *remote* host, and a cwd
        // that silently blanks itself the moment you ssh somewhere is worse
        // than one that is occasionally another machine's path.
        let path = url.strip_prefix("file://").map_or(url, |rest| {
            rest.find('/').map_or(rest, |slash| &rest[slash..])
        });
        self.cwd = percent_decode(path);
    }

    /// The line a marker names, in the **primary** grid's numbering.
    ///
    /// `None` on the alternate screen, which suppresses the marker entirely.
    /// The alt screen is a separate `Grid` whose ids restart at zero, so a
    /// marker recorded there names a line in the wrong numbering space -- and
    /// it would be a plausible small id rather than an obviously wrong one,
    /// which is the kind of bug that gets found months later. A full-screen
    /// program owns the display and its content is not command history anyway.
    fn block_line(&self) -> Option<crate::grid::LineId> {
        if self.alt_grid.is_some() {
            return None;
        }
        self.grid.active_line_id_at(self.grid.cursor.row)
    }

    fn block_prompt_start(&mut self) {
        let Some(line) = self.block_line() else { return };
        let cwd = self.cwd.clone();
        self.blocks.begin_prompt(line, cwd);
        self.prompt_end = None;
        self.pending_command = None;
        self.touch();
    }

    fn block_prompt_end(&mut self) {
        let Some(line) = self.block_line() else { return };
        self.prompt_end = Some((line, self.grid.cursor.col));
    }

    fn block_output_start(&mut self, command: Option<String>) {
        let Some(line) = self.block_line() else { return };
        let command = command
            .or_else(|| self.pending_command.take())
            .unwrap_or_else(|| self.command_text());

        // `C` fires when the shell is about to run the command, which is
        // *before* it echoes the newline the user pressed -- so the cursor is
        // still sitting at the end of the typed command, and this line is the
        // command's, not the output's. `output_line` is documented as the first
        // line of output and consumers rely on that: copy-output would
        // otherwise hand back the prompt and the command it was asked to
        // exclude.
        //
        // Column zero means the shell emitted the newline first, and then this
        // line really is where output begins.
        let line = if self.grid.cursor.col > 0 { line + 1 } else { line };
        self.blocks.begin_output(line, command, self.now_ms);
        self.prompt_end = None;
        self.touch();
    }

    fn block_finish(&mut self, exit_code: Option<i32>) {
        let Some(line) = self.block_line() else { return };

        // The mirror of the adjustment in `block_output_start`. `D` arrives
        // after the command's last newline, so the cursor has already moved to
        // the line the *next* prompt will be drawn on -- and a block that
        // claims that line owns a row belonging to the command after it, which
        // is what makes folding hide one line too many.
        //
        // A command whose output did not end in a newline leaves the cursor
        // mid-line, and then this line really is the block's last.
        let line = if self.grid.cursor.col == 0 { line.saturating_sub(1) } else { line };
        // Never before its own prompt. A command that printed nothing at all
        // ends where it began, and an end line above the start would make
        // `contains` answer for no line and `evict_before` reason backwards.
        let line = self.blocks.last().map_or(line, |b| line.max(b.prompt_line));
        self.blocks.finish(line, exit_code, self.now_ms);
        self.prompt_end = None;
        self.pending_command = None;
        self.touch();
    }

    /// The submitted command, read back from the grid between `B` and `C`.
    ///
    /// OSC 133 carries only positions, so the cells between the two markers
    /// *are* the command. Rows are sliced by column rather than by offset into
    /// `Row::text()`: that method drops wide-character spacers, so a character
    /// index into it is not a column, and a CJK character anywhere in the
    /// command would shift everything after it.
    fn command_text(&self) -> String {
        let Some((start_line, start_col)) = self.prompt_end else { return String::new() };
        let Some(end_line) = self.grid.active_line_id_at(self.grid.cursor.row) else {
            return String::new();
        };

        let mut out = String::new();
        // Walk retained lines rather than viewport rows: a command long enough
        // to wrap can have pushed its own first row into scrollback by the time
        // `C` arrives.
        for index in 0..self.grid.total_lines() {
            let Some(row) = self.grid.line(index) else { continue };
            if row.id < start_line || row.id > end_line {
                continue;
            }
            let from = if row.id == start_line { start_col } else { 0 };
            // A wrapped row is full by definition; only the last row of a
            // logical line has trailing blanks worth dropping.
            let to = if row.wrapped { row.len() } else { row.trimmed_len() };
            for col in from..to {
                let Some(cell) = row.get(col) else { break };
                if cell.flags.contains(crate::cell::CellFlags::WIDE_SPACER) {
                    continue;
                }
                out.push(cell.ch);
                if let Some(extra) = row.extra(cell) {
                    out.extend(extra.zerowidth.iter());
                }
            }
            // Rows joined by `wrapped` are one command, not two.
            if !row.wrapped && row.id != end_line {
                break;
            }
        }
        String::from(out.trim_end())
    }

    fn osc_palette(&mut self, params: &[&[u8]]) {
        // `OSC 4 ; index ; spec` -- possibly repeated.
        let mut i = 1;
        while i + 1 < params.len() {
            let idx = core::str::from_utf8(params[i]).ok().and_then(|s| s.parse::<u8>().ok());
            let spec = core::str::from_utf8(params[i + 1]).unwrap_or("");
            if let Some(idx) = idx {
                if spec == "?" {
                    let c = self.palette.live().colors[idx as usize];
                    let mut s = String::new();
                    core::fmt::Write::write_fmt(
                        &mut s,
                        format_args!(
                            "\x1b]4;{};rgb:{:04x}/{:04x}/{:04x}\x07",
                            idx,
                            u16::from(c.r) * 257,
                            u16::from(c.g) * 257,
                            u16::from(c.b) * 257
                        ),
                    )
                    .ok();
                    self.reply(s.as_bytes());
                } else if let Some(c) = parse_color_spec(spec) {
                    self.palette.set_indexed(idx, c);
                    self.touch_full();
                }
            }
            i += 2;
        }
    }

    fn osc_dynamic_color(&mut self, code: u16, params: &[&[u8]]) {
        let spec = params.get(1).and_then(|b| core::str::from_utf8(b).ok()).unwrap_or("");
        let live = self.palette.live();
        let current = match code {
            10 => live.foreground,
            11 => live.background,
            _ => live.cursor,
        };

        if spec == "?" {
            let mut s = String::new();
            core::fmt::Write::write_fmt(
                &mut s,
                format_args!(
                    "\x1b]{};rgb:{:04x}/{:04x}/{:04x}\x07",
                    code,
                    u16::from(current.r) * 257,
                    u16::from(current.g) * 257,
                    u16::from(current.b) * 257
                ),
            )
            .ok();
            self.reply(s.as_bytes());
        } else if let Some(c) = parse_color_spec(spec) {
            match code {
                10 => self.palette.set_foreground(c),
                11 => self.palette.set_background(c),
                _ => self.palette.set_cursor(c),
            }
            self.touch_full();
        }
    }
}

/// The exit status from `OSC 133;D;<status>`.
///
/// **A missing or unparseable status is `None`, never zero.** Plenty of shells
/// emit `D` bare, and a green tick on a command that actually failed is worse
/// than no tick at all — see [`crate::BlockState::Finished`].
///
/// A status is accepted from a leading run of digits so that `D;1;aid=7` parses
/// as `1`; anything that does not start with a digit is unknown, not success.
fn parse_exit_status(param: &[u8]) -> Option<i32> {
    let s = core::str::from_utf8(param).ok()?;
    let digits = s.find(|c: char| !c.is_ascii_digit()).map_or(s, |i| &s[..i]);
    digits.parse::<i32>().ok()
}

/// Undo the escaping VS Code applies to the `OSC 633;E` command line.
///
/// It escapes the characters that would otherwise terminate or split the
/// sequence — a literal semicolon in a command is otherwise a new OSC
/// parameter, which is how `grep -e 'a;b'` would arrive cut in half.
fn unescape_vscode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('x') => {
                let hex: String = chars.by_ref().take(2).collect();
                match u8::from_str_radix(&hex, 16) {
                    Ok(b) => out.push(b as char),
                    // Not a valid escape: keep the bytes rather than dropping
                    // them, so a command with a literal backslash survives.
                    Err(_) => {
                        out.push('\\');
                        out.push('x');
                        out.push_str(&hex);
                    }
                }
            }
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Decode `%XX` escapes in an OSC 7 path.
///
/// A path with a space arrives as `%20`, and a shell prompt reporting
/// `/Users/me/My%20Code` is a directory that does not exist.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: alloc::vec::Vec<u8> = alloc::vec::Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = core::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(b) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    // A path that is not UTF-8 after decoding is left as it arrived rather than
    // replaced with U+FFFD, which would name a different directory.
    String::from_utf8(out).unwrap_or_else(|_| String::from(s))
}

/// Parse an XParseColor-style spec: `rgb:RR/GG/BB` with 1-4 hex digits per
/// component, or `#rgb` / `#rrggbb`.
fn parse_color_spec(spec: &str) -> Option<Rgb> {
    if let Some(rest) = spec.strip_prefix("rgb:") {
        let mut parts = rest.split('/');
        let scale = |s: &str| -> Option<u8> {
            let v = u32::from_str_radix(s, 16).ok()?;
            // Components may be 1-4 hex digits; normalize to 8 bits.
            Some(match s.len() {
                1 => (v * 17) as u8,
                2 => v as u8,
                3 => (v >> 4) as u8,
                4 => (v >> 8) as u8,
                _ => return None,
            })
        };
        let r = scale(parts.next()?)?;
        let g = scale(parts.next()?)?;
        let b = scale(parts.next()?)?;
        return Some(Rgb::new(r, g, b));
    }

    let hex = spec.strip_prefix('#')?;
    match hex.len() {
        3 => {
            let d = |i: usize| u8::from_str_radix(&hex[i..=i], 16).ok().map(|v| v * 17);
            Some(Rgb::new(d(0)?, d(1)?, d(2)?))
        }
        6 => {
            let d = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).ok();
            Some(Rgb::new(d(0)?, d(2)?, d(4)?))
        }
        _ => None,
    }
}


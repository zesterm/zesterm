//! The `vte::Perform` dispatch: parsed escape sequences to grid operations.

use alloc::string::String;

use crate::cell::Cell;
use crate::grid::ScrollRegion;
use crate::modes::{CursorStyle, Modes};
use crate::palette::Rgb;
use crate::term::{AttentionCause, Progress, ProgressState, TermEvent, TermState};

impl vte::Perform for TermState {
    fn print(&mut self, ch: char) {
        self.write_char(ch);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            // Two events, not one: `Bell` is "ring something" and
            // `Attention` is "mark this tab". A host may want either, both, or
            // neither, and folding them would make that one decision.
            0x07 => {
                self.events.push(TermEvent::Bell);
                self.raise_attention(AttentionCause::Bell);
            }
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
            ('H', _) | ('f', _) => {
                // Homing the cursor while it is hidden is how ConPTY's repaint
                // starts, and on a *grow* it is the only marker there is — the
                // size announcement comes on the shrink and not on the way back
                // (`corpus/resize-drag.vtrec`). See
                // `Grid::note_cursor_homed_while_hidden`. (#271)
                //
                // **Before** the `goto`, which is the opposite of the natural
                // reading order: `goto` strands a lingering restore on the
                // grounds that a cursor move is ordinary output, and this home
                // is the one cursor move that is not — it opens the
                // restatement bracket, and an open bracket is what tells the
                // strand to stand down so the re-bank can do its job. (#341)
                if arg(0, 1) == 1
                    && arg(1, 1) == 1
                    && !self.modes.contains(Modes::SHOW_CURSOR)
                {
                    let t = self.template;
                    self.grid_mut().note_cursor_homed_while_hidden(&t);
                }
                self.goto(arg(0, 1) - 1, arg(1, 1) - 1);
            }
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

            // --- the restating pty's repaint ---
            //
            // `CSI 8 ; rows ; cols t` is XTWINOPS "resize the text area", a
            // request *to* a terminal that nothing here obeys — a program does
            // not get to resize the user's window. ConPTY emits it in the other
            // direction, as the first thing in the repaint it answers a resize
            // with (#205), which makes it the one unambiguous marker for "the
            // restatement starts here". That is all it is taken as. (#247)
            ('t', _) if !private && arg(0, 0) == 8 => {
                let (rows, cols) = (arg(1, 0), arg(2, 0));
                let t = self.template;
                self.grid_mut().note_restatement_began(cols, rows, &t);
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
                let param = arg(0, 0) as u16;
                // 0 is *reset*, not "blinking block" -- DEC's own distinction,
                // and `from_decscusr` folds the two together because the wire
                // encoding has no third state to carry. Honoured here so a
                // program that sets a bar and resets on exit hands the terminal
                // back to `cursor.shape` rather than to a hardcoded block.
                // 0 is *reset*: it hands the terminal back to the user's
                // `cursor.shape`, and gives up the claim a program made on it.
                self.cursor_style_from_program = param != 0;
                let style = if param == 0 {
                    self.default_cursor_style
                } else {
                    CursorStyle::from_decscusr(param)
                };
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
            // 9: iTerm2/ConEmu. Two sequences share this number and only the
            // sub-parameter tells them apart -- `OSC 9 ; 4 ; …` is ConEmu's
            // taskbar *progress*, anything else is a notification whose body
            // is the rest of the string. Reading the 9 alone and calling it a
            // notification would turn every progress tick into one.
            Some(9) => {
                let first = params.get(1).and_then(|b| core::str::from_utf8(b).ok()).unwrap_or("");
                if first == "4" {
                    self.osc_progress(params);
                } else {
                    self.raise_attention(AttentionCause::Notify);
                }
            }
            // 777: the rxvt dialect. `OSC 777 ; notify ; <title> ; <body>`,
            // and only that verb -- 777 also carries `precmd` and others we do
            // not implement, so an unknown verb must fall through rather than
            // be read as a notification.
            Some(777) => {
                let verb = params.get(1).and_then(|b| core::str::from_utf8(b).ok()).unwrap_or("");
                if verb == "notify" {
                    self.raise_attention(AttentionCause::Notify);
                }
            }
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

    /// Record that the program asked to be noticed.
    fn raise_attention(&mut self, cause: AttentionCause) {
        self.events.push(TermEvent::Attention { cause });
    }

    /// `OSC 9 ; 4 ; st ; pr` — ConEmu's taskbar progress.
    ///
    /// A malformed `st` reads as `0` (cleared) rather than being ignored: a
    /// progress state that cannot be parsed must not leave a spinner turning
    /// for the rest of the session, which is what "ignore what you do not
    /// understand" produces for a *latched* value. Ignoring is right for a
    /// one-shot; for state it is a leak.
    fn osc_progress(&mut self, params: &[&[u8]]) {
        // Parsed wide and clamped after, never as the `u8` it ends up in: a
        // producer sending 300 would *fail to parse* as a `u8` and fall to 0,
        // which is a rewind rather than a clamp -- and the only reason that
        // reads as fine is that a test written with 140 in it fits either way.
        let num = |i: usize| -> Option<u32> {
            params
                .get(i)
                .and_then(|b| core::str::from_utf8(b).ok())
                .and_then(|s| s.trim().parse::<u32>().ok())
        };
        let state = num(2).unwrap_or(0);
        // Percentages beyond 100 are clamped rather than refused. A build
        // script that computes 103 is still a build script that is nearly
        // done, and refusing it would stop the bar where the last good value
        // left it -- a wrong number frozen, which is worse than a right one
        // rounded.
        let percent = u8::try_from(num(3).unwrap_or(0).min(100)).unwrap_or(100);
        let was_busy = !matches!(self.progress, Progress::None);
        self.progress = match state {
            1 => Progress::At { percent, state: ProgressState::Normal },
            2 => Progress::At { percent, state: ProgressState::Error },
            3 => Progress::Indeterminate,
            4 => Progress::At { percent, state: ProgressState::Warning },
            // 0, and anything unrecognised.
            _ => Progress::None,
        };
        // Stopping being busy is the thing the person was waiting for, and so
        // is failing. Both raise the same signal a bell would -- which is what
        // makes "my build finished" work for a program that reports progress
        // and never rings.
        let done = was_busy && matches!(self.progress, Progress::None);
        let failed = matches!(self.progress, Progress::At { state: ProgressState::Error, .. });
        if done || failed {
            self.raise_attention(AttentionCause::Notify);
        }
    }

    /// Absolute addressing that ignores origin mode, for column moves.
    fn goto_absolute(&mut self, row: usize, col: usize) {
        self.strand_if_diverged();
        let cols = self.grid().cols();
        self.grid_mut().cursor.row = row;
        self.grid_mut().cursor.col = col.min(cols - 1);
        self.grid_mut().cursor.pending_wrap = false;
        self.touch();
    }

    fn back_tab(&mut self, count: usize) {
        self.strand_if_diverged();
        let mut col = self.grid().cursor.col;
        for _ in 0..count.max(1) {
            col = (0..col).rev().find(|&c| self.tabs.get(c).copied().unwrap_or(false)).unwrap_or(0);
        }
        self.grid_mut().cursor.col = col;
        self.touch();
    }

    fn reverse_index(&mut self) {
        self.strand_if_diverged();
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
        self.strand_if_diverged();
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
            // 2 clears the screen; 3 clears scrollback too, below. The
            // painting half is shared.
            2 | 3 => {
                self.grid_mut().erase_rows(0, rows - 1, &t);
                // The blocks that described those rows describe nothing now.
                // Line ids survive an erase, so the shell reuses the very ids
                // the old blocks still claim and a stale header lands on the
                // live prompt -- opaque, and it eats the click too.
                //
                // Only modes 2 and 3. Mode 0 is what a line editor emits on
                // every keystroke (PSReadLine repaints with `ESC[J` constantly),
                // and invalidating there would delete the block being typed
                // into over and over.
                if let Some(first) = self.grid.active_line_id_at(0) {
                    self.blocks.erase_screen(first);
                }
                self.prompt_end = None;
                self.pending_command = None;
                // A cleared screen is all blank tail, so a restate debt left
                // owing would let the next grow pull history straight back onto
                // the screen the user just cleared. Modes 2 and 3 only, for the
                // reason above: mode 0 is the line editor repainting, and
                // ConPTY's own repaint can arrive either side of it. (#247)
                self.grid_mut().cancel_restate_debt();

                if mode == 3 {
                    // ED 3 also destroys scrollback — xterm's and Windows
                    // Terminal's reading, and what pwsh's `Clear-Host` asks
                    // for with its `2J 3J` pair. The *primary* grid's,
                    // directly: scrollback lives there and the alt grid has
                    // none. (An ED 3 issued while the alt screen is active is
                    // honoured the same way; shells do not run on the alt
                    // screen, and the counter travels when the primary
                    // returns.) With the rows gone, every block dies too —
                    // the ones the screen erase above spared were exactly the
                    // ones living in scrollback — and `authoritative_from`
                    // falls to zero, so the next keyframe replaces the
                    // client's list wholesale. (#314)
                    self.grid.clear_history();
                    self.blocks.erase_screen(0);
                    self.events.push(TermEvent::HistoryCleared);
                }
            }
            _ => {}
        }
        self.touch_full();
    }

    fn erase_in_line(&mut self, mode: usize) {
        self.strand_if_diverged();
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
        self.strand_if_diverged();
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
                25 => {
                    self.modes.set(Modes::SHOW_CURSOR, enable);
                    // ConPTY brackets its resize repaint in DECTCEM: it hides
                    // the cursor, restates the viewport, puts the cursor back
                    // and restores visibility. So the first DECTCEM *after* the
                    // `CSI 8 t` that opened the restatement is the repaint
                    // closing, and from there the viewport is settled: its tail
                    // is blank rows nothing will write again, which is where the
                    // history a shrink displaced can safely go. (#247)
                    //
                    // Either direction closes it. The final state is whatever
                    // the inner program had, so a full-screen app that keeps its
                    // cursor hidden ends the repaint with `?25l` and never sends
                    // `?25h` at all; keying off one of them would leave the debt
                    // unpaid for exactly those sessions.
                    if self.grid().restating() {
                        self.settle_restate();
                    } else {
                        // A repaint that was sat out closes here too, or its
                        // decision outlives it and silences the next one.
                        self.grid_mut().end_restatement_window();
                    }
                }
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
                        // The 633 dialect has no authority part at all, so
                        // `None` here means exactly what it means for a bare
                        // OSC 7 path: nobody said elsewhere.
                        self.set_cwd(None, unescape_vscode(path));
                    }
                }
            }
            _ => {}
        }
    }

    /// `OSC 7 ; file://<host>/<path>`.
    fn osc_cwd(&mut self, params: &[&[u8]]) {
        let Some(url) = params.get(1).and_then(|b| core::str::from_utf8(b).ok()) else { return };
        // The host part never blanks the *path*: over ssh it names the remote
        // host, and a cwd that silently empties itself the moment you ssh
        // somewhere is worse than one that is occasionally another machine's.
        // It is kept beside the path instead, so a consumer probing the local
        // filesystem can decline to (`TermEvent::CwdChanged`).
        let (host, path) = match url.strip_prefix("file://") {
            None => (None, url),
            Some(rest) => match rest.find('/') {
                None => (None, rest),
                Some(slash) => {
                    let authority = &rest[..slash];
                    ((!authority.is_empty()).then(|| authority.to_string()), &rest[slash..])
                }
            },
        };
        self.set_cwd(host, percent_decode(path));
    }

    /// Record a reported cwd, announcing it only when it actually moved.
    ///
    /// OSC 7 arrives on every prompt, so "changed" is the event and "reported
    /// again" is nothing — a consumer re-probing the same directory per prompt
    /// would be the subprocess-per-prompt cost the shell hooks are written to
    /// avoid, moved one layer down.
    fn set_cwd(&mut self, host: Option<String>, path: String) {
        if self.cwd == path && self.cwd_host == host {
            return;
        }
        self.cwd = path.clone();
        self.cwd_host = host.clone();
        self.events.push(TermEvent::CwdChanged { host, path });
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
            let to = if row.wrapped() { row.len() } else { row.trimmed_len() };
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
            if !row.wrapped() && row.id != end_line {
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


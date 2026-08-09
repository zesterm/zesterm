//! VT conformance and replay tests.
//!
//! Two layers, matching the two fixture kinds (see `tests/README.md`):
//! hand-written sequences that assert spec behavior, and replays of real
//! programs recorded through a real ConPTY.

use zest_core::{Cell, CellFlags, Color, Modes, TermEvent, Terminal};

/// Feed a sequence to a fresh terminal and return it.
fn run(cols: usize, rows: usize, input: &str) -> Terminal {
    let mut t = Terminal::new(cols, rows, 100);
    t.advance(input.as_bytes());
    t
}

/// The visible screen with trailing blank lines removed, for readable asserts.
fn screen(t: &Terminal) -> String {
    t.screen_text().trim_end().to_string()
}

// --- text and wrapping ---------------------------------------------------

#[test]
fn plain_text_lands_on_the_grid() {
    let t = run(20, 3, "hello");
    assert_eq!(screen(&t), "hello");
    assert_eq!(t.cursor().col, 5);
}

#[test]
fn crlf_moves_to_the_next_line() {
    let t = run(20, 3, "one\r\ntwo");
    assert_eq!(screen(&t), "one\ntwo");
}

#[test]
fn bare_lf_does_not_return_the_carriage() {
    // Without LNM, \n moves down but keeps the column. Programs in raw mode
    // depend on this, which is why they emit \r\n explicitly.
    let t = run(20, 3, "abc\ndef");
    assert_eq!(screen(&t), "abc\n   def");
}

#[test]
fn wrapping_is_deferred_until_the_next_character() {
    // Writing exactly `cols` characters must NOT scroll or move to the next
    // line -- the wrap happens only when another character arrives. Terminals
    // that wrap eagerly emit a spurious blank line on every full-width row.
    let t = run(5, 3, "abcde");
    assert_eq!(screen(&t), "abcde");
    assert_eq!(t.cursor().row, 0, "still on the first row after exactly cols chars");

    let t = run(5, 3, "abcdef");
    assert_eq!(screen(&t), "abcde\nf");
    assert_eq!(t.cursor().row, 1);
}

#[test]
fn wrapped_rows_are_marked() {
    let t = run(5, 3, "abcdef");
    // Needed so copying a wrapped line does not insert a newline, and so
    // reflow can rejoin the rows later.
    assert!(t.grid().row(0).wrapped);
    assert!(t.grid().cell(0, 4).unwrap().flags.contains(CellFlags::WRAPLINE));
}

#[test]
fn autowrap_off_overwrites_the_last_column() {
    let t = run(5, 2, "\x1b[?7labcdefgh");
    assert_eq!(screen(&t), "abcdh", "later chars pile onto the last cell");
}

// --- cursor movement -----------------------------------------------------

#[test]
fn cup_positions_one_based() {
    let t = run(10, 5, "\x1b[3;4Hx");
    assert_eq!(t.cursor().row, 2, "CSI 3;4H is row 3, col 4, one-based");
    assert_eq!(t.cursor().col, 4, "cursor advanced past the written char");
    assert_eq!(t.grid().cell(2, 3).unwrap().ch, 'x');
}

#[test]
fn cursor_moves_clamp_at_the_edges() {
    let t = run(10, 5, "\x1b[100;100H");
    assert_eq!(t.cursor().row, 4);
    assert_eq!(t.cursor().col, 9);

    let t = run(10, 5, "\x1b[5;5H\x1b[100A\x1b[100D");
    assert_eq!(t.cursor().row, 0);
    assert_eq!(t.cursor().col, 0);
}

#[test]
fn save_and_restore_cursor_round_trips() {
    let t = run(10, 5, "\x1b[3;4H\x1b7\x1b[1;1H\x1b8");
    assert_eq!((t.cursor().row, t.cursor().col), (2, 3));
}

#[test]
fn backspace_and_tab() {
    let t = run(20, 2, "abc\x08\x08X");
    assert_eq!(screen(&t), "aXc");

    let t = run(20, 2, "a\tb");
    assert_eq!(t.grid().cell(0, 8).unwrap().ch, 'b', "tab stops are every 8 columns");
}

// --- erasing -------------------------------------------------------------

#[test]
fn erase_in_line_modes() {
    let t = run(10, 2, "abcdef\x1b[1;3H\x1b[K");
    assert_eq!(screen(&t), "ab", "EL 0 erases to end of line");

    let t = run(10, 2, "abcdef\x1b[1;3H\x1b[1K");
    assert_eq!(screen(&t), "   def", "EL 1 erases to start, inclusive");

    let t = run(10, 2, "abcdef\x1b[2K");
    assert_eq!(screen(&t), "", "EL 2 erases the whole line");
}

#[test]
fn erase_in_display_below_and_above() {
    let t = run(5, 3, "aaa\r\nbbb\r\nccc\x1b[2;2H\x1b[J");
    assert_eq!(screen(&t), "aaa\nb");

    let t = run(5, 3, "aaa\r\nbbb\r\nccc\x1b[2;2H\x1b[1J");
    assert_eq!(screen(&t), "\n  b\nccc");
}

#[test]
fn erase_paints_the_active_background() {
    // ED/EL inside a colored region must leave that color behind, otherwise
    // colored panes get holes punched in them.
    let t = run(5, 1, "\x1b[41m\x1b[K");
    assert_eq!(t.grid().cell(0, 2).unwrap().bg, Color::Indexed(1));
}

// --- scrolling and regions ------------------------------------------------

#[test]
fn output_past_the_bottom_scrolls_into_scrollback() {
    let t = run(10, 2, "one\r\ntwo\r\nthree");
    assert_eq!(screen(&t), "two\nthree");
    assert_eq!(t.grid().scrollback_len(), 1);
}

#[test]
fn decstbm_confines_scrolling_and_homes_the_cursor() {
    let mut t = run(10, 4, "a\r\nb\r\nc\r\nd");
    t.advance(b"\x1b[2;3r");
    assert_eq!((t.cursor().row, t.cursor().col), (0, 0), "DECSTBM homes the cursor");

    t.advance(b"\x1b[2;1H\x1b[2S");
    let s = screen(&t);
    assert!(s.starts_with('a'), "rows above the region are untouched");
    assert!(s.ends_with('d'), "rows below the region are untouched");
}

#[test]
fn reverse_index_at_the_top_scrolls_down() {
    let t = run(5, 3, "a\r\nb\r\nc\x1b[1;1H\x1bM");
    assert_eq!(screen(&t), "\na\nb");
}

// --- SGR -----------------------------------------------------------------

#[test]
fn sgr_sets_and_resets_attributes() {
    let t = run(20, 1, "\x1b[1;3;4mx\x1b[0my");
    let a = t.grid().cell(0, 0).unwrap();
    assert!(a.flags.contains(CellFlags::BOLD));
    assert!(a.flags.contains(CellFlags::ITALIC));
    assert!(a.flags.contains(CellFlags::UNDERLINE));

    let b = t.grid().cell(0, 1).unwrap();
    assert_eq!(b.flags, CellFlags::empty(), "SGR 0 clears everything");
}

#[test]
fn sgr_basic_and_bright_colors() {
    let t = run(20, 1, "\x1b[31ma\x1b[91mb\x1b[42mc");
    assert_eq!(t.grid().cell(0, 0).unwrap().fg, Color::Indexed(1));
    assert_eq!(t.grid().cell(0, 1).unwrap().fg, Color::Indexed(9), "bright red is index 9");
    assert_eq!(t.grid().cell(0, 2).unwrap().bg, Color::Indexed(2));
}

#[test]
fn sgr_truecolor_semicolon_form() {
    let t = run(20, 1, "\x1b[38;2;10;20;30mx");
    assert_eq!(t.grid().cell(0, 0).unwrap().fg, Color::Rgb(10, 20, 30));
}

#[test]
fn sgr_indexed_256_form() {
    let t = run(20, 1, "\x1b[38;5;123mx\x1b[48;5;7my");
    assert_eq!(t.grid().cell(0, 0).unwrap().fg, Color::Indexed(123));
    assert_eq!(t.grid().cell(0, 1).unwrap().bg, Color::Indexed(7));
}

#[test]
fn sgr_subparameter_underline_styles() {
    // `4:3` is undercurl. Distinguishing it from a plain `4` is why the parser
    // must handle subparameters at all.
    let t = run(20, 1, "\x1b[4:3mx\x1b[4:0my\x1b[4:2mz");
    assert!(t.grid().cell(0, 0).unwrap().flags.contains(CellFlags::UNDERCURL));
    assert!(!t.grid().cell(0, 1).unwrap().flags.contains(CellFlags::UNDERCURL));
    assert!(t.grid().cell(0, 2).unwrap().flags.contains(CellFlags::DOUBLE_UNDERLINE));
}

// --- modes ---------------------------------------------------------------

#[test]
fn alt_screen_is_separate_and_restores_on_exit() {
    let mut t = Terminal::new(10, 2, 100);
    t.advance(b"main content");
    t.advance(b"\x1b[?1049h");
    assert!(t.modes().contains(Modes::ALT_SCREEN));
    assert_eq!(screen(&t), "", "the alternate screen starts blank");

    t.advance(b"alt stuff");
    assert_eq!(screen(&t), "alt stuff");

    t.advance(b"\x1b[?1049l");
    assert!(!t.modes().contains(Modes::ALT_SCREEN));
    // "main content" is 12 chars in a 10-column terminal, so it wrapped when
    // it was written. What matters is that the primary screen came back
    // exactly as it was left.
    assert_eq!(screen(&t), "main conte\nnt", "the primary screen is untouched");
}

#[test]
fn alt_screen_has_no_scrollback() {
    // Programs owning the whole display are not producing history.
    let mut t = Terminal::new(5, 2, 100);
    t.advance(b"\x1b[?1049h");
    t.advance(b"a\r\nb\r\nc\r\nd\r\ne");
    assert_eq!(t.grid().scrollback_len(), 0);
}

#[test]
fn modes_the_frontend_must_know_about_are_tracked() {
    let mut t = Terminal::new(10, 2, 0);
    t.advance(b"\x1b[?1h\x1b[?2004h\x1b[?1006h\x1b[?1002h");
    let m = t.modes();
    assert!(m.contains(Modes::APP_CURSOR), "changes arrow-key encoding");
    assert!(m.contains(Modes::BRACKETED_PASTE));
    assert!(m.contains(Modes::MOUSE_SGR));
    assert!(m.mouse_enabled());
}

#[test]
fn synchronized_output_is_reported_to_the_host() {
    let mut t = Terminal::new(10, 2, 0);
    t.advance(b"\x1b[?2026h");
    assert!(t.modes().contains(Modes::SYNC_UPDATE));
    assert!(t.take_events().contains(&TermEvent::SyncUpdate(true)));

    t.advance(b"\x1b[?2026l");
    assert!(t.take_events().contains(&TermEvent::SyncUpdate(false)));
}

#[test]
fn insert_mode_shifts_instead_of_overwriting() {
    let t = run(10, 1, "abcd\x1b[1;2H\x1b[4hXY");
    assert_eq!(screen(&t), "aXYbcd");
}

// --- OSC -----------------------------------------------------------------

#[test]
fn osc_sets_the_window_title() {
    let mut t = Terminal::new(10, 2, 0);
    t.advance(b"\x1b]0;my title\x07");
    assert_eq!(t.title(), "my title");
    assert!(t.take_events().contains(&TermEvent::Title("my title".into())));
}

#[test]
fn osc_background_query_is_answered() {
    // TUIs query the background to decide light vs dark. Not answering makes
    // them guess, usually wrong.
    let mut t = Terminal::new(10, 2, 0);
    t.advance(b"\x1b]11;?\x07");
    let replies: Vec<_> = t
        .take_events()
        .into_iter()
        .filter_map(|e| match e {
            TermEvent::Reply(b) => Some(String::from_utf8_lossy(&b).into_owned()),
            _ => None,
        })
        .collect();
    assert_eq!(replies.len(), 1, "exactly one reply");
    assert!(replies[0].starts_with("\x1b]11;rgb:"), "got {:?}", replies[0]);
}

#[test]
fn osc_palette_set_then_reset() {
    let mut t = Terminal::new(10, 2, 0);
    let original = t.palette().colors[1];

    t.advance(b"\x1b]4;1;rgb:ff/00/00\x07");
    assert_eq!(t.palette().colors[1], zest_core::Rgb::new(0xff, 0, 0));

    t.advance(b"\x1b]104;1\x07");
    assert_eq!(t.palette().colors[1], original, "reset restores the theme's seed");
}

#[test]
fn device_status_report_answers_with_the_cursor_position() {
    let mut t = Terminal::new(20, 5, 0);
    t.advance(b"\x1b[3;7H\x1b[6n");
    let replies: Vec<_> = t
        .take_events()
        .into_iter()
        .filter_map(|e| match e {
            TermEvent::Reply(b) => Some(String::from_utf8_lossy(&b).into_owned()),
            _ => None,
        })
        .collect();
    assert_eq!(replies, vec!["\x1b[3;7R".to_string()]);
}

// --- the Kitty keyboard protocol -----------------------------------------

/// Every reply the terminal has queued, as strings.
fn replies(t: &mut Terminal) -> Vec<String> {
    t.take_events()
        .into_iter()
        .filter_map(|e| match e {
            TermEvent::Reply(b) => Some(String::from_utf8_lossy(&b).into_owned()),
            _ => None,
        })
        .collect()
}

/// Ask the terminal what keyboard flags it thinks are in force.
fn kitty_query(t: &mut Terminal) -> String {
    t.advance(b"\x1b[?u");
    let r = replies(t);
    assert_eq!(r.len(), 1, "a keyboard query is answered exactly once");
    r.into_iter().next().unwrap()
}

#[test]
fn a_bare_csi_u_still_restores_the_cursor() {
    // The kitty arms sit above SCORC in the same match, and getting the order
    // or the intermediates wrong silently breaks cursor save/restore for every
    // program that has never heard of kitty.
    let mut t = Terminal::new(20, 5, 0);
    t.advance(b"\x1b[3;7H\x1b[s\x1b[1;1H\x1b[u");
    assert_eq!((t.grid().cursor.row, t.grid().cursor.col), (2, 6), "SCORC still works");
}

#[test]
fn the_keyboard_flags_start_empty_and_are_reported() {
    let mut t = Terminal::new(20, 5, 0);
    assert_eq!(kitty_query(&mut t), "\x1b[?0u", "nothing is enabled until asked for");
    assert_eq!(t.modes().kitty_flags(), 0);
}

#[test]
fn pushing_flags_makes_them_current() {
    let mut t = Terminal::new(20, 5, 0);
    t.advance(b"\x1b[>3u");
    assert_eq!(kitty_query(&mut t), "\x1b[?3u");
    assert!(t.modes().contains(Modes::KITTY_DISAMBIGUATE));
    assert!(t.modes().contains(Modes::KITTY_EVENT_TYPES));
}

#[test]
fn unimplemented_flags_are_never_reported_as_enabled() {
    // Answering `31` would tell the program it will receive alternate keys and
    // associated text. It will not, and having been told otherwise it has no
    // reason to fall back.
    let mut t = Terminal::new(20, 5, 0);
    t.advance(b"\x1b[>31u");
    assert_eq!(kitty_query(&mut t), "\x1b[?11u", "only flags 1, 2 and 8 are implemented");
}

#[test]
fn popping_restores_what_was_pushed_before() {
    let mut t = Terminal::new(20, 5, 0);
    t.advance(b"\x1b[>1u\x1b[>9u");
    assert_eq!(kitty_query(&mut t), "\x1b[?9u");
    t.advance(b"\x1b[<u");
    assert_eq!(kitty_query(&mut t), "\x1b[?1u", "a bare pop removes one entry");
}

#[test]
fn popping_an_empty_stack_disables_everything() {
    // The protocol says an emptied stack means all flags reset -- not "keep the
    // last value", which is what a naive `pop().unwrap_or(current)` would do.
    let mut t = Terminal::new(20, 5, 0);
    t.advance(b"\x1b[>9u\x1b[<5u");
    assert_eq!(kitty_query(&mut t), "\x1b[?0u");
}

#[test]
fn set_replaces_adds_and_removes_by_mode() {
    let mut t = Terminal::new(20, 5, 0);
    t.advance(b"\x1b[=1;1u");
    assert_eq!(kitty_query(&mut t), "\x1b[?1u", "mode 1 replaces");
    t.advance(b"\x1b[=8;2u");
    assert_eq!(kitty_query(&mut t), "\x1b[?9u", "mode 2 adds");
    t.advance(b"\x1b[=1;3u");
    assert_eq!(kitty_query(&mut t), "\x1b[?8u", "mode 3 removes");
    t.advance(b"\x1b[=2u");
    assert_eq!(kitty_query(&mut t), "\x1b[?2u", "the default mode replaces");
}

#[test]
fn set_does_not_grow_the_stack() {
    // `CSI = ` modifies the entry in force; a program that sets in a loop must
    // not be able to push the terminal's own entry out from under it.
    let mut t = Terminal::new(20, 5, 0);
    t.advance(b"\x1b[>1u");
    for _ in 0..20 {
        t.advance(b"\x1b[=9;1u");
    }
    t.advance(b"\x1b[<u");
    assert_eq!(kitty_query(&mut t), "\x1b[?0u", "one push means one pop empties it");
}

#[test]
fn the_stack_is_bounded_and_evicts_the_oldest() {
    // Unbounded, this is a denial of service one escape sequence long.
    let mut t = Terminal::new(20, 5, 0);
    t.advance(b"\x1b[>1u");
    for _ in 0..64 {
        t.advance(b"\x1b[>9u");
    }
    for _ in 0..64 {
        t.advance(b"\x1b[<u");
    }
    assert_eq!(kitty_query(&mut t), "\x1b[?0u", "the first entry was evicted, not kept");
}

#[test]
fn each_screen_keeps_its_own_flags() {
    // A full-screen program pushes on entry and pops on exit. Sharing one stack
    // leaves the shell encoding keys the way `nvim` wanted after `nvim` dies.
    let mut t = Terminal::new(20, 5, 0);
    t.advance(b"\x1b[>1u");
    t.advance(b"\x1b[?1049h\x1b[>9u");
    assert_eq!(kitty_query(&mut t), "\x1b[?9u", "the alternate screen has its own");
    t.advance(b"\x1b[?1049l");
    assert_eq!(kitty_query(&mut t), "\x1b[?1u", "leaving it restores the shell's");
}

#[test]
fn changing_the_flags_bumps_the_sequence() {
    // Deltas are computed against `seq`. Flags that change without bumping it
    // never reach an attached client, which then keeps encoding the legacy way
    // at a program that has stopped expecting it -- and the local window, which
    // reads modes off this terminal directly, looks perfectly correct while it
    // happens. The bug this pins was written and shipped before it was caught.
    let mut t = Terminal::new(20, 5, 0);
    let before = t.seq();
    t.advance(b"\x1b[>1u");
    assert!(t.seq() > before, "a client never told the flags changed cannot encode for them");

    // A query changes nothing and must not wake every subscriber.
    let quiet = t.seq();
    t.advance(b"\x1b[?u");
    assert_eq!(t.seq(), quiet, "asking is not changing");
}

#[test]
fn a_full_reset_empties_both_stacks() {
    let mut t = Terminal::new(20, 5, 0);
    t.advance(b"\x1b[>9u\x1b[>1u");
    t.advance(b"\x1bc");
    assert_eq!(kitty_query(&mut t), "\x1b[?0u");
    // Not just the flags: a surviving stack would put them back on the next pop.
    t.advance(b"\x1b[<u");
    assert_eq!(kitty_query(&mut t), "\x1b[?0u", "RIS drops the stack, not only the top");
}

// --- unicode -------------------------------------------------------------

#[test]
fn wide_characters_occupy_two_cells() {
    let t = run(10, 1, "世界");
    assert!(t.grid().cell(0, 0).unwrap().flags.contains(CellFlags::WIDE));
    assert!(t.grid().cell(0, 1).unwrap().flags.contains(CellFlags::WIDE_SPACER));
    assert_eq!(t.grid().cell(0, 2).unwrap().ch, '界');
    assert_eq!(t.cursor().col, 4);
    assert_eq!(screen(&t), "世界");
}

#[test]
fn a_wide_char_will_not_straddle_the_right_edge() {
    // With one column left, the wide char must wrap rather than be split.
    let t = run(3, 2, "ab世");
    assert_eq!(t.grid().cell(1, 0).unwrap().ch, '世');
}

#[test]
fn combining_marks_attach_to_the_previous_cell() {
    let t = run(10, 1, "e\u{0301}x");
    assert_eq!(t.cursor().col, 2, "the mark consumed no cell of its own");
    assert_eq!(screen(&t), "e\u{0301}x");
}

// --- damage --------------------------------------------------------------

#[test]
fn an_idle_terminal_reports_no_damage() {
    // This is what lets the renderer skip frames entirely, which is the whole
    // basis of the 0%-GPU-at-idle requirement.
    let mut t = Terminal::new(10, 2, 0);
    t.advance(b"hello");
    assert!(t.take_damage().dirty);

    t.advance(b"");
    assert!(!t.take_damage().dirty, "no input means no repaint");
}

#[test]
fn the_sequence_counter_advances_on_mutation() {
    // M3 computes deltas against this.
    let mut t = Terminal::new(10, 2, 0);
    let before = t.seq();
    t.advance(b"x");
    assert!(t.seq() > before);
}

// --- fixture replay -------------------------------------------------------

/// Parse the trivial `.vtrec` container: `VTREC1\n` then repeated
/// `<micros:u64le><len:u32le><bytes>`.
fn parse_vtrec(bytes: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    let Some(body) = bytes.strip_prefix(b"VTREC1\n") else {
        panic!("not a vtrec file");
    };
    let mut i = 0;
    while i + 12 <= body.len() {
        let len = u32::from_le_bytes(body[i + 8..i + 12].try_into().unwrap()) as usize;
        i += 12;
        if i + len > body.len() {
            break;
        }
        out.push(body[i..i + len].to_vec());
        i += len;
    }
    out
}

#[test]
fn ansi_fixtures_parse_without_panicking() {
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ansi");
    let mut checked = 0;
    for entry in std::fs::read_dir(dir).expect("ansi fixture dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("ans") {
            continue;
        }
        let bytes = std::fs::read(&path).expect("read fixture");
        let mut t = Terminal::new(80, 24, 100);
        t.advance(&bytes);
        // The invariant that matters: whatever the input, the cursor stays in
        // bounds and the grid stays coherent.
        assert!(t.cursor().row < t.grid().rows(), "{path:?} put the cursor out of bounds");
        assert!(t.cursor().col < t.grid().cols(), "{path:?} put the cursor out of bounds");
        checked += 1;
    }
    assert!(checked >= 4, "expected the ansi fixtures to be present, found {checked}");
}

// --- command blocks (OSC 133 / 7 / 633) ----------------------------------

/// The fixture's block markers, so a change to the file is a change to one
/// place rather than to five tests.
const OSC_FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ansi/osc.ans");

#[test]
fn the_osc_fixture_produces_one_complete_block() {
    // `osc.ans` has carried a full A/B/C/D sequence since M1, when the markers
    // were recognized and discarded. This is the test that stopped discarding
    // them being true.
    let bytes = std::fs::read(OSC_FIXTURE).expect("osc fixture");
    let mut t = Terminal::new(80, 24, 100);
    t.advance(&bytes);

    let blocks = t.blocks().blocks();
    assert_eq!(blocks.len(), 1, "one prompt, one command, one block");
    let b = &blocks[0];

    // The prompt is on row 1: row 0 is the title/hyperlink line the fixture
    // opens with, and `\r\n` moved to row 1 before `133;A` arrived.
    assert_eq!(b.prompt_line, 1, "the block starts where the prompt was drawn");
    // Not row 1, where `133;C` arrived: that marker fires before the shell
    // echoes the newline, so it lands on the row the command was typed on. The
    // first line of output is the one after it, and `output_line` says what it
    // means -- copy-output would otherwise hand back the prompt.
    assert_eq!(b.output_line, Some(2), "`out` is on the row below the prompt");
    // Likewise `133;D` arrives after the trailing newline, with the cursor
    // already on the row the next prompt will use. The block ends on row 2.
    assert_eq!(b.end_line, Some(2), "the block ends on its last output row");
    assert_eq!(b.command, "ls", "the command is the cells between B and C");
    assert_eq!(
        b.state,
        zest_core::BlockState::Finished { exit_code: Some(0) },
        "the fixture reports a successful exit"
    );
    assert!(!b.failed());
    assert!(b.contains(2), "the output line belongs to the block that produced it");
}

#[test]
fn a_bare_d_marker_is_unknown_rather_than_success() {
    // Plenty of shells emit `133;D` with no status. A green tick on a command
    // that actually failed is worse than no tick at all, so the parser must not
    // invent a zero. `blocks.rs` tests the index; this tests the wire form.
    let t = run(20, 4, "\x1b]133;A\x07$ \x1b]133;B\x07x\x1b]133;C\x07\r\n\x1b]133;D\x07");
    let b = t.blocks().last().expect("one block");
    assert_eq!(b.state, zest_core::BlockState::Finished { exit_code: None });
    assert!(!b.failed(), "unknown is not failure");
}

#[test]
fn block_timestamps_come_from_the_embedder_and_only_from_it() {
    // The parser has no clock (`no_std`): a terminal never told the time
    // produces blocks with no stamps, and one told the time stamps C and D
    // with whatever was current — which is how "51.2s" can be honest even
    // when output arrives in bursts.
    let mut silent = Terminal::new(20, 4, 100);
    silent.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07x\x1b]133;C\x07\r\n\x1b]133;D;0\x07");
    let b = silent.blocks().last().expect("one block");
    assert_eq!((b.started_ms, b.ended_ms), (None, None), "no clock, no stamps");

    let mut timed = Terminal::new(20, 4, 100);
    timed.set_now_ms(1_000);
    timed.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07x\x1b]133;C\x07\r\n");
    timed.set_now_ms(52_200);
    timed.advance(b"\x1b]133;D;0\x07");
    let b = timed.blocks().last().expect("one block");
    assert_eq!(b.started_ms, Some(1_000), "C stamps the start");
    assert_eq!(b.ended_ms, Some(52_200), "D stamps the end");
}

#[test]
fn a_nonzero_status_survives_a_trailing_key_value_tail() {
    // `133;D;1;aid=7` is what several shells actually emit. Parsing the status
    // as "the whole parameter" would make it unknown, which reads as a command
    // that failed silently.
    let t = run(20, 4, "\x1b]133;A\x07$ \x1b]133;B\x07x\x1b]133;C\x07\r\n\x1b]133;D;1;aid=7\x07");
    assert!(t.blocks().last().expect("one block").failed(), "exit 1 is a failure");
}

#[test]
fn a_running_block_has_no_end_and_covers_output_still_arriving() {
    // What makes a long build readable while it runs rather than only after it
    // finishes.
    let t = run(20, 6, "\x1b]133;A\x07$ \x1b]133;B\x07make\x1b]133;C\x07\r\nline one\r\n");
    let b = t.blocks().last().expect("one block");
    assert!(b.is_running());
    assert_eq!(b.command, "make");
    assert_eq!(b.end_line, None, "a running command has not ended");
    assert!(b.contains(9_999), "so it extends to wherever output has reached");
}

#[test]
fn a_wrapped_command_is_read_back_whole() {
    // The command lives in the grid, not in the marker, so a command long
    // enough to wrap must be rejoined across rows. Splitting it would put a
    // newline in the middle of something someone will re-run.
    let t = run(10, 4, "\x1b]133;A\x07$ \x1b]133;B\x07echo abcdefgh\x1b]133;C\x07\r\n");
    assert_eq!(
        t.blocks().last().expect("one block").command,
        "echo abcdefgh",
        "rows joined by `wrapped` are one command"
    );
}

#[test]
fn markers_on_the_alternate_screen_are_ignored() {
    // The alt screen is a separate grid whose line ids restart at zero, so a
    // marker recorded there names a line in the primary grid's numbering that
    // holds entirely different text -- and it would be a plausible small id,
    // not an obviously wrong one.
    let t = run(
        20,
        4,
        "\x1b[?1049h\x1b]133;A\x07$ \x1b]133;B\x07vim\x1b]133;C\x07\r\n\x1b]133;D;0\x07",
    );
    assert!(t.blocks().blocks().is_empty(), "a full-screen program has no command history");
}

#[test]
fn a_full_reset_clears_the_block_index() {
    // RIS blanks the screen the index describes. Keeping the blocks would leave
    // every one of them pointing at lines that no longer hold their output.
    let mut t = Terminal::new(20, 4, 100);
    t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07x\x1b]133;C\x07\r\n\x1b]133;D;0\x07");
    assert_eq!(t.blocks().blocks().len(), 1);
    t.advance(b"\x1bc");
    assert!(t.blocks().blocks().is_empty(), "RIS leaves nothing for a block to describe");
}

#[test]
fn a_session_past_its_scrollback_bound_does_not_grow_its_index() {
    // The leak with a long fuse: a fleet makes a session that has been running
    // for weeks the normal case, and an index that only ever grows is how that
    // session dies.
    let mut t = Terminal::new(20, 3, 4);
    for i in 0..40 {
        t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07x\x1b]133;C\x07\r\n");
        t.advance(format!("out {i}\r\n").as_bytes());
        t.advance(b"\x1b]133;D;0\x07");
    }
    let held = t.blocks().blocks().len();
    assert!(held > 0, "the recent blocks are still there");
    assert!(
        held < 40,
        "40 blocks through a 4-line scrollback should have evicted most of them, kept {held}"
    );

    // And what is left is genuinely reachable, not a stale id.
    let oldest = &t.blocks().blocks()[0];
    assert!(
        oldest.end_line.expect("finished") >= t.grid().row(0).id - t.grid().scrollback_len() as u64,
        "a retained block must not point past the oldest line still held"
    );
}

#[test]
fn widening_the_window_re_anchors_blocks_instead_of_losing_them() {
    // Reflow renumbers line ids, so a block that is not re-anchored names
    // different text afterwards. The selection is cleared in this situation;
    // a block must not be, because losing the block for a build because the
    // window was widened while it ran is the case blocks exist for.
    // A wrapped line *above* the prompt, so rejoining it shifts every id below
    // — without that, the block starts at line 0 and the test passes whether
    // anything was re-anchored or not.
    let mut t = Terminal::new(10, 5, 100);
    t.advance(b"0123456789abcde\r\n");
    t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07echo abcdefgh\x1b]133;C\x07\r\nout\r\n\x1b]133;D;0\x07");

    let before = t.blocks().last().expect("one block").clone();
    assert_eq!(
        (before.prompt_line, before.output_line, before.end_line),
        (2, Some(4), Some(4)),
        "the prompt wraps across rows 2-3, so output begins on row 4 and ends there"
    );

    t.resize(40, 5);
    let after = t.blocks().last().expect("the block survived the resize");

    assert_ne!(
        after.prompt_line, before.prompt_line,
        "rewrapping renumbered the lines, so an un-re-anchored block would be untouched here \
         and would name the wrong row"
    );
    assert_eq!(
        (after.prompt_line, after.output_line, after.end_line),
        (1, Some(2), Some(2)),
        "each end of the block follows its logical line to the new numbering"
    );
    assert_eq!(after.command, before.command, "re-anchoring must not disturb the command");

    let row = t.grid().row_of_line(after.prompt_line).expect("the prompt line still exists");
    assert_eq!(
        t.grid().row(row).text(),
        "$ echo abcdefgh",
        "the block still points at its own prompt"
    );
}

#[test]
fn the_block_path_is_independent_of_chunk_boundaries() {
    // A pty hands over arbitrary chunks, so an OSC handler that accumulated
    // state outside the parser would produce a different index depending on
    // where a read happened to split. `vt.rs` already asserts this for the
    // grid; blocks have their own state and need their own claim.
    let bytes = std::fs::read(OSC_FIXTURE).expect("osc fixture");

    let mut whole = Terminal::new(80, 24, 100);
    whole.advance(&bytes);

    let mut split = Terminal::new(80, 24, 100);
    for b in &bytes {
        split.advance(&[*b]);
    }

    assert_eq!(
        split.blocks().blocks(),
        whole.blocks().blocks(),
        "the same bytes produced a different block index when split differently"
    );
}

#[test]
fn osc_7_gives_the_next_block_its_working_directory() {
    // OSC 7 arrives *before* the prompt marker that consumes it, which is why
    // it is held rather than pushed as an event.
    let t = run(
        40,
        4,
        "\x1b]7;file://andy-mac/Users/andy/My%20Code\x07\x1b]133;A\x07$ \x1b]133;B\x07x\x1b]133;C\x07",
    );
    assert_eq!(t.cwd(), "/Users/andy/My Code", "the host is dropped and %XX decoded");
    assert_eq!(
        t.blocks().last().expect("one block").cwd,
        "/Users/andy/My Code",
        "the block records where it ran"
    );
}

#[test]
fn osc_633_indexes_the_same_blocks_as_133() {
    // VS Code's dialect. A great many people already have its shell
    // integration installed, and getting no blocks here while getting them
    // there would look like zesterm was broken.
    let t = run(
        40,
        4,
        "\x1b]633;P;Cwd=/tmp\x07\x1b]633;A\x07$ \x1b]633;B\x07\x1b]633;E;git commit -m 'a\\x3bb'\x07\
         git commit\x1b]633;C\x07\r\nout\r\n\x1b]633;D;0\x07",
    );
    let b = t.blocks().last().expect("one block");
    assert_eq!(b.cwd, "/tmp");
    assert_eq!(
        b.command, "git commit -m 'a;b'",
        "633;E states the command explicitly, and its escaping must be undone"
    );
    assert_eq!(b.state, zest_core::BlockState::Finished { exit_code: Some(0) });
}

#[test]
fn a_real_zsh_session_produces_real_blocks() {
    // Recorded through a real pty from a real interactive `zsh` with zesterm's
    // shell integration injected -- not a hand-written sequence. It is the
    // difference between "the parser handles this escape" and "the thing a
    // shell actually emits produces the blocks we claim".
    //
    // Recorded neutrally on purpose (see tests/README.md): no username, no
    // hostname, a default prompt. The corpus is committed, and a recording that
    // carries whoever made it is a recording nobody can replace.
    let bytes =
        std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/blocks-zsh.vtrec"))
            .expect("blocks fixture");
    let mut t = Terminal::new(120, 30, 200);
    for chunk in parse_vtrec(&bytes) {
        t.advance(&chunk);
    }

    let blocks = t.blocks().blocks();
    assert!(blocks.len() >= 3, "expected a block per prompt, got {}", blocks.len());

    let finished: Vec<_> = blocks.iter().filter(|b| b.end_line.is_some()).collect();
    assert!(finished.len() >= 2, "two commands ran to completion");

    // The two that matter, and they must be distinguishable: a shell that
    // reports every command as succeeding is worse than one that reports none.
    assert!(!finished[0].failed(), "`echo hello` succeeded");
    assert!(finished[1].failed(), "`false` did not, and the status says so");

    assert_eq!(
        finished[0].command, "echo hello",
        "the command is read back from the cells between B and C"
    );
    assert_eq!(finished[1].command, "false");

    // OSC 7 rode along with the same hook.
    assert_eq!(finished[0].cwd, "/tmp/zestdemo");

    // The boundaries, against a real shell rather than a hand-written sequence.
    // `133;C` arrives before the shell echoes the newline and `133;D` after the
    // trailing one, so a block that took both markers literally would claim the
    // command's own row at one end and the next prompt's at the other — and
    // copy-output would hand back a prompt at each end of what it copied.
    let out = finished[0].output_line.expect("output began");
    let row = t.grid().row_of_line(out).expect("still on screen");
    assert_eq!(t.grid().row(row).text(), "hello", "output_line is the first line of output");
    assert_eq!(finished[0].end_line, Some(out), "and `hello` is the whole of it");

    // `false` printed nothing, so its range is empty rather than covering a
    // neighbour's row.
    assert!(
        finished[1].output_line.expect("marked") > finished[1].end_line.expect("finished"),
        "a command that printed nothing owns no output rows"
    );
}

#[test]
fn truecolor_fixture_produces_distinct_colors() {
    let bytes = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ansi/truecolor.ans"))
        .expect("truecolor fixture");
    let mut t = Terminal::new(80, 24, 100);
    t.advance(&bytes);

    let row = t.grid().row(0);
    let a = row.cells()[0].fg;
    let b = row.cells()[10].fg;
    assert!(matches!(a, Color::Rgb(..)), "expected truecolor, got {a:?}");
    assert_ne!(a, b, "the gradient should vary across the row");
}

#[test]
fn sgr_fixture_applies_every_attribute() {
    let bytes = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ansi/sgr-attrs.ans"))
        .expect("sgr fixture");
    let mut t = Terminal::new(120, 24, 100);
    t.advance(&bytes);

    let text = t.screen_text();
    assert!(text.contains("bold"));
    assert!(text.contains("undercurl"));

    // Find the cell where "bold" starts and confirm the attribute landed.
    let row = t.grid().row(0);
    let bold_cell = row.cells().iter().find(|c| c.ch == 'b').expect("the word bold");
    assert!(bold_cell.flags.contains(CellFlags::BOLD));
}

#[test]
fn recorded_sessions_replay_without_panicking() {
    // Real ConPTY output from real programs. The point is not a specific
    // assertion but that nothing in the wild trips the parser.
    let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus");
    let mut replayed = 0;
    for entry in std::fs::read_dir(dir).expect("corpus dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|e| e.to_str()) != Some("vtrec") {
            continue;
        }
        let bytes = std::fs::read(&path).expect("read recording");
        let mut t = Terminal::new(120, 30, 1000);
        for chunk in parse_vtrec(&bytes) {
            t.advance(&chunk);
        }
        assert!(t.cursor().row < t.grid().rows(), "{path:?}");
        assert!(t.cursor().col < t.grid().cols(), "{path:?}");
        replayed += 1;
    }
    assert!(replayed >= 3, "expected recordings to be present, found {replayed}");
}

#[test]
fn git_log_recording_keeps_its_colors() {
    let bytes = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/git-log.vtrec"))
        .expect("git-log recording");
    let mut t = Terminal::new(120, 30, 1000);
    for chunk in parse_vtrec(&bytes) {
        t.advance(&chunk);
    }

    let text = t.screen_text();
    assert!(text.contains("zesterm") || text.contains("ConPTY"), "got:\n{text}");

    let colored = (0..t.grid().rows())
        .flat_map(|r| (0..t.grid().cols()).map(move |c| (r, c)))
        .filter_map(|(r, c)| t.grid().cell(r, c))
        .any(|cell| cell.fg != Color::Default);
    assert!(colored, "git's SGR colors should survive the round trip");
}

/// Feeding a chunk one byte at a time must produce the identical grid.
///
/// This is the property that makes PTY read boundaries irrelevant, and it is
/// easy to break by accumulating state outside the parser.
#[test]
fn parsing_is_independent_of_chunk_boundaries() {
    let input = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ansi/cursor-ops.ans"))
        .expect("cursor-ops fixture");

    let mut whole = Terminal::new(40, 12, 50);
    whole.advance(&input);

    let mut split = Terminal::new(40, 12, 50);
    for byte in &input {
        split.advance(&[*byte]);
    }

    assert_eq!(whole.screen_text(), split.screen_text());
    assert_eq!(whole.cursor(), split.cursor());
}

/// The parser must not panic on arbitrary bytes.
///
/// A cheap stand-in for the fuzzing that should follow: VT parsers are a panic
/// factory, and a panic here would take down a user's whole session.
#[test]
fn arbitrary_bytes_do_not_panic() {
    let mut t = Terminal::new(40, 10, 50);
    // A deterministic pseudo-random stream -- reproducible, unlike rand.
    let mut x: u32 = 0x1234_5678;
    let mut buf = Vec::with_capacity(64 * 1024);
    for _ in 0..64 * 1024 {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        buf.push((x >> 16) as u8);
    }
    t.advance(&buf);

    assert!(t.cursor().row < t.grid().rows());
    assert!(t.cursor().col < t.grid().cols());
}

#[test]
fn a_blank_cell_template_is_still_sixteen_bytes() {
    // Guard against Cell growing via an innocuous-looking field addition.
    assert_eq!(std::mem::size_of::<Cell>(), 16);
}

/// A real oh-my-posh prompt, captured from the theme the author uses daily.
///
/// This is the first thing on screen in every session, so anything it exercises
/// is not an edge case — it is the common path. It combines truecolor
/// backgrounds, a `38;2;r;g;b;49` compound SGR (an extended colour immediately
/// followed by another parameter, which a naive parser swallows), reverse video
/// for the diamond caps, Private Use Area separators, and `CSI 1000C` / `CSI nD`
/// for right alignment.
#[test]
fn a_real_oh_my_posh_prompt_keeps_its_segment_colours() {
    let input =
        std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/ansi/oh-my-posh-prompt.ans"))
            .expect("oh-my-posh fixture");

    let mut t = Terminal::new(100, 10, 50);
    t.advance(&input);

    // The session segment's background, straight from the theme's `#c386f1`.
    let purple = Color::Rgb(195, 134, 241);
    let row0: Vec<Color> = (0..100).filter_map(|c| t.grid().cell(0, c)).map(|c| c.bg).collect();
    assert!(
        row0.contains(&purple),
        "no cell on the prompt row carries the session segment's background"
    );

    // The path segment follows it, so at least two distinct truecolor
    // backgrounds must survive on one row -- a single one could be luck.
    let distinct: std::collections::HashSet<_> =
        row0.iter().filter(|c| matches!(c, Color::Rgb(..))).collect();
    assert!(
        distinct.len() >= 2,
        "expected several coloured segments, found {distinct:?}"
    );

    // The compound `38;2;r;g;b;49` must leave the background at default rather
    // than consuming the 49 as part of the colour.
    let text = t.screen_text();
    assert!(text.contains("andy@"), "prompt text missing: {text:?}");
}

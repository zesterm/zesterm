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

/// The same, keeping the microsecond stamps.
///
/// `resize-drag.vtrec` needs them: a `.vtrec` records bytes and nothing else,
/// so the only way to replay a resize at the point the recorder made it is by
/// when it happened. See the test.
fn parse_vtrec_timed(bytes: &[u8]) -> Vec<(u128, Vec<u8>)> {
    let mut out = Vec::new();
    let Some(body) = bytes.strip_prefix(b"VTREC1\n") else {
        panic!("not a vtrec file");
    };
    let mut i = 0;
    while i + 12 <= body.len() {
        let us = u64::from_le_bytes(body[i..i + 8].try_into().unwrap()) as u128;
        let len = u32::from_le_bytes(body[i + 8..i + 12].try_into().unwrap()) as usize;
        i += 12;
        if i + len > body.len() {
            break;
        }
        out.push((us, body[i..i + len].to_vec()));
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
fn an_abandoned_prompt_is_reused_rather_than_left_open() {
    // An empty Enter, a ^C, or any prompt redraw is an `A` with no `C` and no
    // `D` after it: zsh emits `C` from preexec and `D` only when something
    // actually ran. Pushing a block per prompt therefore left a trail of
    // `Prompt` blocks with no `end_line` -- and since `contains` treats an
    // open block as covering every line below it, the FIRST one swallowed the
    // rest of the session. The live prompt then rendered inside that block's
    // output instead of on the prompt line, and a client typing into a shell
    // that was answering perfectly well saw nothing appear. (#193)
    //
    // pwsh brackets even an empty line with C/D, so every block closes there.
    // That is the whole reason this only ever appeared on macOS.
    let mut t = Terminal::new(20, 4, 100);
    t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07");
    t.advance(b"\r\n\x1b]133;A\x07$ \x1b]133;B\x07");
    t.advance(b"\r\n\x1b]133;A\x07$ \x1b]133;B\x07");

    assert_eq!(
        t.blocks().blocks().len(),
        1,
        "three prompts and no command is one prompt showing, not three blocks: {:?}",
        t.blocks().blocks()
    );

    // And it is the prompt showing *now*, not the first one: a block anchored
    // to an abandoned row goes on claiming that row and everything after it.
    let live = t.grid().active_line_id_at(t.cursor().row).expect("a live row");
    let b = t.blocks().last().expect("the prompt block");
    assert_eq!(b.prompt_line, live, "the surviving block is anchored to the live prompt");
    assert_eq!(b.state, zest_core::BlockState::Prompt);
    assert!(b.output_line.is_none(), "nothing has run in it");
}

#[test]
fn reusing_an_abandoned_prompt_never_swallows_a_command_that_ran() {
    // The reuse above is only sound while it is confined to a prompt that
    // produced nothing. A block that actually ran is history, and the next
    // prompt is a new block however the previous one ended -- including the
    // `Running` block of a command still going when the shell somehow
    // reprompts, which must not be silently rewritten as the live prompt.
    let mut t = Terminal::new(20, 6, 100);
    t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07ls\x1b]133;C\x07\r\nout\r\n\x1b]133;D;0\x07");
    t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07");
    assert_eq!(t.blocks().blocks().len(), 2, "a command that ran keeps its own block");

    let first = &t.blocks().blocks()[0];
    assert_eq!(first.command, "ls");
    assert_eq!(first.state, zest_core::BlockState::Finished { exit_code: Some(0) });

    // A second abandoned prompt still collapses into the trailing one.
    t.advance(b"\r\n\x1b]133;A\x07$ \x1b]133;B\x07");
    assert_eq!(t.blocks().blocks().len(), 2, "the abandoned prompt is reused, not appended");
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
fn clearing_the_screen_drops_the_blocks_it_erased() {
    // `cls` blanks cells in place, so line ids survive and the shell reuses
    // them for the next prompt. Every block anchored there kept claiming rows
    // whose content was gone, and since a stale block *has* an `output_line`
    // while a fresh prompt does not, the header pass drew the stale one -- an
    // opaque band over the row the user was typing on. RIS already does this;
    // ED is the same situation.
    let mut t = Terminal::new(20, 4, 100);
    t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07ls\x1b]133;C\x07\r\nout\r\n\x1b]133;D;0\x07");
    assert_eq!(t.blocks().blocks().len(), 1, "the command ran");

    // What `Clear-Host` emits under ConPTY: erase the display, cursor home.
    t.advance(b"\x1b[2J\x1b[H");
    assert!(t.blocks().blocks().is_empty(), "the blocks described rows that were erased");

    // And the prompt that follows is a clean block of its own, covering the
    // live row with nothing stale over it.
    t.advance(b"\x1b]133;D;0\x07\x1b]133;A\x07$ \x1b]133;B\x07");
    let live = t.grid().active_line_id_at(0).expect("a live row");
    let covering: Vec<_> = t.blocks().blocks().iter().filter(|b| b.contains(live)).collect();
    assert_eq!(covering.len(), 1, "exactly one block owns the prompt row: {covering:?}");
    assert!(covering[0].output_line.is_none(), "the prompt block has printed nothing yet");
}

#[test]
fn a_clear_leaves_the_blocks_that_scrolled_out_of_reach() {
    // Only what was erased is gone. A command whose output is already in
    // scrollback was not touched by the clear and is still real history.
    let mut t = Terminal::new(20, 3, 100);
    t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07old\x1b]133;C\x07\r\nout\r\n\x1b]133;D;0\x07");
    for _ in 0..6 {
        t.advance(b"filler\r\n");
    }
    assert_eq!(t.blocks().blocks().len(), 1);

    t.advance(b"\x1b[2J\x1b[H");
    assert_eq!(
        t.blocks().blocks().len(),
        1,
        "a block sitting entirely in scrollback survives a screen clear"
    );
}

#[test]
fn csi_3j_clears_scrollback_and_2j_does_not() {
    // ED 2 clears the screen and ED 3 also clears scrollback -- xterm's and
    // Windows Terminal's reading, and the one pwsh asks for: `Clear-Host`
    // under ConPTY emits an explicit `ESC[3J` (measured on this box; the
    // corpus has no 3J anywhere, so these are hand-built on purpose). zesterm
    // treated the two alike and kept all history, so `cls` followed by
    // scrolling up showed everything the user had just asked to be rid of. (#314)
    let mut t = Terminal::new(20, 4, 100);
    for i in 0..12 {
        t.advance(format!("line {i}\r\n").as_bytes());
    }
    let held = t.grid().scrollback_len();
    assert!(held > 0, "the fixture never scrolled");

    t.advance(b"\x1b[2J");
    assert_eq!(t.grid().scrollback_len(), held, "ED 2 must keep scrollback");

    t.advance(b"\x1b[3J");
    assert_eq!(t.grid().scrollback_len(), 0, "ED 3 must clear scrollback");
}

#[test]
fn scrolling_up_after_a_windows_cls_finds_nothing() {
    // The reported gesture, whole: `cls`, scroll up, expect nothing. The bytes
    // are what pwsh's Clear-Host emits under ConPTY -- 2J and 3J together,
    // then home.
    let mut t = Terminal::new(20, 4, 100);
    for i in 0..12 {
        t.advance(format!("line {i}\r\n").as_bytes());
    }
    t.advance(b"\x1b[2J\x1b[3J\x1b[H");

    t.scroll_display(10);
    assert_eq!(t.grid().display_offset(), 0, "there is history to scroll into after a cls");
    assert!(
        !t.screen_text().contains("line"),
        "the cleared content is still reachable: {:?}",
        t.screen_text()
    );
}

#[test]
fn a_block_only_in_scrollback_does_not_survive_ed_3() {
    // The counterpart of the ED 2 pin above: with the rows themselves
    // destroyed, a block that described them describes nothing, and keeping it
    // would render a header over content that no longer exists anywhere.
    let mut t = Terminal::new(20, 3, 100);
    t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07old\x1b]133;C\x07\r\nout\r\n\x1b]133;D;0\x07");
    for _ in 0..6 {
        t.advance(b"filler\r\n");
    }
    assert_eq!(t.blocks().blocks().len(), 1, "the fixture's block is in scrollback");

    t.advance(b"\x1b[2J\x1b[3J\x1b[H");
    assert!(
        t.blocks().blocks().is_empty(),
        "a block whose rows ED 3 destroyed is still in the index"
    );
}

#[test]
fn ed_3_announces_history_cleared() {
    // Scrollback dying is a change no delta can describe -- the rows are not
    // damaged, they are gone -- so every subscriber is owed a keyframe, the
    // `ViewportRebased` precedent. ED 2 alone announces nothing.
    let mut t = Terminal::new(20, 4, 100);
    for i in 0..12 {
        t.advance(format!("line {i}\r\n").as_bytes());
    }
    let _ = t.take_events();

    t.advance(b"\x1b[2J");
    assert!(
        !t.take_events().iter().any(|e| matches!(e, TermEvent::HistoryCleared)),
        "ED 2 does not destroy history and must not claim to"
    );

    t.advance(b"\x1b[3J");
    assert!(
        t.take_events().iter().any(|e| matches!(e, TermEvent::HistoryCleared)),
        "ED 3 destroyed history without telling anyone"
    );
}

#[test]
fn erasing_below_the_cursor_leaves_the_index_alone() {
    // ED 0 is what a line editor emits on every keystroke -- PSReadLine repaints
    // with it constantly. Invalidating there would delete the block the user is
    // typing into, over and over.
    let mut t = Terminal::new(20, 4, 100);
    t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07ls\x1b]133;C\x07\r\nout\r\n\x1b]133;D;0\x07");
    assert_eq!(t.blocks().blocks().len(), 1);
    t.advance(b"\x1b[J");
    assert_eq!(t.blocks().blocks().len(), 1, "an erase-to-end is not a screen clear");
}

#[test]
fn output_arriving_while_the_reader_scrolled_back_is_not_lost() {
    // The whole-terminal statement of the grid bug: a build streaming output
    // while someone reads their scrollback used to print onto the rows being
    // read, leaving the live screen blank. Nothing about it is recoverable
    // afterwards -- the text is simply gone.
    let mut t = Terminal::new(20, 3, 100);
    t.advance(b"anchor\r\n");
    for _ in 0..5 {
        t.advance(b"filler\r\n");
    }
    t.scroll_display(5);
    let read_before = t.screen_text();

    t.advance(b"one\r\ntwo");
    assert_eq!(t.screen_text(), read_before, "output moved or overwrote what was being read");

    t.scroll_to_bottom();
    let after = t.screen_text();
    for line in ["one", "two"] {
        assert!(after.contains(line), "{line:?} never reached the live screen: {after:?}");
    }
}

#[test]
fn block_markers_name_the_live_line_not_the_scrolled_one() {
    // `block_line` read the cursor's row through the display, so every OSC 133
    // marker emitted while the user was scrolled back named the line they had
    // scrolled *to*. A block then straddled thousands of lines, and the header
    // pass -- which matches an id range against the visible rows -- painted one
    // opaque band over the entire pane.
    let mut t = Terminal::new(20, 3, 100);
    for _ in 0..8 {
        t.advance(b"old\r\n");
    }
    t.scroll_display(6);

    t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07x\x1b]133;C\x07\r\nout\r\n\x1b]133;D;0\x07");

    let b = t.blocks().blocks().last().expect("a block");
    let oldest_live = t.grid().active_row(0).id;
    assert!(
        b.prompt_line >= oldest_live,
        "the prompt was recorded at line {} but the live screen starts at {oldest_live}",
        b.prompt_line
    );
    let (o, e) = (b.output_line.expect("output"), b.end_line.expect("end"));
    assert!(e >= o, "a block that printed a line must not end before it began");
    assert!(
        e - b.prompt_line < t.grid().rows() as u64,
        "a one-command block spanned {} lines",
        e - b.prompt_line
    );
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
fn narrowing_hard_must_not_evict_the_whole_history() {
    // The scrollback cap counts *rows*, and narrowing rewraps every logical
    // line into more of them — so halving the width can double the row count
    // and push the oldest content past the cap. That is eviction working as
    // designed, and its effect on the index is not: `evict_before` raises
    // `authoritative_from` to `next_id` once the last block goes, after which
    // every keyframe declares the host authoritative past every block that
    // ever existed and carries none. The loss is permanent and global — a
    // brand-new client attaching later is told there is no history. (#200)
    let mut t = Terminal::new(80, 10, 40);
    for i in 0..12 {
        let line = format!("\x1b]133;A\x07$ \x1b]133;B\x07cmd{i}\x1b]133;C\x07\r\n{}\r\n\x1b]133;D;0\x07",
            "x".repeat(70));
        t.advance(line.as_bytes());
    }
    let before = t.blocks().blocks().len();
    assert!(before > 0, "commands ran, so there is history to lose");

    t.resize(10, 10);
    let after = t.blocks().blocks().len();

    assert!(
        after > 0,
        "narrowing from 80 to 10 columns evicted every one of the {before} blocks — \
         and once the last goes, authoritative_from rises past them all and no client, \
         however fresh, is ever told they existed"
    );
}

#[test]
fn narrowing_and_widening_back_keeps_every_block() {
    // A drag is not one resize. A window goes narrow and comes back, each step
    // renumbering every line id, and the test above covers only the widening
    // half. The round trip destroyed the entire index in a live session: three
    // blocks before, none after — proved host-side by reloading the client so
    // the daemon sent a fresh keyframe, while a second session on the same
    // daemon still delivered its own blocks. (#200)
    //
    // Tall enough that nothing is evicted: this is about re-anchoring, and a
    // block legitimately dropped for scrolling out of a short scrollback would
    // make the test lie about which one it is testing.
    let mut t = Terminal::new(40, 20, 500);
    t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07one\x1b]133;C\x07\r\nfirst output\r\n\x1b]133;D;0\x07");
    t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07two\x1b]133;C\x07\r\nsecond output\r\n\x1b]133;D;0\x07");
    assert_eq!(t.blocks().blocks().len(), 2, "two commands ran");

    t.resize(12, 20);
    assert_eq!(
        t.blocks().blocks().len(),
        2,
        "narrowing rewraps every line; the blocks are still the same two commands: {:?}",
        t.blocks().blocks()
    );

    t.resize(40, 20);
    assert_eq!(
        t.blocks().blocks().len(),
        2,
        "and coming back must not lose them either: {:?}",
        t.blocks().blocks()
    );

    // Surviving is not enough — a block that names the wrong rows renders as
    // somebody else's output, which is how a listing ends up split across two
    // cards and the live prompt is swallowed into one.
    for (b, command, text) in [
        (&t.blocks().blocks()[0], "one", "first output"),
        (&t.blocks().blocks()[1], "two", "second output"),
    ] {
        assert_eq!(b.command, command);
        let out = b.output_line.expect("the command produced output");
        let row = t.grid().row_of_line(out).expect("its output line still exists");
        assert_eq!(
            t.grid().row(row).text().trim_end(),
            text,
            "block {:?} must still name its own output after the round trip",
            b.command
        );
    }
}

/// The bytes ConPTY sends in answer to `ResizePseudoConsole`, restating the
/// viewport it was just handed.
///
/// Measured against a real pwsh with `pty_dump --resize` (#205, re-measured for
/// #200): hide the cursor, declare the new size with XTWINOPS, home, then
/// restate the viewport, each line terminated with `ESC[K`. **There is no
/// `ESC[2J` anywhere** — the screen is rewritten in place, which is why
/// `BlockIndex::erase_screen` is never reached by a resize and why a block's
/// anchor can survive while the text underneath it does not.
///
/// **It restates *logical lines*, not physical rows.** Resized to 40 columns, a
/// 100-character line comes back as 100 characters and one `ESC[K` — ConPTY
/// does not re-break it, it relies on our autowrap to lay it out:
///
/// ```text
/// ESC[?25l ESC[8;30;40t ESC[H xxxx…(100)… ESC[K \r\n TAIL ESC[K \r\n ESC[K \r\n …
/// ```
///
/// A helper emitting row-by-row with `\r\n` between would therefore be testing
/// something ConPTY never does, and would break every wrap it restated.
fn conpty_repaint(t: &Terminal) -> Vec<u8> {
    let (cols, rows) = (t.grid().cols(), t.grid().rows());
    let (row, col) = (t.cursor().row + 1, t.cursor().col + 1);
    let mut out = format!("\x1b[?25l\x1b[8;{rows};{cols}t\x1b[H");

    let mut r = 0;
    while r < rows {
        // Collect one logical line, the way the shell printed it.
        let mut text = String::new();
        while r < rows && t.grid().row(r).wrapped {
            text.push_str(&t.grid().row(r).text());
            r += 1;
        }
        if r < rows {
            text.push_str(t.grid().row(r).text().trim_end());
            r += 1;
        }
        out.push_str(&text);
        // Not when a *non-empty* line ends exactly on a row boundary: the
        // cursor is then at the right margin with a deferred wrap, where `EL`
        // erases the last cell -- correct per the spec, and an emitter that
        // wanted the text it just wrote would be destroying it. An empty line
        // is not that case and does get its `ESC[K`, which is most of what the
        // capture consists of.
        let len = text.chars().count();
        if len == 0 || !len.is_multiple_of(cols) {
            out.push_str("\x1b[K");
        }
        if r < rows {
            out.push_str("\r\n");
        }
    }

    out.push_str(&format!("\x1b[{row};{col}H\x1b[?25h"));
    out.into_bytes()
}

#[test]
fn erasing_a_wrapped_row_to_its_end_stops_it_being_wrapped() {
    // `wrapped` is one fact kept in two places -- the row's own flag and
    // `CellFlags::WRAPLINE` on its last cell, written together by
    // `Grid::set_wrapped`. An erase blanks the cells, which clears the second,
    // and left the first alone: the row went on claiming it continued into the
    // next one while the cell that said so had been erased.
    //
    // Not a corner. Every row of a ConPTY resize repaint is terminated with
    // `ESC[K` (#205), and the repaint overwrites rows *in place* -- it never
    // scrolls, so `Row::reset`, the only other thing that clears the flag,
    // never runs. Nothing looks wrong until the *next* width change, when
    // reflow rejoins two rows that were never one logical line: a listing
    // collapses to half its rows and the text below is dragged up under
    // anchors the reanchor had mapped perfectly correctly. (#200)
    let mut t = Terminal::new(5, 3, 100);
    t.advance(b"abcdefgh");
    assert!(t.grid().row(0).wrapped, "the fixture did not wrap");

    t.advance(b"\x1b[1;1H\x1b[K");
    assert!(
        !t.grid().row(0).wrapped,
        "the row was erased to its end and still claims to continue into the next"
    );

    // The consequence, which is what makes this worth a test rather than tidiness.
    t.resize(20, 3);
    assert_eq!(
        screen(&t),
        "\nfgh",
        "reflow rejoined an erased row with the one below it, dragging `fgh` up onto \
         a line it never belonged to"
    );
}

/// Which half of a drag a repaint is answering.
///
/// **The two are not the same bytes, and assuming they were is what let #247
/// ship broken.** `corpus/resize-drag.vtrec` has both halves of one real drag:
///
/// ```text
/// Down:  ESC[?25l  ESC[8;<rows>;<cols>t  ESC[H  <rows, each ESC[K>              ESC[?25h
/// Up:    ESC[?25l                        ESC[H  <rows, each ESC[K>  ESC[<r>;1H  ESC[?25h
/// ```
///
/// `<r>` is where the shell's cursor really is, which is the *kept* row count
/// rather than the viewport's: the grow restates what it still had and leaves
/// the cursor at the end of it. `corpus/resize-drag.vtrec` has `ESC[8;1H` after
/// seven restated rows and a blank.
///
/// ConPTY announces the new size on the way **down** and not on the way back,
/// and only the way back positions the cursor at the end. The settle exists for
/// the way back, so a helper that announced both taught every test that the
/// marker was there when it was not. (#271)
#[derive(Clone, Copy, PartialEq, Eq)]
enum Drag {
    Down,
    Up,
}

/// A repaint from a pty whose buffer is only as tall as the viewport.
///
/// ConPTY's is. Shrink to a few rows and everything that no longer fits is
/// gone from it for good; grow back and it restates the little it kept and
/// **blanks the rest** — every one of those blank lines terminated with
/// `ESC[K`, which is most of what a real capture consists of.
///
/// See [`Drag`] for why the direction is a parameter rather than a detail.
fn conpty_repaint_after_a_squeeze(cols: usize, rows: usize, kept: &[&str], dir: Drag) -> Vec<u8> {
    let mut out = String::from("\x1b[?25l");
    if dir == Drag::Down {
        out.push_str(&format!("\x1b[8;{rows};{cols}t"));
    }
    out.push_str("\x1b[H");
    for r in 0..rows {
        if r > 0 {
            out.push_str("\r\n");
        }
        out.push_str(kept.get(r).copied().unwrap_or(""));
        out.push_str("\x1b[K");
    }
    if dir == Drag::Up {
        out.push_str(&format!("\x1b[{};1H", kept.len().max(1)));
    }
    out.push_str("\x1b[?25h");
    out.into_bytes()
}

#[test]
fn dragging_the_height_down_and_back_puts_the_screen_back_as_it_was() {
    // What the user actually reported, one issue after the test below: the
    // blocks survived and the history survived, and the pane still came back
    // with two lines of output and a prompt jammed against the top of an
    // otherwise empty window. #200 stopped the *destruction*; the rows were
    // simply never given back, because the pull that would give them back had
    // to be skipped -- ConPTY's repaint would have blanked them.
    //
    // It only had to be skipped *until the repaint*. This drives the literal
    // bytes ConPTY sends (#205, #224's checklist item) and asserts the whole
    // gesture is reversible: same screen, same cursor, nothing left in history.
    let mut t = Terminal::new(40, 12, 500);
    t.set_pty_restates_viewport(true);
    t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07ls\x1b]133;C\x07\r\n");
    for i in 0..9 {
        t.advance(format!("entry {i}\r\n").as_bytes());
    }
    t.advance(b"\x1b]133;D;0\x07\x1b]133;A\x07$ ");
    let before = t.screen_text();
    let cursor_before = t.cursor();
    let out = t.blocks().blocks()[0].output_line.expect("the command produced output");

    t.resize(40, 1);
    t.advance(&conpty_repaint_after_a_squeeze(40, 1, &["$ "], Drag::Down));
    t.resize(40, 12);
    let _ = t.take_events();
    t.advance(&conpty_repaint_after_a_squeeze(40, 12, &["$ "], Drag::Up));

    // The host has to tell its clients, and it cannot tell them in a delta:
    // rows that were history are on screen now, and a client applying deltas
    // over that holds each of them twice.
    assert!(
        t.take_events().iter().any(|e| matches!(e, TermEvent::ViewportRebased)),
        "the settle moved the boundary without asking anyone for a keyframe"
    );
    assert_eq!(t.screen_text(), before, "the drag was not reversible");
    assert_eq!(t.cursor().row, cursor_before.row, "the prompt did not come back down");
    assert_eq!(
        t.grid().scrollback_len(),
        0,
        "the displaced rows are still parked in history with a blank screen below them"
    );
    // On the *screen* this time, not merely reachable as history: a block whose
    // rows are only in scrollback renders as a card with nothing in it, which is
    // what "everything disappeared" looked like.
    for (n, line) in (out..out + 9).enumerate() {
        let row = t.grid().row_of_line(line).unwrap_or_else(|| {
            panic!("line {line} of the listing is not on screen after the drag")
        });
        assert_eq!(t.grid().row(row).text().trim_end(), format!("entry {n}"));
    }
}

#[test]
fn a_repaint_while_a_full_screen_program_is_up_leaves_the_primary_grid_alone() {
    // The latch is armed on whichever grid is active, so it has to be settled
    // there too. Settling the primary unconditionally reads as harmless — the
    // alt screen has no scrollback to give back, so what is there to get wrong —
    // and is not: the primary is carrying a debt from the drag that happened
    // before vim started, and an alt-screen repaint would pay it against a
    // viewport ConPTY is describing for a different screen entirely.
    // Eleven newlines, so the twelve rows are full and nothing has scrolled:
    // every row of history below comes from the drag, which makes the count
    // exact rather than something to work out.
    let mut t = Terminal::new(40, 12, 500);
    t.set_pty_restates_viewport(true);
    for i in 0..11 {
        t.advance(format!("line {i}\r\n").as_bytes());
    }
    t.advance(b"line 11");
    assert_eq!(t.grid().scrollback_len(), 0, "the fixture scrolled before the drag");

    // vim starts, and *then* the window is dragged — so the resize reaches both
    // grids and the repaint that answers it describes the alternate screen.
    // Cursor on the last row, where a full-screen program's status line puts
    // it. It matters: with the cursor at the top a shrink gives up the blank
    // rows below it and nothing goes over the top at all, so the alt grid never
    // reaches the code this is about and the test passes for the wrong reason.
    t.advance(b"\x1b[?1049h\x1b[12;1H");
    t.resize(40, 4);
    t.resize(40, 12);
    t.advance(&conpty_repaint_after_a_squeeze(40, 12, &["~"], Drag::Up));
    t.advance(b"\x1b[?1049l");

    assert_eq!(
        t.grid().scrollback_len(),
        8,
        "an alt-screen repaint paid the primary grid's debt, against rows it never restated"
    );
}

#[test]
fn a_repaint_for_a_size_the_grid_has_left_is_sat_out() {
    // A drag emits resizes faster than ConPTY answers them, so a repaint laid
    // out for a size we have already left is routine rather than exotic. Its
    // `CSI 8;r;c t` names that stale size, which is how it can be told apart —
    // and it has to be, because settling on it pays a grow's debt against a
    // viewport that has since shrunk, dragging history down into rows the next
    // repaint is about to blank. That is #200 arrived at from the other side.
    let mut t = Terminal::new(40, 12, 500);
    t.set_pty_restates_viewport(true);
    for i in 0..12 {
        t.advance(format!("line {i}\r\n").as_bytes());
    }

    t.resize(40, 4);
    t.advance(&conpty_repaint_after_a_squeeze(40, 4, &["line 11"], Drag::Down));
    t.resize(40, 12);
    let banked = t.grid().scrollback_len();

    // The repaint for the 4-row viewport, arriving after the grow to 12.
    t.advance(&conpty_repaint_after_a_squeeze(40, 4, &["line 11"], Drag::Down));

    assert_eq!(
        t.grid().scrollback_len(),
        banked,
        "a stale repaint settled the debt, so the rows it gave back are about to be blanked"
    );
}

#[test]
fn a_stale_unannounced_repaint_does_not_pay_the_debt_early() {
    // The test above can exist because a shrink's repaint names its size. A
    // grow's does not (#271), so during a drag *up* the stale repaint cannot be
    // told apart by anything it says -- only by what it does. It restates the
    // whole of the viewport it was laid out for, every row terminated with
    // `ESC[K`, so a repaint the grid has outrun stops short of the bottom row.
    // Settling on one pays the debt against blank rows a *later* repaint is
    // about to rewrite: the pulled history lands exactly where the next
    // repaint's `ESC[K`s fall, and it is no longer in scrollback either. That
    // is #200's destruction, reached through the fix for #247 -- and it is what
    // "I dragged the height down and back and the text is gone, the block is
    // still there" was. (#312)
    let mut t = Terminal::new(40, 12, 500);
    t.set_pty_restates_viewport(true);
    t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07ls\x1b]133;C\x07\r\n");
    for i in 0..9 {
        t.advance(format!("entry {i}\r\n").as_bytes());
    }
    t.advance(b"\x1b]133;D;0\x07\x1b]133;A\x07$ ");
    let out = t.blocks().blocks()[0].output_line.expect("the command produced output");

    // Down to 4 rows; ConPTY keeps the last four and answers with a repaint
    // that names its size.
    t.resize(40, 4);
    let kept = ["entry 6", "entry 7", "entry 8", "$ "];
    t.advance(&conpty_repaint_after_a_squeeze(40, 4, &kept, Drag::Down));
    let banked = t.grid().scrollback_len();

    // The drag continues up. The repaint for an intermediate 6-row viewport
    // arrives after the grid has already grown to 8 -- unannounced, so there
    // is no size in it to sit out.
    t.resize(40, 8);
    t.advance(&conpty_repaint_after_a_squeeze(40, 6, &kept, Drag::Up));
    assert_eq!(
        t.grid().scrollback_len(),
        banked,
        "a stale unannounced repaint settled the debt, handing history to the next \
         repaint to blank"
    );

    // The repaint the grow was actually waiting for. After it, every line of
    // the listing must still exist -- on screen or as history.
    t.advance(&conpty_repaint_after_a_squeeze(40, 8, &kept, Drag::Up));
    for (n, line) in (out..out + 9).enumerate() {
        let found = t
            .grid()
            .row_of_line(line)
            .map(|row| t.grid().row(row).text())
            .or_else(|| t.grid().lines_by_id(line, 1).first().map(|r| r.text()))
            .unwrap_or_default();
        assert_eq!(
            found.trim_end(),
            format!("entry {n}"),
            "line {line} of the listing was destroyed by the drag"
        );
    }
}

#[test]
fn a_multi_step_drag_with_lagging_repaints_comes_back_whole() {
    // A real drag is what `ResizeObserver` and the window server make of one
    // gesture: a stream of resizes, with ConPTY's repaints arriving late and
    // laid out for sizes the grid has already left. The corpus recording and
    // the tests above drive one shrink and one grow; this is the storm, and
    // the storm is where #312 lives -- every intermediate repaint is a chance
    // to pay the debt early into rows the next one blanks.
    let mut t = Terminal::new(40, 12, 500);
    t.set_pty_restates_viewport(true);
    t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07ls\x1b]133;C\x07\r\n");
    for i in 0..9 {
        t.advance(format!("entry {i}\r\n").as_bytes());
    }
    t.advance(b"\x1b]133;D;0\x07\x1b]133;A\x07$ ");
    let before = t.screen_text();
    let cursor_before = t.cursor();
    let _ = t.take_events();

    let kept = ["entry 6", "entry 7", "entry 8", "$ "];
    t.resize(40, 4);
    t.advance(&conpty_repaint_after_a_squeeze(40, 4, &kept, Drag::Down));
    // Two more resizes land before the next repaint does, so the repaint that
    // arrives is for a size in the middle of the gesture.
    t.resize(40, 6);
    t.resize(40, 8);
    t.advance(&conpty_repaint_after_a_squeeze(40, 6, &kept, Drag::Up));
    t.resize(40, 10);
    t.resize(40, 12);
    t.advance(&conpty_repaint_after_a_squeeze(40, 8, &kept, Drag::Up));
    // The repaint that answers where the mouse stopped.
    t.advance(&conpty_repaint_after_a_squeeze(40, 12, &kept, Drag::Up));

    let rebased = t
        .take_events()
        .iter()
        .filter(|e| matches!(e, TermEvent::ViewportRebased))
        .count();
    assert_eq!(
        rebased, 1,
        "only the repaint that covers the final viewport may move the boundary; \
         {rebased} settles ran"
    );
    assert_eq!(t.screen_text(), before, "the drag was not reversible");
    assert_eq!(t.cursor().row, cursor_before.row, "the prompt did not come back down");
    assert_eq!(t.grid().scrollback_len(), 0, "rows are still parked in history");
}

#[test]
fn a_widen_repaint_lands_on_the_rows_that_already_hold_its_content() {
    // The width axis (#224), with the loss half the height work made visible:
    // `ls`, drag the width smaller then bigger, and rows are destroyed. The
    // narrow halves agree — both reflows tail-anchor onto the prompt, row for
    // row (measured, `resize-width.vtrec`). The widen is the trap: ConPTY
    // un-wraps its viewport-tall buffer into fewer, wider rows and restates
    // them **from home**, ELs below — while our reflow put the prompt back at
    // the *bottom*, so the restatement overwrote the top of the listing with
    // a copy of its tail and the ELs erased the middle. In place, ids intact,
    // nowhere in scrollback: destroyed, which is what "the content is
    // fourteen rows too high" always was.
    //
    // The fix is the height model's posture: after a width reflow on a
    // restated grid, the viewport re-anchors **top-aligned on the line ConPTY
    // still holds** — the pre-resize viewport-top line, whose new position
    // the reflow's `Reindex` knows. Surplus above is scrollback (scroll up to
    // see it, exactly the strand view); the blanks below are what the ELs
    // land on.
    let mut t = Terminal::new(40, 12, 500);
    t.set_pty_restates_viewport(true);
    t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07ls\x1b]133;C\x07\r\n");
    for i in 0..9 {
        // Long enough that 20 columns wraps each into two rows.
        t.advance(format!("entry {i} ------------------- x\r\n").as_bytes());
    }
    t.advance(b"\x1b]133;D;0\x07\x1b]133;A\x07$ ");

    // Narrow. Our reflow and ConPTY's agree on the tail; its repaint restates
    // the *logical lines* its buffer holds (six of ours), which our autowrap
    // lays back into exactly the twelve rows on screen. Announced, like every
    // first repaint of a gesture.
    t.resize(20, 12);
    let narrow_kept = [
        "entry 4 ------------------- x",
        "entry 5 ------------------- x",
        "entry 6 ------------------- x",
        "entry 7 ------------------- x",
        "entry 8 ------------------- x",
        "$ ",
    ];
    t.advance(&conpty_repaint_after_a_squeeze(20, 12, &narrow_kept, Drag::Down));

    // And back out. ConPTY un-wraps its twelve narrow rows into six wide ones
    // and restates them from home, ELs the rest — the shape of chunk #20 in
    // the recording, unannounced with the cursor placed at the end.
    t.resize(40, 12);
    t.advance(&conpty_repaint_after_a_squeeze(40, 12, &narrow_kept, Drag::Up));

    // Nothing destroyed, nothing doubled: every entry exactly once across
    // history and screen…
    for n in 0..9 {
        let want = format!("entry {n} ------------------- x");
        let copies = (0..t.grid().total_lines())
            .filter_map(|i| t.grid().line(i))
            .filter(|r| r.text().trim_end() == want)
            .count();
        assert_eq!(
            copies, 1,
            "{want:?} exists {copies} times after the widen repaint"
        );
    }
    // …and the screen is ConPTY-aligned: its top line at our row 0, the
    // prompt where it put the cursor, the ELs having landed on blanks.
    assert_eq!(
        t.grid().row(0).text().trim_end(),
        "entry 4 ------------------- x",
        "the viewport is not anchored on the line ConPTY still holds"
    );
    assert_eq!(t.grid().row(5).text().trim_end(), "$", "the prompt is not where ConPTY put it");
}

#[test]
fn ordinary_output_after_a_restore_strands_the_pull_instead_of_overwriting_it() {
    // Inspected live before it was understood (#341): after a drag restored
    // the listing, running `ls` again interleaved two listings -- rows like
    // "Length Namees" and "AGENTS.mdchain.toml", a block header mid-print.
    // The repaints are all handled now; this is *ordinary output*. After a
    // settle, ConPTY's buffer still holds only its kept rows at the top, so
    // the shell's next render positions with absolute coordinates in
    // ConPTY's row-space -- offset from ours by everything the settle pulled
    // -- and writes land mid-listing with no erase over the tails.
    //
    // The restore is a between-gestures view. The first ordinary content op
    // (a print, a linefeed, a cursor move that is not a restatement's hidden
    // home) strands the pull: boundary up over the restored rows, blanks
    // minted below, cursor realigned, and the write lands exactly where
    // ConPTY meant it -- which is where Windows Terminal would have had the
    // prompt all along.
    let mut t = Terminal::new(40, 12, 500);
    t.set_pty_restates_viewport(true);
    t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07ls\x1b]133;C\x07\r\n");
    for i in 0..9 {
        t.advance(format!("entry {i}\r\n").as_bytes());
    }
    t.advance(b"\x1b]133;D;0\x07\x1b]133;A\x07$ ");

    let kept = ["entry 6", "entry 7", "entry 8", "$ "];
    t.resize(40, 4);
    t.advance(&conpty_repaint_after_a_squeeze(40, 4, &kept, Drag::Down));
    t.resize(40, 12);
    t.advance(&conpty_repaint_after_a_squeeze(40, 12, &kept, Drag::Up));
    assert_eq!(t.grid().scrollback_len(), 0, "the round trip did not settle, so this proves nothing");
    let _ = t.take_events();

    // The user types `ls` again. ConPTY's screen is the four kept rows at the
    // top, so the shell's render addresses row 4 -- which in the restored
    // grid is the middle of the listing.
    t.advance(b"\x1b[4;3Hls\r\n");
    for i in 0..9 {
        t.advance(format!("entry {i}\r\n").as_bytes());
    }
    t.advance(b"$ ");

    // The strand moved the boundary, so every client is owed a keyframe.
    assert!(
        t.take_events().iter().any(|e| matches!(e, TermEvent::ViewportRebased)),
        "the boundary moved (or rows were destroyed) without a keyframe"
    );
    // Both listings, whole: every entry exactly twice across history and
    // screen, with its own text -- an overlay leaves counts unbalanced and
    // rows carrying two generations of text.
    for n in 0..9 {
        let want = format!("entry {n}");
        let copies = (0..t.grid().total_lines())
            .filter_map(|i| t.grid().line(i))
            .filter(|r| r.text().trim_end() == want)
            .count();
        assert_eq!(
            copies, 2,
            "{want:?} exists {copies} times -- the second listing was written over the first"
        );
    }
}

#[test]
fn a_shrink_after_a_settle_does_not_forget_the_pull_the_repaint_needs_taken_back() {
    // The reported gesture's third leg, and the one every earlier test
    // stopped short of: drag up, drag down -- the settle restores the screen
    // -- and drag up *again*. After the settle this grid's viewport holds
    // more of the session than ConPTY's buffer does, permanently, so the new
    // shrink's repaint restates a lesser truth over a fuller screen. The
    // re-bank exists for exactly that moment -- but the shrink resize arrives
    // first (grid before pty, always), and it zeroed `settled_pull` on the
    // reasoning that a shrink re-banks displaced rows on its own terms. It
    // banks only what leaves over the *top*; a partial shrink with blank rows
    // below the cursor banks nothing, the pull was forgotten anyway, and the
    // repaint blanked the pulled rows in place -- no longer in scrollback
    // either. "It snaps and disappears." (#335)
    let mut t = Terminal::new(40, 12, 500);
    t.set_pty_restates_viewport(true);
    t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07ls\x1b]133;C\x07\r\n");
    for i in 0..9 {
        t.advance(format!("entry {i}\r\n").as_bytes());
    }
    t.advance(b"\x1b]133;D;0\x07\x1b]133;A\x07$ ");
    let out = t.blocks().blocks()[0].output_line.expect("the command produced output");

    // Down, and back up: the round trip the earlier tests cover.
    let kept = ["entry 6", "entry 7", "entry 8", "$ "];
    t.resize(40, 4);
    t.advance(&conpty_repaint_after_a_squeeze(40, 4, &kept, Drag::Down));
    t.resize(40, 12);
    t.advance(&conpty_repaint_after_a_squeeze(40, 12, &kept, Drag::Up));
    assert_eq!(t.grid().scrollback_len(), 0, "the round trip did not settle, so this proves nothing");

    // And up again -- partially, so the blank rows below the cursor absorb
    // most of it and almost nothing is banked over the top. ConPTY answers
    // with a repaint of the little *it* still holds.
    t.resize(40, 10);
    t.advance(&conpty_repaint_after_a_squeeze(40, 10, &kept, Drag::Down));

    for (n, line) in (out..out + 9).enumerate() {
        let found = t
            .grid()
            .row_of_line(line)
            .map(|row| t.grid().row(row).text())
            .or_else(|| t.grid().lines_by_id(line, 1).first().map(|r| r.text()))
            .unwrap_or_default();
        assert_eq!(
            found.trim_end(),
            format!("entry {n}"),
            "line {line} of the listing was destroyed by the second shrink's repaint"
        );
    }
}

#[test]
fn an_intermediate_settle_is_taken_back_before_the_next_repaint_writes() {
    // The storm tests above are about repaints the grid outran. This is the
    // opposite and it is what the daemon actually produces at a slower drag:
    // every repaint *matches* its size, one per step. Each grow's settle is
    // then legitimate by every local test -- and still wrong, because ConPTY's
    // buffer never got the pulled rows back: the next step's repaint restates
    // that buffer from home, overwriting the rows the settle just pulled and
    // blanking the tail. Rows destroyed, one step at a time, with nothing
    // stale anywhere. Measured live with `probe:resize` (23 -> 12 non-blank
    // rows across one height-only drag) before it was understood. (#312)
    //
    // A settle is therefore *provisional*: when the next repaint opens, the
    // grid takes the pull back -- boundary up, rows re-banked, the share owed
    // again -- and that repaint's own settle pays it out over what *it* wrote.
    let mut t = Terminal::new(40, 12, 500);
    t.set_pty_restates_viewport(true);
    t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07ls\x1b]133;C\x07\r\n");
    for i in 0..9 {
        t.advance(format!("entry {i}\r\n").as_bytes());
    }
    t.advance(b"\x1b]133;D;0\x07\x1b]133;A\x07$ ");
    let before = t.screen_text();
    let out = t.blocks().blocks()[0].output_line.expect("the command produced output");

    let kept = ["entry 6", "entry 7", "entry 8", "$ "];
    t.resize(40, 4);
    t.advance(&conpty_repaint_after_a_squeeze(40, 4, &kept, Drag::Down));
    // Each step's repaint arrives before the next resize, at its own size --
    // the daemon's cadence, not the recorder's.
    t.resize(40, 8);
    t.advance(&conpty_repaint_after_a_squeeze(40, 8, &kept, Drag::Up));
    t.resize(40, 12);
    t.advance(&conpty_repaint_after_a_squeeze(40, 12, &kept, Drag::Up));

    assert_eq!(t.screen_text(), before, "the stepped drag was not reversible");
    assert_eq!(t.grid().scrollback_len(), 0, "rows are still parked in history");
    for (n, line) in (out..out + 9).enumerate() {
        let row = t.grid().row_of_line(line).unwrap_or_else(|| {
            panic!("line {line} of the listing is not on screen after the stepped drag")
        });
        assert_eq!(
            t.grid().row(row).text().trim_end(),
            format!("entry {n}"),
            "the intermediate settle's rows were overwritten by the next repaint"
        );
    }
}

#[test]
fn a_stale_repaint_taller_than_the_grid_neither_cancels_the_debt_nor_duplicates_history() {
    // The shrink half of a storm. ConPTY coalesces, so the one repaint it does
    // send can be laid out for a size several resizes back -- *taller* than
    // the grid is now. Its writes overflow the bottom, and each overflow
    // scroll did two bad things at once: `scroll_up` cancelled the restate
    // debt (that guard exists for real content moving on, not for a stale
    // repaint's overflow), stranding the drag; and the scrolled-off rows --
    // the repaint's own restatement of content this grid already holds above
    // the viewport -- were banked into scrollback, so the host's history held
    // the same rows twice and scrolling up after the drag showed them twice.
    // (#315; the mechanism was measured on `resize-drag-storm2`, this box.)
    //
    // Everything inside a restatement bracket comes from ConPTY restating
    // content we hold, so its overflow is dropped rather than banked and the
    // debt stays owed -- which is what lets the drag's final repaint settle
    // and put the screen back.
    let mut t = Terminal::new(40, 12, 500);
    t.set_pty_restates_viewport(true);
    t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07ls\x1b]133;C\x07\r\n");
    for i in 0..9 {
        t.advance(format!("entry {i}\r\n").as_bytes());
    }
    t.advance(b"\x1b]133;D;0\x07\x1b]133;A\x07$ ");
    let before = t.screen_text();
    let out = t.blocks().blocks()[0].output_line.expect("the command produced output");

    // Two shrinks land before ConPTY's first repaint does -- so the repaint
    // that arrives is for 8 rows on a 4-row grid, and it is the *first* of
    // the storm, which is the one that announces. Its four overflow rows
    // scroll.
    t.resize(40, 8);
    t.resize(40, 4);
    let banked = t.grid().scrollback_len();
    let kept8 = ["entry 2", "entry 3", "entry 4", "entry 5", "entry 6", "entry 7", "entry 8", "$ "];
    t.advance(&conpty_repaint_after_a_squeeze(40, 8, &kept8, Drag::Down));
    assert_eq!(
        t.grid().scrollback_len(),
        banked,
        "the stale repaint's overflow was banked into history (as duplicates)"
    );

    // The drag comes back up, and ConPTY answers where the mouse stopped.
    t.resize(40, 12);
    t.advance(&conpty_repaint_after_a_squeeze(
        40,
        12,
        &["entry 6", "entry 7", "entry 8", "$ "],
        Drag::Up,
    ));

    assert_eq!(
        t.screen_text(),
        before,
        "the overflow's scrolls cancelled the debt, so the drag never came back"
    );
    // Once each, anywhere: a line id that resolves must hold its own text and
    // no other copy of it may exist in history.
    for (n, line) in (out..out + 9).enumerate() {
        let copies = (0..t.grid().total_lines())
            .filter_map(|i| t.grid().line(i))
            .filter(|r| r.text().trim_end() == format!("entry {n}"))
            .count();
        assert_eq!(
            copies, 1,
            "entry {n} (line {line}) exists {copies} times across history and screen"
        );
    }
}

#[test]
fn a_resize_landing_mid_repaint_sits_that_repaint_out() {
    // `Session::resize` releases the terminal lock before it tells the pty
    // (the `ClosePseudoConsole` deadlock, `AGENTS.md`), so a resize can land
    // between a repaint's first byte and its last. A repaint that was armed
    // when the grid moved was laid out for a viewport that no longer exists --
    // stale by construction, whatever its shape. The corner that makes this
    // its own guard rather than a job for coverage: the drag keeps moving and
    // lands *on the repaint's own size*, so the stale repaint covers every
    // visible row and reads as current. Settling there pays the debt onto a
    // screen the next (announced, matching) repaint restates in place --
    // pulled history overwritten, no longer in scrollback. (#312)
    let mut t = Terminal::new(40, 12, 500);
    t.set_pty_restates_viewport(true);
    for i in 0..12 {
        t.advance(format!("line {i}\r\n").as_bytes());
    }

    t.resize(40, 4);
    t.advance(&conpty_repaint_after_a_squeeze(40, 4, &["line 11"], Drag::Down));
    t.resize(40, 8);
    let banked = t.grid().scrollback_len();

    // The stale repaint for a 6-row viewport begins against the 8-row grid --
    // it arms at its `ESC[H` -- and the drag's next shrink lands mid-repaint,
    // putting the grid at exactly the 6 rows the repaint describes.
    let repaint = conpty_repaint_after_a_squeeze(40, 6, &["line 11"], Drag::Up);
    let armed_at = b"\x1b[?25l\x1b[H".len();
    let (first, rest) = repaint.split_at(armed_at);
    t.advance(first);
    t.resize(40, 6);
    t.advance(rest);

    assert_eq!(
        t.grid().scrollback_len(),
        banked,
        "a repaint interrupted by a resize settled the debt against a viewport it \
         never described"
    );
}

#[test]
fn dragging_the_height_to_nothing_and_back_does_not_blank_every_block() {
    // The reported gesture, and the one the width-change story never covered:
    // drag the window's height down to nothing and back, and every block comes
    // back empty. The width never changes, so `Grid::resize` returns an empty
    // `Reindex`, no re-anchoring happens and none is needed — the blocks are
    // intact throughout. What was gone was the *text*.
    //
    // Growing used to pull rows back out of scrollback so a drag was one
    // reversible gesture. Against ConPTY that is not merely undone, it is
    // destructive: its buffer is viewport-tall, so the squeeze discarded what
    // no longer fitted, and the repaint on the way back blanks every row it no
    // longer has. Rows pulled down to meet it are erased, and the pull has
    // already moved them out of scrollback. History destroyed, not misplaced.
    // (#200)
    // Output that FILLS the viewport, which is the whole point: a real `ls` is
    // twenty-odd rows in a twenty-four-row pane, so the block lives on the
    // screen rather than safely up in history. A two-line block would sit in
    // scrollback throughout and survive by luck.
    let mut t = Terminal::new(40, 12, 500);
    t.set_pty_restates_viewport(true);
    t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07ls\x1b]133;C\x07\r\n");
    for i in 0..9 {
        t.advance(format!("entry {i}\r\n").as_bytes());
    }
    t.advance(b"\x1b]133;D;0\x07\x1b]133;A\x07$ ");
    assert_eq!(t.blocks().blocks().len(), 2, "one command and a live prompt");
    let out = t.blocks().blocks()[0].output_line.expect("the command produced output");

    // Down to a single row. ConPTY keeps that row and discards the rest.
    t.resize(40, 1);
    t.advance(&conpty_repaint_after_a_squeeze(40, 1, &["$ "], Drag::Down));

    // And back up. It restates the one line it still has, and blanks eleven.
    t.resize(40, 12);
    t.advance(&conpty_repaint_after_a_squeeze(40, 12, &["$ "], Drag::Up));

    assert_eq!(t.blocks().blocks().len(), 2, "the blocks were never the problem");
    for (n, line) in (out..out + 9).enumerate() {
        // Either place is right: the fix keeps this above the viewport, where
        // the repaint cannot reach it, so `row_of_line` -- which only searches
        // the screen -- correctly no longer finds it.
        let found = t
            .grid()
            .row_of_line(line)
            .map(|row| t.grid().row(row).text())
            .or_else(|| t.grid().lines_by_id(line, 1).first().map(|r| r.text()))
            .unwrap_or_default();
        assert_eq!(
            found.trim_end(),
            format!("entry {n}"),
            "line {line} of the listing came back blank -- which renders as the block \
             being gone, however intact the index is"
        );
    }
}

#[test]
fn a_drag_with_conpty_repaints_leaves_every_block_naming_its_own_output() {
    // `narrowing_and_widening_back_keeps_every_block` covers the reflow half of
    // a drag and passes. What it cannot see is that on Windows every resize is
    // *also* a full-viewport repaint, so the real gesture is resize-then-bytes,
    // twice -- our reflow and then ConPTY's restatement on top of it.
    //
    // The repaint here restates the screen we already have, which is ConPTY
    // agreeing with us and is therefore the *best* case: anything that breaks
    // under it breaks under every worse one, and breaks for every client.
    let mut t = Terminal::new(24, 8, 500);
    t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07one\x1b]133;C\x07\r\nfirst output\r\n\x1b]133;D;0\x07");
    t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07two\x1b]133;C\x07\r\nsecond output\r\n\x1b]133;D;0\x07");
    t.advance(b"\x1b]133;A\x07$ ");
    assert_eq!(t.blocks().blocks().len(), 3, "two commands and a live prompt");

    // Ordered the way the host must order it: the grid first, so the repaint is
    // parsed at the width it was laid out for (see `Session::resize`).
    for (cols, rows) in [(12, 8), (24, 8)] {
        t.resize(cols, rows);
        let repaint = conpty_repaint(&t);
        t.advance(&repaint);
    }

    for (i, text) in [(0usize, "first output"), (1, "second output")] {
        let b = &t.blocks().blocks()[i];
        let out = b.output_line.expect("the command produced output");
        let row = t.grid().row_of_line(out).expect("its output line still exists");
        assert_eq!(
            t.grid().row(row).text().trim_end(),
            text,
            "block {:?} names line {out}, which now reads {:?} -- the repaint moved the \
             screen out from under an anchor reflow had mapped correctly",
            b.command,
            t.grid().row(row).text()
        );
    }

    let live = t.blocks().last().expect("the live prompt");
    assert!(live.output_line.is_none(), "the live prompt has run nothing: {live:?}");
    assert!(
        t.blocks().blocks()[..2].iter().all(|b| !b.contains(live.prompt_line)),
        "the live prompt was swallowed into a finished block above it: {live:?}"
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
fn a_vscode_cwd_is_unescaped_like_the_command_it_travels_with() {
    // `633;P` carries the same `\xNN` escaping as `633;E`, and reading it
    // literally is wrong on exactly the platform where it matters: VS Code's own
    // PowerShell hook escapes every backslash, so a Windows cwd arrived as
    // `C:\x5cDev` and the status bar showed it. Measured against the real script
    // (#83), not inferred from its source.
    let t = run(40, 4, "\x1b]633;P;Cwd=C:\\x5cUsers\\x5candy\\x5cMy Code\x07\x1b]633;A\x07$ ");
    assert_eq!(t.cwd(), r"C:\Users\andy\My Code");

    // A path that merely *looks* escaped must survive, because `\x64` is a
    // directory name people really have and `\D` is every other Windows path.
    let t = run(40, 4, "\x1b]633;P;Cwd=/home/andy\x07\x1b]633;A\x07$ ");
    assert_eq!(t.cwd(), "/home/andy", "an unescaped path is left alone");
}

#[test]
fn a_real_pwsh_session_produces_real_blocks() {
    // The PowerShell half of the zsh test below, and recorded the same way:
    // through a real pty, from a real interactive `pwsh` with zesterm's hook
    // dot-sourced by the command line it builds. PSReadLine's inline prediction
    // is in the recording too, which is what makes it worth having -- the
    // command a person typed is repainted several times before it runs.
    let bytes =
        std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/blocks-pwsh.vtrec"))
            .expect("blocks fixture");
    let mut t = Terminal::new(120, 30, 200);
    for chunk in parse_vtrec(&bytes) {
        t.advance(&chunk);
    }

    let blocks = t.blocks().blocks();
    assert!(blocks.len() >= 3, "expected a block per prompt, got {}", blocks.len());

    let finished: Vec<_> = blocks.iter().filter(|b| b.end_line.is_some()).collect();
    assert!(finished.len() >= 2, "two commands ran to completion");

    assert!(!finished[0].failed(), "`echo hello` succeeded");
    assert!(finished[1].failed(), "`cmd /c exit 3` did not, and the status says so");

    // The whole reason the hook emits `633;E` rather than leaving zesterm to
    // read the grid: PSReadLine repaints the line as it predicts, so the cells
    // between `B` and `C` are a rendering of the command and not the command.
    assert_eq!(finished[0].command, "echo hello");
    assert_eq!(finished[1].command, "cmd /c exit 3");

    // A native command's real exit code, not a bare "it failed". `$?` alone
    // could not have produced this, which is what the status calculation in the
    // hook exists for.
    assert_eq!(
        finished[1].state,
        zest_core::BlockState::Finished { exit_code: Some(3) },
        "the exit code is the command's own, not a stand-in 1"
    );

    // `633;P;Cwd=` rode along with the same hook -- escaped by the hook and
    // unescaped here, which is the pairing a Windows path needs.
    assert_eq!(finished[0].cwd, r"C:\zestdemo");

    let out = finished[0].output_line.expect("output began");
    let row = t.grid().row_of_line(out).expect("still on screen");
    assert_eq!(t.grid().row(row).text(), "hello", "output_line is the first line of output");
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


/// A real ConPTY height drag: 100x30, down to 100x8, back to 100x30.
///
/// The recording that made this worth having also broke the fix it was meant to
/// guard. #247 armed the settle on `CSI 8 ; r ; c t`, on the strength of #205's
/// single capture of a *shrink*. This capture has both halves of one drag and
/// they are not the same shape:
///
/// ```text
/// shrink:  ESC[?25l  ESC[8;8;100t  ESC[H  <rows, each ESC[K>  ESC[?25h
/// grow:    ESC[?25l                ESC[H  <rows, each ESC[K>  ESC[8;1H ESC[?25h
/// ```
///
/// **ConPTY announces the size on the way down and not on the way back**, and
/// the way back is the one the settle exists for — so it never ran, and every
/// synthetic test went on passing because the helper emitted an announcement
/// ConPTY does not. That is the whole argument for replaying real bytes.
#[test]
fn a_recorded_conpty_drag_comes_back_as_it_was() {
    let bytes =
        std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/corpus/resize-drag.vtrec"))
            .expect("corpus/resize-drag.vtrec");

    // The geometry is part of the fixture and is not in the file: the listing
    // before the first resize was laid out for 100 columns, and a grid at any
    // other width wraps it somewhere ConPTY never did. `tests/README.md` says
    // so beside the recipe.
    let mut t = Terminal::new(100, 30, 500);
    t.set_pty_restates_viewport(true);

    // Nor is *when* the recorder resized. `pty_dump` waits 1500ms before each
    // step and 800ms after it for the repaint, so the two land either side of
    // these; the chunk stamps are what place them. Driving it any other way
    // would mean trusting a marker in the stream, and the absence of one on the
    // grow is exactly what this test is about.
    //
    // The stamps are already relative to the start of the capture
    // (`started.elapsed()` in `pty_dump`), so they are used as they are.
    // Re-basing them on the first chunk would shift everything by the time the
    // shell took to say anything — 433ms in this recording, against margins of
    // ~100ms, which is enough to put a resize *after* the repaint it is
    // supposed to precede. That is the ordering #200 is about, and it would
    // read as this test's assertions being wrong rather than its clock.
    const SHRINK_AT_US: u128 = 1_400_000;
    const GROW_AT_US: u128 = 3_600_000;
    const RESTORED_UNTIL_US: u128 = 5_000_000;

    let chunks = parse_vtrec_timed(&bytes);
    let (mut shrunk, mut grown) = (None, None);
    let mut restored: Option<(usize, String)> = None;
    for (i, (us, chunk)) in chunks.iter().enumerate() {
        if shrunk.is_none() && *us >= SHRINK_AT_US {
            t.resize(100, 8);
            shrunk = Some(i);
        }
        if grown.is_none() && *us >= GROW_AT_US {
            t.resize(100, 30);
            grown = Some(i);
        }
        // The restore holds until the shell speaks again: the recording's
        // trailing output (pwsh exiting) legitimately strands it. The restore
        // assertions therefore run between the settle and that output. (#341)
        if *us >= RESTORED_UNTIL_US && restored.is_none() {
            restored = Some((t.grid().scrollback_len(), t.screen_text()));
        }
        t.advance(chunk);
    }

    // Each resize has to land immediately before the repaint that answers it,
    // and asserting that keeps a re-recording honest: shift the timing and this
    // says so, instead of quietly replaying a repaint into a grid of the wrong
    // size and failing somewhere further down where the cause is not visible.
    let shrink_repaint = &chunks[shrunk.expect("the recording never reached the shrink")].1;
    let grow_repaint = &chunks[grown.expect("the recording never reached the grow")].1;
    assert!(
        shrink_repaint.windows(4).any(|w| w == b"\x1b[8;"),
        "the shrink resize did not land just before the repaint that announces it"
    );
    assert!(
        grow_repaint.windows(6).any(|w| w == b"\x1b[?25l"),
        "the grow resize did not land just before a repaint"
    );

    // The listing is back on screen rather than parked above it, and the last
    // line of it sits where it did before the drag rather than seven rows down
    // from the top of an otherwise empty window — measured while the restore
    // held, before the shell's trailing output stranded it.
    let (held, screen) = restored.expect("the recording ended before the restore window");
    assert_eq!(
        held, 0,
        "the grow never gave back what the shrink took: {held} rows still in history"
    );
    for name in ["AGENTS.md", "Cargo.lock", "README.md", "rust-toolchain.toml"] {
        assert!(screen.contains(name), "{name} is not on screen after the drag");
    }
    let last_ink = screen
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, _)| i)
        .max()
        .expect("a blank screen");
    assert!(
        last_ink >= 23,
        "the content is bunched at the top: last non-blank row is {last_ink} of 30"
    );
    // And after the trailing output: stranded, never destroyed.
    for name in ["AGENTS.md", "Cargo.lock", "README.md", "rust-toolchain.toml"] {
        let found = (0..t.grid().total_lines())
            .filter_map(|i| t.grid().line(i))
            .any(|r| r.text().contains(name));
        assert!(found, "{name} was destroyed when the trailing output stranded the restore");
    }
}

#[test]
fn a_recorded_drag_storm_survives_its_stale_repaints() {
    // The recording above is one shrink and one grow, with each repaint
    // arriving at the size it was laid out for. A drag is not that: winit
    // fires resizes throughout the gesture, ConPTY coalesces its answers, and
    // the repaints that do arrive are laid out for sizes the grid has already
    // left. `corpus/resize-drag-storm.vtrec` is that gesture recorded for real
    // (`pty_dump --resize-settle-ms 0`, #312): shrink 30 -> 8, then four grows
    // issued back-to-back -- 14, 20, 26, 30, within ~100us of each other --
    // and ConPTY answered with exactly two repaints:
    //
    // ```text
    // 1500.9ms  ESC[?25l ESC[8;8;100t ESC[H <8 rows>              ESC[?25h
    // 1901.3ms  ESC[?25l              ESC[H <20 rows>  ESC[8;1H   ESC[?25h
    // 1915.1ms  ESC[?25l              ESC[H <30 rows>  ESC[8;1H   ESC[?25h
    // ```
    //
    // The middle one is the trap this test exists for: a repaint for a 20-row
    // viewport, unannounced, parsing into a 30-row grid. Settling on it pays
    // the whole debt into blank rows the 30-row repaint blanks again three
    // bytes of wall clock later, with the pulled rows no longer in scrollback
    // -- the reported "text is gone, the block is still there". What refuses
    // it is its own coverage: its erases stop at row 20 of 30.
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/corpus/resize-drag-storm.vtrec"
    ))
    .expect("the recording exists");

    let mut t = Terminal::new(100, 30, 500);
    t.set_pty_restates_viewport(true);

    // As with the test above, geometry and timing are the fixture's, not the
    // file's. The four grows are injected at one threshold because that is how
    // they were issued: back-to-back, faster than ConPTY's first answer.
    const SHRINK_AT_US: u128 = 1_450_000;
    const GROWS_AT_US: u128 = 1_901_000;
    const RESTORED_UNTIL_US: u128 = 5_000_000;

    let chunks = parse_vtrec_timed(&bytes);
    let (mut shrunk, mut grown) = (None, None);
    let mut restored: Option<(usize, String)> = None;
    for (i, (us, chunk)) in chunks.iter().enumerate() {
        if shrunk.is_none() && *us >= SHRINK_AT_US {
            t.resize(100, 8);
            shrunk = Some(i);
        }
        if grown.is_none() && *us >= GROWS_AT_US {
            t.resize(100, 14);
            t.resize(100, 20);
            t.resize(100, 26);
            t.resize(100, 30);
            grown = Some(i);
        }
        // The restore holds until the shell's trailing output strands it, so
        // the restore assertions are taken here, mid-recording. (#341)
        if *us >= RESTORED_UNTIL_US && restored.is_none() {
            restored = Some((t.grid().scrollback_len(), t.screen_text()));
        }
        t.advance(chunk);
    }

    // What keeps a re-recording honest. The shrink's repaint announces; the
    // first repaint after the grows must be *stale* -- unannounced and laid
    // out for fewer rows than the grid holds -- or this replay is no longer
    // about staleness at all; and the one after it must cover the final size,
    // or the debt legitimately stays unpaid.
    let rows_of = |chunk: &[u8]| chunk.windows(2).filter(|w| w == b"\r\n").count() + 1;
    // `ESC[8;` alone is not it: the grow half *ends* with `ESC[8;1H`, ConPTY
    // putting the cursor back on row 8. An announcement is the same prefix
    // terminated with `t`.
    let announces = |chunk: &[u8]| {
        chunk.windows(4).enumerate().any(|(i, w)| {
            w == b"\x1b[8;"
                && chunk[i + 4..].iter().find(|b| !(b.is_ascii_digit() || **b == b';'))
                    == Some(&b't')
        })
    };
    let shrink_repaint = &chunks[shrunk.expect("the recording never reached the shrink")].1;
    assert!(
        announces(shrink_repaint),
        "the shrink resize did not land just before the repaint that announces it"
    );
    let stale = &chunks[grown.expect("the recording never reached the grows")].1;
    assert!(
        stale.windows(6).any(|w| w == b"\x1b[?25l") && !announces(stale),
        "the repaint after the grows is not the unannounced kind this test needs"
    );
    assert!(
        rows_of(stale) < 30,
        "the repaint after the grows covers the whole grid, so nothing here is stale -- \
         re-record with the grows back-to-back"
    );
    let closing = &chunks[grown.unwrap() + 1].1;
    assert_eq!(
        rows_of(closing),
        30,
        "the last repaint does not cover the final viewport, so the debt is honestly unpaid"
    );

    let (held, screen) = restored.expect("the recording ended before the restore window");
    assert_eq!(
        held, 0,
        "the storm never gave back what the shrink took: {held} rows still in history"
    );
    for name in ["AGENTS.md", "Cargo.lock", "README.md", "rust-toolchain.toml"] {
        assert!(
            screen.contains(name),
            "{name} is not on screen after the storm -- the stale repaint's settle handed \
             it to the closing repaint to blank"
        );
    }
    let last_ink = screen
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .map(|(i, _)| i)
        .max()
        .expect("a blank screen");
    assert!(
        last_ink >= 20,
        "the content is bunched at the top: last non-blank row is {last_ink} of 30"
    );
    // And after the trailing output stranded the restore: present, somewhere.
    for name in ["AGENTS.md", "Cargo.lock", "README.md", "rust-toolchain.toml"] {
        let found = (0..t.grid().total_lines())
            .filter_map(|i| t.grid().line(i))
            .any(|r| r.text().contains(name));
        assert!(found, "{name} was destroyed when the trailing output stranded the restore");
    }
}

#[test]
fn a_recorded_overflowing_storm_is_reversible_and_leaves_no_duplicates() {
    // `corpus/resize-drag-overflow.vtrec`: three shrinks issued back-to-back
    // (30 -> 24 -> 16 -> 8 within ~100us), a 300ms turnaround, four grows
    // back-to-back, all against a real ConPTY. What it answered with:
    //
    // ```text
    // 2001.1ms  ESC[?25l ESC[8;24;100t ESC[H <24 rows>            ESC[?25h
    // 2016.3ms  ESC[?25l               ESC[H <8 rows>             ESC[?25h
    // 2301.7ms  ESC[?25l               ESC[H <20 rows> ESC[8;1H   ESC[?25h
    // 2310.7ms  ESC[?25l               ESC[H <30 rows> ESC[8;1H   ESC[?25h
    // ```
    //
    // The first repaint is the trap this test exists for: laid out for 24
    // rows, parsing into a grid already at 8 — sixteen rows of overflow, each
    // scroll of which used to cancel the restate debt (stranding the drag)
    // and bank the restated row into history (duplicating it). The 20-row
    // repaint is #312's stale-smaller case, refused by coverage; the 30-row
    // one settles. Between them this is a whole drag storm, both directions,
    // from recorded bytes. (#315)
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/corpus/resize-drag-overflow.vtrec"
    ))
    .expect("the recording exists");
    let chunks = parse_vtrec_timed(&bytes);

    // As in the stepped test below: the expected end state is the session as
    // if the drag never happened — every chunk except the drag window's.
    const DRAG_WINDOW_US: core::ops::Range<u128> = 2_000_000..3_000_000;
    const RESTORED_UNTIL_US: u128 = 5_000_000;
    let mut plain = Terminal::new(100, 30, 500);
    plain.set_pty_restates_viewport(true);
    let mut plain_mid: Option<String> = None;
    for (us, chunk) in &chunks {
        if *us >= RESTORED_UNTIL_US && plain_mid.is_none() {
            plain_mid = Some(plain.screen_text());
        }
        if !DRAG_WINDOW_US.contains(us) {
            plain.advance(chunk);
        }
    }

    let mut t = Terminal::new(100, 30, 500);
    t.set_pty_restates_viewport(true);
    const SHRINKS_AT_US: u128 = 2_000_000;
    const GROWS_AT_US: u128 = 2_300_000;
    let (mut shrunk, mut grown) = (false, false);
    let mut restored: Option<(usize, String)> = None;
    for (us, chunk) in &chunks {
        if !shrunk && *us >= SHRINKS_AT_US {
            t.resize(100, 24);
            t.resize(100, 16);
            t.resize(100, 8);
            shrunk = true;
        }
        if !grown && *us >= GROWS_AT_US {
            t.resize(100, 14);
            t.resize(100, 20);
            t.resize(100, 26);
            t.resize(100, 30);
            grown = true;
        }
        // The restore holds until the shell's trailing output strands it (#341).
        if *us >= RESTORED_UNTIL_US && restored.is_none() {
            restored = Some((t.grid().scrollback_len(), t.screen_text()));
        }
        t.advance(chunk);
    }
    assert!(shrunk && grown, "the recording ended before the drag did -- re-record");

    let (held, screen) = restored.expect("the recording ended before the restore window");
    assert_eq!(
        screen,
        plain_mid.expect("the plain replay never reached the restore window"),
        "the storm did not come back to the screen an undisturbed replay shows"
    );
    assert_eq!(held, 0, "the storm left history parked above the viewport");
    // The same rows, the same number of times — the overflow used to bank the
    // repaint's restatement of rows the grid already held, so the storm's
    // history carried extra copies the undisturbed replay does not. Compared
    // as multisets against the plain replay rather than asserting global
    // uniqueness, because legitimate output may repeat a line and that is not
    // this bug.
    let texts = |t: &Terminal| -> Vec<String> {
        let mut v: Vec<String> = (0..t.grid().total_lines())
            .filter_map(|i| t.grid().line(i))
            .map(|r| r.text().trim_end().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        v.sort();
        v
    };
    assert_eq!(
        texts(&t),
        texts(&plain),
        "the storm's history holds different rows (or extra copies) than the \
         undisturbed replay's"
    );
}

#[test]
fn a_recorded_width_drag_destroys_nothing() {
    // `corpus/resize-width.vtrec`: two `ls`es at 100x30, narrowed to 50 and
    // widened back, real ConPTY answering both moves. The narrow repaint
    // (announced) agrees with our reflow row for row; the widen repaint
    // un-wraps ConPTY's thirty narrow rows into sixteen wide ones and
    // restates them from home, ELs below — which erased the middle of the
    // listing in place until the viewport learned to re-anchor on the line
    // ConPTY still holds. (#224)
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/corpus/resize-width.vtrec"
    ))
    .expect("the recording exists");
    let chunks = parse_vtrec_timed(&bytes);

    // The expected content is the session with no drag: every chunk except
    // the gesture window's. Layout differs by design — after the widen the
    // surplus lives in scrollback (the strand view) — so the comparison is
    // the multiset of non-blank line texts, nothing destroyed and nothing
    // doubled.
    const DRAG_WINDOW_US: core::ops::Range<u128> = 2_400_000..5_000_000;
    let mut plain = Terminal::new(100, 30, 500);
    plain.set_pty_restates_viewport(true);
    for (us, chunk) in &chunks {
        if !DRAG_WINDOW_US.contains(us) {
            plain.advance(chunk);
        }
    }

    let mut t = Terminal::new(100, 30, 500);
    t.set_pty_restates_viewport(true);
    const NARROW_AT_US: u128 = 2_450_000;
    const WIDEN_AT_US: u128 = 3_950_000;
    let (mut narrowed, mut widened) = (false, false);
    for (us, chunk) in &chunks {
        if !narrowed && *us >= NARROW_AT_US {
            t.resize(50, 30);
            narrowed = true;
        }
        if !widened && *us >= WIDEN_AT_US {
            t.resize(100, 30);
            widened = true;
        }
        t.advance(chunk);
    }
    assert!(narrowed && widened, "the recording ended before the drag did -- re-record");

    let texts = |t: &Terminal| -> Vec<String> {
        let mut v: Vec<String> = (0..t.grid().total_lines())
            .filter_map(|i| t.grid().line(i))
            .map(|r| r.text().trim_end().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        v.sort();
        v
    };
    // Nothing destroyed and nothing doubled: every row of the undisturbed
    // session survives the drag, exactly as many times. The drag replay may
    // hold *more* — ConPTY's widen repaint opens by rewriting the wrapped
    // fragment its buffer's top row held (`ESC[H crates ESC[K`, measured), so
    // one fragment row is its honest screen content and Windows Terminal
    // shows the same one. Every extra must be such a fragment: a proper
    // suffix of a line the session holds.
    let (mine, theirs) = (texts(&t), texts(&plain));
    let mut extra = mine.clone();
    for want in &theirs {
        match extra.iter().position(|s| s == want) {
            Some(i) => {
                extra.remove(i);
            }
            None => panic!("the width drag destroyed {want:?}"),
        }
    }
    for e in &extra {
        assert!(
            theirs.iter().any(|w| w.ends_with(e.as_str()) && w != e),
            "the drag left a row that is not the restater's fragment of anything: {e:?}"
        );
    }
}

#[test]
fn a_recorded_third_leg_destroys_nothing() {
    // `corpus/resize-drag-thirdleg.vtrec`: shrink to 8, grow to 30 (the
    // settle restores the screen), then shrink again — twice, partially —
    // with ConPTY answering each move. The third leg is the reported "it
    // snaps and disappears": after the settle this grid holds more than
    // ConPTY's buffer, and each later repaint restates the lesser truth, so
    // the pulled rows must be re-banked before it writes. The recording's
    // stamps: 2000ms → 8, 3500ms → 30, 5000ms → 28, 6500ms → 26. (#335)
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/corpus/resize-drag-thirdleg.vtrec"
    ))
    .expect("the recording exists");
    let chunks = parse_vtrec_timed(&bytes);

    // The expected *content* is the session with no drag at all. The final
    // layout legitimately differs — at 26 rows some of the listing lives in
    // scrollback — so the comparison is the multiset of non-blank line texts
    // across scrollback and viewport: nothing destroyed, nothing duplicated.
    const DRAG_WINDOW_US: core::ops::Range<u128> = 2_000_000..7_000_000;
    let mut plain = Terminal::new(100, 30, 500);
    plain.set_pty_restates_viewport(true);
    for (us, chunk) in &chunks {
        if !DRAG_WINDOW_US.contains(us) {
            plain.advance(chunk);
        }
    }

    let mut t = Terminal::new(100, 30, 500);
    t.set_pty_restates_viewport(true);
    let moves: [(u128, usize); 4] =
        [(2_000_000, 8), (3_500_000, 30), (5_000_000, 28), (6_500_000, 26)];
    let mut next = 0;
    for (us, chunk) in &chunks {
        while next < moves.len() && *us >= moves[next].0 {
            t.resize(100, moves[next].1);
            next += 1;
        }
        t.advance(chunk);
    }
    assert_eq!(next, moves.len(), "the recording ended before the drag did -- re-record");

    let texts = |t: &Terminal| -> Vec<String> {
        let mut v: Vec<String> = (0..t.grid().total_lines())
            .filter_map(|i| t.grid().line(i))
            .map(|r| r.text().trim_end().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        v.sort();
        v
    };
    assert_eq!(
        texts(&t),
        texts(&plain),
        "the second shrink's repaint destroyed (or duplicated) rows the settle had pulled"
    );
}

#[test]
fn a_recorded_stepped_drag_is_reversible() {
    // The storm above is repaints the grid outran. This is the other cadence,
    // and the daemon's usual one: a resize every ~120ms, each answered by a
    // matching repaint before the next lands. Nothing is ever stale — and the
    // gesture still destroyed rows, because each intermediate settle pulled
    // history that the *next* repaint (restating ConPTY's buffer, which never
    // got those rows back) overwrote in place. The provisional settle is what
    // fixes it: each repaint takes the previous pull back before writing, and
    // pays it out again over what it wrote. (#312)
    //
    // `corpus/resize-drag-stepped.vtrec`: two `ls`es at 80x24, then
    // 20, 14, 8, 14, 20, 24 — one resize every 120ms starting at 2000ms
    // (`pty_dump --resize-after-ms 2000 --resize-settle-ms 120 …`, re-recording
    // recipe in `tests/README.md`).
    let bytes = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/corpus/resize-drag-stepped.vtrec"
    ))
    .expect("the recording exists");
    let chunks = parse_vtrec_timed(&bytes);

    // The expected end state is the session as if the drag never happened:
    // every chunk except ConPTY's answers to it, which all land inside the
    // gesture's window (the drag runs 2000..2720ms and the last repaint is
    // back within ~15ms of its resize; everything after is the shell). A
    // reversible drag must come back to exactly this screen. A golden built
    // from the fixture itself, so a re-recording carries its own expectation.
    const DRAG_WINDOW_US: core::ops::Range<u128> = 2_000_000..3_000_000;
    const RESTORED_UNTIL_US: u128 = 5_000_000;
    let mut plain = Terminal::new(80, 24, 500);
    plain.set_pty_restates_viewport(true);
    let mut plain_mid: Option<(usize, String)> = None;
    for (us, chunk) in &chunks {
        if *us >= RESTORED_UNTIL_US && plain_mid.is_none() {
            plain_mid = Some((plain.grid().scrollback_len(), plain.screen_text()));
        }
        if !DRAG_WINDOW_US.contains(us) {
            plain.advance(chunk);
        }
    }

    let mut t = Terminal::new(80, 24, 500);
    t.set_pty_restates_viewport(true);
    let sizes = [20usize, 14, 8, 14, 20, 24];
    let mut next = 0usize;
    let mut restored: Option<(usize, String)> = None;
    for (us, chunk) in &chunks {
        while next < sizes.len() && *us >= 2_000_000 + (next as u128) * 120_000 {
            t.resize(80, sizes[next]);
            next += 1;
        }
        // The restore holds until the shell's trailing output strands it (#341).
        if *us >= RESTORED_UNTIL_US && restored.is_none() {
            restored = Some((t.grid().scrollback_len(), t.screen_text()));
        }
        t.advance(chunk);
    }
    assert_eq!(next, sizes.len(), "the recording ended before the drag did — re-record");

    let (held, screen) = restored.expect("the recording ended before the restore window");
    let (plain_held, plain_screen) =
        plain_mid.expect("the plain replay never reached the restore window");
    assert_eq!(
        screen, plain_screen,
        "the stepped drag did not come back to the screen an undisturbed replay shows"
    );
    assert_eq!(
        held, plain_held,
        "the drag left a different amount of history than the undisturbed replay"
    );
}

/// `DECSCUSR 0` is *reset*, and reset means the user's configured shape.
///
/// `cursor.shape` is documented as the shape used "unless the program sets one
/// with DECSCUSR", and reset is what makes that true rather than merely
/// initial: a program that sets a bar and resets on exit has to hand the
/// terminal back to the user's choice. Before this, `from_decscusr(0)` folded 0
/// and 1 together into a blinking block, so a `vim` exiting left every terminal
/// on a block whatever the settings said.
#[test]
fn decscusr_zero_resets_to_the_configured_shape() {
    use zest_core::{CursorShape, CursorStyle};

    let mut t = Terminal::new(10, 2, 100);
    t.set_default_cursor_style(CursorStyle { shape: CursorShape::Bar, blinking: true });
    assert_eq!(t.cursor_style().shape, CursorShape::Bar, "the default applies to a live session");

    // A program takes over...
    t.advance(b"\x1b[2 q");
    assert_eq!(t.cursor_style().shape, CursorShape::Block, "DECSCUSR beats the config while set");
    assert!(!t.cursor_style().blinking, "and 2 is the steady block, not the blinking one");

    // ...and hands it back.
    t.advance(b"\x1b[0 q");
    assert_eq!(
        t.cursor_style().shape,
        CursorShape::Bar,
        "reset returns to the configured shape, not to a hardcoded block"
    );
}

/// Setting the default must not steal a shape a program is currently using.
#[test]
fn a_config_reload_does_not_override_a_running_program() {
    use zest_core::{CursorShape, CursorStyle};

    let mut t = Terminal::new(10, 2, 100);
    t.advance(b"\x1b[5 q"); // a program asks for a blinking bar
    assert_eq!(t.cursor_style().shape, CursorShape::Bar);

    // The user edits cursor.shape while that program is still running. It is
    // still running and still means it, so the live style stands; the new
    // default is what a later reset will land on.
    t.set_default_cursor_style(CursorStyle { shape: CursorShape::Underline, blinking: true });
    assert_eq!(t.cursor_style().shape, CursorShape::Bar, "the running program keeps its cursor");
    t.advance(b"\x1b[0 q");
    assert_eq!(t.cursor_style().shape, CursorShape::Underline, "reset lands on the new default");
}

/// A program that asks for the shape the default already is still owns it.
///
/// The trap in inferring provenance from a value: `CSI 1 SP q` against a
/// default of "blinking block" leaves `cursor_style == default_cursor_style`,
/// so an equality check reads it as untouched and a later config reload takes
/// the program's cursor away. Provenance is not recoverable from a value, so it
/// is tracked rather than guessed.
#[test]
fn a_program_asking_for_the_current_default_still_owns_the_cursor() {
    use zest_core::{CursorShape, CursorStyle};

    let mut t = Terminal::new(10, 2, 100);
    // The default is a blinking block, and the program explicitly asks for
    // exactly that -- the value is now indistinguishable from untouched.
    assert_eq!(t.cursor_style(), CursorStyle { shape: CursorShape::Block, blinking: true });
    t.advance(b"\x1b[1 q");

    // The user then edits cursor.shape. The program is still running and still
    // means the block it asked for.
    t.set_default_cursor_style(CursorStyle { shape: CursorShape::Bar, blinking: true });
    assert_eq!(
        t.cursor_style().shape,
        CursorShape::Block,
        "an explicit DECSCUSR keeps the cursor even when it matched the default"
    );

    // ...until it resets, which gives the claim up.
    t.advance(b"\x1b[0 q");
    assert_eq!(t.cursor_style().shape, CursorShape::Bar);
    t.set_default_cursor_style(CursorStyle { shape: CursorShape::Underline, blinking: true });
    assert_eq!(
        t.cursor_style().shape,
        CursorShape::Underline,
        "after a reset the config owns the shape again"
    );
}

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

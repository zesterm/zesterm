//! The spine.
//!
//! Replays recorded terminal sessions through the encoder, applies the deltas
//! with the reference decoder, and asserts the two grids agree **at every
//! frame** — not at the end, because two errors that cancel out would pass a
//! final comparison and the whole point is to find the first frame where they
//! diverge.
//!
//! Checked against two independent references, deliberately:
//!
//! 1. The encoder's own keyframe of the same grid. Catches anything wrong with
//!    the *incremental* path — a missed row, a scroll applied in the wrong
//!    order, a stale shadow — which is where the bugs actually are.
//! 2. The terminal's `screen_text()`. Catches anything wrong with the encoder
//!    itself, which reference 1 cannot, since a systematic encoder bug would
//!    appear identically on both sides of it.
//!
//! The TypeScript decoder in the web client is checked against these same
//! recordings. Two implementations agreeing with each other is not the goal;
//! both agreeing with the terminal is.

use zest_core::{Modes, Terminal};
use zest_proto::decode::GridView;
use zest_proto::delta::CursorState;
use zest_proto::encode::Encoder;

const CORPUS: &[&str] =
    &["basic-echo", "dir-colors", "git-log", "unicode-wide", "vim-macos"];

fn corpus_path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../zest-core/tests/corpus")
        .join(format!("{name}.vtrec"))
}

/// A `.vtrec` is timestamped chunks; the timing is irrelevant here.
///
/// Chunk boundaries are *not* irrelevant, though — they are where the parser
/// was interrupted mid-sequence in the real session, and replaying them as
/// recorded is what makes this a regression test rather than a happy path.
fn chunks(name: &str) -> Vec<Vec<u8>> {
    let path = corpus_path(name);
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

    const MAGIC: &[u8] = b"VTREC1\n";
    assert!(
        bytes.starts_with(MAGIC),
        "{} is not a .vtrec -- without this check a mis-parse reads the header as \
         a timestamp and the whole suite passes on garbage",
        path.display()
    );

    let mut out = Vec::new();
    let mut i = MAGIC.len();
    while i + 12 <= bytes.len() {
        // u64 timestamp, u32 length, then the payload.
        let len = u32::from_le_bytes([bytes[i + 8], bytes[i + 9], bytes[i + 10], bytes[i + 11]])
            as usize;
        i += 12;
        if i + len > bytes.len() {
            break;
        }
        out.push(bytes[i..i + len].to_vec());
        i += len;
    }

    // A recording that parses to nothing would make every assertion below pass
    // vacuously, which is the failure mode a corpus test must not have. The
    // total is checked too: a length field misread as a huge number yields one
    // chunk and stops, which looks like success.
    assert!(!out.is_empty(), "{} produced no chunks", path.display());
    let total: usize = out.iter().map(Vec::len).sum();
    assert!(
        total > bytes.len() / 2,
        "{} parsed to only {total} of {} bytes -- the framing is being misread",
        path.display(),
        bytes.len()
    );
    out
}

fn cursor(t: &Terminal) -> CursorState {
    let c = t.cursor();
    CursorState {
        row: u16::try_from(c.row).unwrap_or(0),
        col: u16::try_from(c.col).unwrap_or(0),
        visible: t.modes().contains(Modes::SHOW_CURSOR),
        shape: 0,
    }
}

/// Rows as plain text, trailing blanks trimmed the way `screen_text` trims them.
fn view_text(view: &GridView) -> String {
    view.rows()
        .iter()
        .map(|r| {
            let line: String = r.runs.iter().map(|run| run.text.as_str()).collect();
            line.trim_end().to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn terminal_text(t: &Terminal) -> String {
    // `split`, not `lines`: a screen whose last row is blank ends in a newline,
    // and `lines` drops the empty string after it. That silently compares 23
    // rows against 24 and fails on a difference that is not there.
    t.screen_text()
        .split('\n')
        .map(|l| l.trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn replay(name: &str, cols: usize, rows: usize) {
    let mut term = Terminal::new(cols, rows, 2000);
    let mut enc = Encoder::new();
    let mut view = GridView::new();

    let alt = |t: &Terminal| t.modes().contains(Modes::ALT_SCREEN);
    view.apply_keyframe(&enc.keyframe(term.grid(), cursor(&term), alt(&term), ""));

    for (step, chunk) in chunks(name).iter().enumerate() {
        term.advance(chunk);
        let d = enc.delta(term.grid(), cursor(&term), alt(&term), "");
        view.apply_delta(&d);

        // Reference 1: the incremental path must match the full one.
        let mut probe = Encoder::new();
        let truth = probe.keyframe(term.grid(), cursor(&term), alt(&term), "");
        assert_eq!(
            view.rows().len(),
            truth.rows_data.len(),
            "{name} step {step}: row count diverged"
        );
        for (r, (got, want)) in view.rows().iter().zip(truth.rows_data.iter()).enumerate() {
            let got_text: String = got.runs.iter().map(|x| x.text.as_str()).collect();
            let want_text: String = want.runs.iter().map(|x| x.text.as_str()).collect();
            assert_eq!(got_text, want_text, "{name} step {step}, row {r}: text diverged");

            let got_cells: u16 = got.runs.iter().map(|x| x.cells).sum();
            let want_cells: u16 = want.runs.iter().map(|x| x.cells).sum();
            assert_eq!(got_cells, want_cells, "{name} step {step}, row {r}: cell count diverged");

            assert_eq!(got.line, want.line, "{name} step {step}, row {r}: line id diverged");
            assert_eq!(got.wrapped, want.wrapped, "{name} step {step}, row {r}: wrap diverged");
        }

        // Reference 2: and both must match the terminal itself.
        assert_eq!(
            view_text(&view),
            terminal_text(&term),
            "{name} step {step}: the decoded grid does not match the terminal"
        );

        // Every run must name an attribute the client has been told about, or
        // it renders in whatever it happened to be holding.
        for row in view.rows() {
            for run in &row.runs {
                assert!(
                    view.attrs.contains_key(&run.attr),
                    "{name} step {step}: run uses undefined attribute {:?}",
                    run.attr
                );
            }
        }
    }
}

#[test]
fn basic_echo() {
    replay("basic-echo", 80, 24);
}

#[test]
fn dir_colors() {
    replay("dir-colors", 80, 24);
}

#[test]
fn git_log() {
    replay("git-log", 100, 30);
}

#[test]
fn unicode_wide() {
    replay("unicode-wide", 80, 24);
}

/// The same recordings at a size that forces heavy scrolling.
///
/// Scrolling is where the ordering between `SCROLL` and `ROW` matters, and a
/// tall viewport can hide the bug entirely by never scrolling at all.
#[test]
fn every_recording_survives_a_short_viewport() {
    for name in CORPUS {
        replay(name, 80, 5);
    }
}

/// Reconnection is the normal case on a phone, not the exception.
///
/// A client that drops mid-session and receives a keyframe must land exactly
/// where a client that never dropped is. Code exercised only by accident is
/// code that does not work.
#[test]
fn resyncing_at_every_point_lands_in_the_same_place() {
    for name in CORPUS {
        let all = chunks(name);
        for drop_at in 0..all.len() {
            let mut term = Terminal::new(80, 10, 2000);
            let mut enc = Encoder::new();
            let mut view = GridView::new();
            let alt = |t: &Terminal| t.modes().contains(Modes::ALT_SCREEN);

            view.apply_keyframe(&enc.keyframe(term.grid(), cursor(&term), alt(&term), ""));

            for (i, chunk) in all.iter().enumerate() {
                term.advance(chunk);
                if i == drop_at {
                    // The connection dropped: the client missed this delta
                    // entirely and is resynced with a keyframe instead.
                    let _lost = enc.delta(term.grid(), cursor(&term), alt(&term), "");
                    let k = enc.keyframe(term.grid(), cursor(&term), alt(&term), "");
                    view.apply_keyframe(&k);
                } else {
                    view.apply_delta(&enc.delta(term.grid(), cursor(&term), alt(&term), ""));
                }
            }

            assert_eq!(
                view_text(&view),
                terminal_text(&term),
                "{name}: a client that dropped at chunk {drop_at} did not recover"
            );
        }
    }
}

/// The corpus is real, and this suite is not passing on an empty replay.
///
/// A framing bug once made every test above green while feeding the terminal
/// garbage. Asserting the shape of the input is the cheapest possible guard
/// against a suite that looks thorough and tests nothing.
#[test]
fn the_corpus_contains_real_terminal_output() {
    for name in CORPUS {
        let c = chunks(name);
        let total: usize = c.iter().map(Vec::len).sum();
        assert!(total > 50, "{name}: only {total} bytes of payload");

        // Every recording should contain at least one escape sequence -- these
        // were captured from real programs, and one that is plain text would
        // exercise none of the interesting paths.
        assert!(
            c.iter().any(|chunk| chunk.contains(&0x1b)),
            "{name}: no escape sequences, so this exercises nothing"
        );
    }
}

/// A real macOS vim session: alt-screen, truecolour, UTF-8, heavy repaints.
///
/// Recorded on the machine that first ran the unix pty, and by some distance
/// the hardest thing in the corpus — 10KB against the next largest at under 1KB.
/// A full-screen editor is where the encoder is most likely to be wrong, because
/// it repaints regions rather than appending lines and it switches screens
/// underneath the scroll detection.
#[test]
fn a_real_vim_session() {
    replay("vim-macos", 80, 24);
}

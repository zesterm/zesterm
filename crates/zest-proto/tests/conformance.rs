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
use zest_proto::apply::{Applied, Applier};
use zest_proto::decode::GridView;
use zest_proto::delta::{BlockPayload, CursorState};
use zest_proto::encode::Encoder;

const CORPUS: &[&str] = &[
    "basic-echo",
    "dir-colors",
    "git-log",
    "unicode-wide",
    "vim-macos",
    "blocks-zsh",
    "blocks-pwsh",
];

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

    // The third participant: a real `Terminal` the deltas are applied into,
    // which is what a Rust client actually holds. `GridView` is the reference
    // for the TypeScript clients and keeps a flat row list; comparing only
    // against it would miss everything that depends on there being two grids,
    // a scrollback, and a cursor -- which is where the alt-screen ordering bug
    // lived.
    let mut client = Terminal::new(cols, rows, 2000);
    let mut applier = Applier::new();

    let k = enc.keyframe(term.grid(), cursor(&term), term.modes(), "", term.blocks());
    view.apply_keyframe(&k);
    applier.apply_keyframe(&mut client, &k, 0);

    for (step, chunk) in chunks(name).iter().enumerate() {
        term.advance(chunk);
        let d = enc.delta(term.grid(), cursor(&term), term.modes(), "", term.blocks());

        // Both ordering invariants, on every real delta the encoder produces.
        // These are producer bugs, so asserting here catches them against five
        // recorded sessions rather than against hand-built fixtures.
        assert!(d.scrolls_come_first(), "{name} step {step}: {:?}", d.ops);
        assert!(d.screen_switch_comes_first(), "{name} step {step}: {:?}", d.ops);

        view.apply_delta(&d);
        let base = applier.applied();
        let applied = applier.apply_delta(&mut client, &d, base, term.seq());
        assert_eq!(
            applied,
            Applied::Ok,
            "{name} step {step}: the applier could not apply a delta the encoder \
             produced for a grid of the same size"
        );

        // Reference 1: the incremental path must match the full one.
        let mut probe = Encoder::new();
        let truth = probe.keyframe(term.grid(), cursor(&term), term.modes(), "", term.blocks());
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

        // Reference 3: two real terminals, cell for cell.
        //
        // This is the claim CONTRACTS.md makes -- that the renderer's path is
        // identical at both ends -- and only two real `Terminal`s can
        // demonstrate it. The text comparisons above are projections, and a
        // projection can agree while the things behind it differ: same
        // characters, wrong colours.
        assert_cells_agree(&term, &client, name, step);

        // State the renderer reads that no text comparison covers.
        assert_eq!(
            client.modes().contains(Modes::ALT_SCREEN),
            term.modes().contains(Modes::ALT_SCREEN),
            "{name} step {step}: the client is on the wrong screen"
        );
        assert_eq!(
            client.grid().row(0).id,
            term.grid().row(0).id,
            "{name} step {step}: absolute line ids diverged -- selections and \
             command blocks would name different rows on the two machines"
        );
        assert_eq!(
            client.seq(),
            term.seq(),
            "{name} step {step}: the client would acknowledge the wrong sequence"
        );
        assert_blocks_agree(&term, &client, &view, name, step);
    }
}

/// All three participants must hold the same command blocks.
///
/// `blocks-zsh` is what makes this bite: a real interactive `zsh` recorded
/// through a real pty with zesterm's shell integration injected, so the markers
/// are the ones a shell actually emits rather than ones a test author chose.
/// The other five predate shell integration and hold no blocks at all, which is
/// worth keeping — they assert that a session *without* markers produces an
/// empty index on both sides rather than a spurious one.
fn assert_blocks_agree(
    host: &Terminal,
    client: &Terminal,
    view: &GridView,
    name: &str,
    step: usize,
) {
    assert_eq!(
        client.blocks().blocks(),
        host.blocks().blocks(),
        "{name} step {step}: the client's command blocks diverged from the host's"
    );

    // The TypeScript reference has to agree too, and it holds the wire form
    // rather than the core one -- so compare through the conversion, which is
    // also the thing a client author gets wrong.
    let host_wire: Vec<_> = host.blocks().blocks().iter().map(BlockPayload::from_block).collect();
    assert_eq!(
        view.blocks, host_wire,
        "{name} step {step}: GridView and the host disagree about the block list"
    );
}

/// The two references must agree about an op neither has ever received.
///
/// `DeltaOp::Erase` is not emitted by the current encoder, so nothing in the
/// corpus exercises it and the replay above cannot reach it. Both `GridView`
/// A session with real command blocks, driven all the way through the wire.
///
/// The corpus cannot cover this: none of the five recordings contains OSC 133,
/// because nothing emitted it when they were captured. So this is the test that
/// makes `assert_blocks_agree` mean something, in the same shape as
/// `both_references_read_erase_the_same_way` below — a hand-built session for
/// something the recordings cannot reach.
///
/// It drives a *running* command as well as a finished one, because those are
/// two different wire states and only one of them has an end line.
#[test]
fn blocks_survive_the_wire() {
    let mut host = Terminal::new(40, 6, 200);
    let mut client = Terminal::new(40, 6, 200);
    let mut enc = Encoder::new();
    let mut view = GridView::new();
    let mut applier = Applier::new();

    let k = enc.keyframe(host.grid(), cursor(&host), host.modes(), "", host.blocks());
    view.apply_keyframe(&k);
    applier.apply_keyframe(&mut client, &k, host.seq());

    // Each chunk is one step a shell would actually produce, applied on its own
    // so a block is compared while half-formed rather than only once complete.
    let script: &[&[u8]] = &[
        b"\x1b]7;file:///tmp/build\x07\x1b]133;A\x07$ ",
        b"\x1b]133;B\x07cargo build\x1b]133;C\x07\r\n",
        b"   Compiling zest-core\r\n",
        b"\x1b]133;D;0\x07\x1b]133;A\x07$ ",
        b"\x1b]133;B\x07tail -f log\x1b]133;C\x07\r\n",
        b"waiting\r\n",
    ];

    for (step, chunk) in script.iter().enumerate() {
        host.advance(chunk);
        let d = enc.delta(host.grid(), cursor(&host), host.modes(), "", host.blocks());
        view.apply_delta(&d);
        let base = applier.applied();
        assert_eq!(
            applier.apply_delta(&mut client, &d, base, host.seq()),
            Applied::Ok,
            "step {step}"
        );
        assert_blocks_agree(&host, &client, &view, "blocks", step);
    }

    // And the host actually produced what the test claims to be checking —
    // without this, an index that stayed empty would satisfy every assertion
    // above.
    let blocks = client.blocks().blocks();
    assert_eq!(blocks.len(), 2, "two prompts, two blocks");
    assert_eq!(blocks[0].command, "cargo build");
    assert_eq!(blocks[0].cwd, "/tmp/build");
    assert!(!blocks[0].failed());
    assert_eq!(blocks[1].command, "tail -f log");
    assert!(blocks[1].is_running(), "the second command has not ended");
    assert_eq!(blocks[1].end_line, None, "so it has no end line to send");
}

/// An unchanged block is not re-sent.
///
/// The 0%-idle guarantee extended to blocks. Sending the whole index whenever
/// anything at all changed would pass every correctness test above while
/// putting a week of command history on the wire on each keystroke — and the
/// daemon's empty-delta check would stop suppressing anything, because no delta
/// would ever be empty again.
#[test]
fn blocks_are_sent_once_and_not_repeated() {
    let mut host = Terminal::new(40, 6, 200);
    let mut enc = Encoder::new();
    enc.keyframe(host.grid(), cursor(&host), host.modes(), "", host.blocks());

    host.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07ls\x1b]133;C\x07\r\n");
    let first = enc.delta(host.grid(), cursor(&host), host.modes(), "", host.blocks());
    assert_eq!(first.blocks.len(), 1, "the new block is sent");

    // Output that does not touch the block: it is still running, still the same
    // command, still open-ended.
    host.advance(b"a\r\n");
    let second = enc.delta(host.grid(), cursor(&host), host.modes(), "", host.blocks());
    assert!(
        second.blocks.is_empty(),
        "an unchanged block was re-sent: {:?}",
        second.blocks
    );

    // And when it does change, it is sent again -- so the emptiness above is
    // suppression, not a shadow that stopped updating.
    host.advance(b"\x1b]133;D;0\x07");
    let third = enc.delta(host.grid(), cursor(&host), host.modes(), "", host.blocks());
    assert_eq!(third.blocks.len(), 1, "the finished block is sent");
    assert!(third.blocks[0].end_line.is_some());
}

/// A client that attaches to a session already running carries its history.
///
/// The delta path above only ever sends what *changed*, so a client joining
/// late learns nothing from it. A week-old session's command history is exactly
/// the keyframe's block list, and there is no other way to obtain it.
#[test]
fn a_keyframe_carries_every_block_a_late_client_missed() {
    let mut host = Terminal::new(40, 6, 200);
    host.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07make\x1b]133;C\x07\r\nout\r\n\x1b]133;D;2\x07");

    // A brand-new subscriber, as `Session::attach_with` builds one.
    let mut enc = Encoder::new();
    let mut client = Terminal::new(40, 6, 200);
    let mut applier = Applier::new();
    let k = enc.keyframe(host.grid(), cursor(&host), host.modes(), "", host.blocks());

    assert_eq!(k.blocks.len(), 1, "the keyframe carries the history, not just changes");
    applier.apply_keyframe(&mut client, &k, host.seq());

    let b = client.blocks().last().expect("the client learned about it");
    assert_eq!(b.command, "make");
    assert!(b.failed(), "exit 2 crossed the wire as a failure");
}

/// and `Applier` implement it, and if their readings differ, the first person
/// to find out is whoever writes the encoder that starts sending it — or worse,
/// a client author whose grid quietly disagrees with the desktop's.
#[test]
fn both_references_read_erase_the_same_way() {
    let mut term = Terminal::new(20, 3, 100);
    term.advance(b"abcdefghij\r\nklmnopqrst\r\nuvwxyz");

    let mut enc = Encoder::new();
    let mut view = GridView::new();
    let mut client = Terminal::new(20, 3, 100);
    let mut applier = Applier::new();

    let k = enc.keyframe(term.grid(), cursor(&term), term.modes(), "", term.blocks());
    view.apply_keyframe(&k);
    applier.apply_keyframe(&mut client, &k, term.seq());

    let bg = zest_core::Color::Indexed(4);
    let attr = zest_proto::AttrId(9_000);
    let d = zest_proto::Delta {
        blocks: Vec::new(),
        attrs: vec![zest_proto::AttrDef {
            id: attr,
            fg: zest_core::Color::Default,
            bg,
            flags: zest_core::CellFlags::empty(),
        }],
        ops: vec![zest_proto::DeltaOp::Erase { top: 0, left: 2, bottom: 1, right: 5, attr }],
    };

    view.apply_delta(&d);
    let base = applier.applied();
    assert_eq!(applier.apply_delta(&mut client, &d, base, term.seq() + 1), Applied::Ok);

    for r in 0..2usize {
        let flat: String = view.rows()[r]
            .runs
            .iter()
            .flat_map(|run| {
                let mut chars = run.text.chars();
                (0..run.cells).map(move |_| chars.next().unwrap_or(' ')).collect::<Vec<_>>()
            })
            .collect();
        let from_terminal: String =
            (0..20).map(|c| client.grid().cell(r, c).map_or(' ', |x| x.ch)).collect();
        assert_eq!(
            flat.trim_end(),
            from_terminal.trim_end(),
            "row {r}: the two references disagree about Erase"
        );

        for c in 2..=5usize {
            assert_eq!(
                client.grid().cell(r, c).unwrap().bg,
                bg,
                "row {r} col {c}: Erase must paint the attribute it names"
            );
        }
    }
}

/// Compare every cell of the host's grid against the client's.
///
/// **One field is excluded, and it is named in the failure message** so that a
/// later failure cannot be "fixed" by quietly widening the exclusion list. It
/// is not an applier bug: it is a pre-existing gap in the *encoder* that this
/// comparison is what finally documents.
fn assert_cells_agree(host: &Terminal, client: &Terminal, name: &str, step: usize) {
    let (hg, cg) = (host.grid(), client.grid());
    assert_eq!(cg.rows(), hg.rows(), "{name} step {step}: grid height diverged");
    assert_eq!(cg.cols(), hg.cols(), "{name} step {step}: grid width diverged");

    for r in 0..hg.rows() {
        for c in 0..hg.cols() {
            let (h, k) = (hg.cell(r, c).copied(), cg.cell(r, c).copied());
            let (Some(h), Some(k)) = (h, k) else { continue };

            assert_eq!(
                k.ch, h.ch,
                "{name} step {step}: character differs at row {r} col {c}\n\
                 host {:?} client {:?}",
                h.ch, k.ch
            );
            assert_eq!(
                k.bg, h.bg,
                "{name} step {step}: background differs at row {r} col {c}"
            );
            assert_eq!(
                k.flags, h.flags,
                "{name} step {step}: flags differ at row {r} col {c}"
            );

            // Excluded: foreground on a cell that paints nothing.
            //
            // `Encoder::encode_row` trims trailing cells by testing `ch`, `bg`
            // and `flags` -- not `fg` -- so a run of coloured spaces at the end
            // of a row is dropped and rebuilt with the default foreground.
            // Invisible to a renderer, because there is no glyph to colour.
            let paints = h.ch != ' ' || h.bg != zest_core::Color::Default;
            if paints {
                assert_eq!(
                    k.fg, h.fg,
                    "{name} step {step}: foreground differs at row {r} col {c} \
                     on a cell that paints"
                );
            }
        }
    }

    // Combining marks used to be excluded here, because the encoder wrote
    // `Cell::ch` and never touched the side table -- `e` + U+0301 arrived as a
    // bare `e`. `Run::marks` carries them now, so the exclusion is gone and
    // what remains is compared rather than described.
    for r in 0..hg.rows() {
        assert_eq!(
            cg.row(r).text(),
            hg.row(r).text(),
            "{name} step {step}: row {r} differs once combining marks are included"
        );
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

/// A real `zsh` with shell integration, so the block comparison is not vacuous.
///
/// The other recordings predate shell integration and carry no markers, which
/// makes them a useful negative — but only this one asserts that blocks a shell
/// actually emitted survive encode, wire and apply, frame by frame.
#[test]
fn blocks_zsh() {
    replay("blocks-zsh", 120, 30);
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
        
            view.apply_keyframe(&enc.keyframe(term.grid(), cursor(&term), term.modes(), "", term.blocks()));

            for (i, chunk) in all.iter().enumerate() {
                term.advance(chunk);
                if i == drop_at {
                    // The connection dropped: the client missed this delta
                    // entirely and is resynced with a keyframe instead.
                    let _lost = enc.delta(term.grid(), cursor(&term), term.modes(), "", term.blocks());
                    let k = enc.keyframe(term.grid(), cursor(&term), term.modes(), "", term.blocks());
                    view.apply_keyframe(&k);
                } else {
                    view.apply_delta(&enc.delta(term.grid(), cursor(&term), term.modes(), "", term.blocks()));
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

/// A recorded ConPTY height drag, with all three participants along for it.
///
/// Not in `CORPUS`, deliberately: its repaints are laid out for the specific
/// sizes of the recorded gesture, so the generic replays would feed them into
/// wrong-sized grids. Driven here the way the daemon drives it — the host
/// resized at the recorded moments, a keyframe pushed to every client on each
/// resize (`reconcile_size`) and on every `ViewportRebased`/`HistoryCleared`
/// (`keyframe_everyone`) — because until now the conformance suite never saw a
/// resize at all, which is how three different un-bank rules shipped in three
/// readers. (#313)
#[test]
fn a_recorded_conpty_drag_keeps_all_three_participants_agreeing() {
    use zest_core::TermEvent;

    let path = corpus_path("resize-drag");
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert!(bytes.starts_with(b"VTREC1\n"), "not a .vtrec");
    let mut timed = Vec::new();
    let mut i = 7;
    while i + 12 <= bytes.len() {
        let us = u64::from_le_bytes(bytes[i..i + 8].try_into().unwrap());
        let len = u32::from_le_bytes(bytes[i + 8..i + 12].try_into().unwrap()) as usize;
        timed.push((us, bytes[i + 12..i + 12 + len].to_vec()));
        i += 12 + len;
    }

    // The geometry and resize timing are the fixture's, not the file's — the
    // same constants as the replay in `zest-core/tests/vt.rs`.
    let mut term = Terminal::new(100, 30, 500);
    term.set_pty_restates_viewport(true);
    let mut enc = Encoder::new();
    let mut view = GridView::new();
    let mut client = Terminal::new(100, 30, 500);
    let mut applier = Applier::new();
    let mut seq = 0u64;

    let keyframe_everyone =
        |term: &mut Terminal,
         enc: &mut Encoder,
         view: &mut GridView,
         client: &mut Terminal,
         applier: &mut Applier,
         seq: &mut u64| {
            *seq += 1;
            let k = enc.keyframe(term.grid(), cursor(term), term.modes(), "", term.blocks());
            view.apply_keyframe(&k);
            applier.apply_keyframe(client, &k, *seq);
        };

    const SHRINK_AT_US: u64 = 1_400_000;
    const GROW_AT_US: u64 = 3_600_000;
    let (mut shrunk, mut grown) = (false, false);
    for (step, (us, chunk)) in timed.iter().enumerate() {
        if !shrunk && *us >= SHRINK_AT_US {
            term.resize(100, 8);
            keyframe_everyone(&mut term, &mut enc, &mut view, &mut client, &mut applier, &mut seq);
            shrunk = true;
        }
        if !grown && *us >= GROW_AT_US {
            term.resize(100, 30);
            keyframe_everyone(&mut term, &mut enc, &mut view, &mut client, &mut applier, &mut seq);
            grown = true;
        }
        term.advance(chunk);
        if term
            .take_events()
            .iter()
            .any(|e| matches!(e, TermEvent::ViewportRebased | TermEvent::HistoryCleared))
        {
            keyframe_everyone(&mut term, &mut enc, &mut view, &mut client, &mut applier, &mut seq);
        } else {
            seq += 1;
            let d = enc.delta(term.grid(), cursor(&term), term.modes(), "", term.blocks());
            view.apply_delta(&d);
            let base = applier.applied();
            let applied = applier.apply_delta(&mut client, &d, base, seq);
            assert_eq!(applied, Applied::Ok, "step {step}: a delta did not apply");
        }

        // Text truth, both clients, every frame.
        assert_eq!(
            view_text(&view),
            terminal_text(&term),
            "step {step}: the view diverged from the host"
        );
        assert_eq!(
            terminal_text(&client),
            terminal_text(&term),
            "step {step}: the client terminal diverged from the host"
        );

        // No line held twice, in either client. This is the assertion three
        // divergent un-bank rules were all failing in different ways.
        let mut ids: Vec<u64> =
            (0..client.grid().total_lines()).map(|i| client.grid().line(i).unwrap().id).collect();
        let total = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), total, "step {step}: the client terminal holds a line twice");
        let mut lines: Vec<i64> = view
            .scrollback
            .iter()
            .chain(view.rows().iter())
            .map(|r| r.line)
            .filter(|&l| l != i64::MIN)
            .collect();
        let total = lines.len();
        lines.sort_unstable();
        lines.dedup();
        assert_eq!(lines.len(), total, "step {step}: the view holds a line twice");
    }
    assert!(shrunk && grown, "the recording ended before the drag did");

    assert_eq!(term.grid().scrollback_len(), 0, "the host did not settle -- see vt.rs");
    assert_eq!(
        client.grid().scrollback_len(),
        term.grid().scrollback_len(),
        "the client and the host disagree about where history ends"
    );
}

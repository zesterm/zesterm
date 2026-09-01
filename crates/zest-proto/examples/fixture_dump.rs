//! The conformance corpus, written out as fixtures a TypeScript client is held to.
//!
//! `tests/conformance.rs` proves the Rust decoder agrees with the terminal at
//! every frame. This replays exactly the same thing and writes it to disk, so
//! the web and phone clients can be held to that standard without a Rust
//! toolchain — which is what lets the decoder be built and checked long before
//! a browser can reach a daemon.
//!
//! # Two independent things per frame
//!
//! `wire` is the **complete framed bytes**, length prefix included, as hex. A
//! decoded-to-JSON dump would have been smaller and would have tested almost
//! nothing: the client's own framing, its MessagePack reader, the `t`/`op`
//! internal tags, `Color`'s three shapes and the absence of `marks` on a run
//! that has none are all things only the real bytes exercise.
//!
//! `expect` is read off the **host `Terminal`** — never off `GridView`, and
//! never off the encoder. An expectation taken from the decoder under test
//! passes for any two implementations that are wrong in the same way, which is
//! the one failure a cross-implementation fixture exists to rule out. It is the
//! same discipline `conformance.rs` calls "Reference 2".
//!
//! # Why an example and not a test
//!
//! `cargo test --workspace` runs on three operating systems, so a test that
//! writes into the source tree makes every one of them a source of line-ending
//! churn — the reason `check-bindings` keeps `ts-rs` behind a feature. This is
//! the generator; `cargo xtask check-fixtures` is the gate.
//!
//! It is also the repo's own idiom for "which layer is wrong": when the
//! TypeScript suite reports a mismatch at one frame,
//! `--only vim-macos --print 7` prints that frame decoded and writes nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Serialize;
use zest_core::{CellFlags, Color, Modes, Terminal};
use zest_proto::apply::{Applied, Applier};
use zest_proto::{
    BlockPayload, ClientId, ClientMessage, CursorState, Delta, DeltaOp, Encoder, GridView, HostId,
    HostMessage, Nonce32, PROTOCOL_VERSION, Seq, SessionAddr, SessionId, Sig64,
};

/// The fixture format's own version, independent of the protocol's.
///
/// A client asserts it on load: the other way a golden file goes stale is a
/// change to the *shape* of these files without regenerating them, which would
/// otherwise show up as a field silently read as `undefined`.
const FIXTURE_SCHEMA: u32 = 1;

/// Where one fixture's input comes from.
enum Source {
    /// A recording in `zest-core/tests/corpus`, by name.
    Vtrec(&'static str),
    /// Literal VT input with resizes in it, for the one path plain input
    /// cannot reach: a resize is not bytes on the pty, it is a call the host
    /// makes, so a fixture containing one has to say where.
    Script(&'static [Step]),
}

/// One step of a [`Source::Script`].
enum Step {
    Text(&'static str),
    /// The host resizes mid-stream, delivered the way the daemon delivers it
    /// (`reconcile_size`): the grid reflows, then every subscriber gets a
    /// keyframe under the new numbering.
    Resize(u16, u16),
}

/// The recordings, at the sizes `conformance.rs` replays them.
///
/// Same sizes deliberately — a fixture that diverges from the Rust replay it is
/// meant to mirror is a second thing to keep in sync for no benefit.
///
/// `combining-marks` is not a recording. **No recording in the corpus contains a
/// single combining mark**, which is not obvious and is worth stating: the side
/// table `Run::marks` carries has real coverage in `apply.rs`'s unit tests and
/// none at all in the five replays, so `conformance.rs` dropping its exclusion
/// for marks proved less than it appears to. A client reading `at` as an offset
/// into the row rather than into its run would pass every recorded fixture. The
/// synthetic session below is the smallest thing that catches it.
const CORPUS: &[(&str, usize, usize, Source)] = &[
    ("basic-echo", 80, 24, Source::Vtrec("basic-echo")),
    ("dir-colors", 80, 24, Source::Vtrec("dir-colors")),
    ("git-log", 100, 30, Source::Vtrec("git-log")),
    ("unicode-wide", 80, 24, Source::Vtrec("unicode-wide")),
    ("vim-macos", 80, 24, Source::Vtrec("vim-macos")),
    // The fixture that holds a TypeScript client to `Delta::blocks`. Without it
    // a client could ignore the field entirely and pass every fixture here — the
    // same gap `combining-marks` exists to close, and for the same reason: the
    // other recordings predate the thing being tested.
    //
    // `blocks-pwsh` is deliberately *not* a second one. It exists in the corpus
    // and `conformance.rs` replays it, but what it adds over this — a command
    // taken from `633;E` instead of read back off the grid, a real native exit
    // code, an escaped Windows cwd — is all host-side parsing. Blocks reach a
    // client already parsed, so the wire shape is identical and a second fixture
    // would be a second thing to regenerate for no coverage.
    ("blocks-zsh", 120, 30, Source::Vtrec("blocks-zsh")),
    // The same recordings at a viewport short enough to force heavy scrolling,
    // mirroring `conformance.rs::every_recording_survives_a_short_viewport`.
    //
    // Not redundant, and measurably so: at the natural sizes only `vim-macos`
    // scrolls enough that reordering `scroll` after `row` changes the result, so
    // the ordering invariant — which is the one thing `Delta::scrolls_come_first`
    // exists to protect — had a single fixture standing behind it. A tall
    // viewport hides the bug by never scrolling at all.
    ("basic-echo-short", 80, 5, Source::Vtrec("basic-echo")),
    ("dir-colors-short", 80, 5, Source::Vtrec("dir-colors")),
    ("git-log-short", 80, 5, Source::Vtrec("git-log")),
    ("unicode-wide-short", 80, 5, Source::Vtrec("unicode-wide")),
    ("vim-macos-short", 80, 5, Source::Vtrec("vim-macos")),
    // Decomposed Unicode through a real ConPTY (#17), replacing the synthetic
    // byte list this entry started as: a mark inside a coloured run, one after
    // the run boundary (the case `CellMarks::at` exists for — the offset is
    // within the run, not within the row), one riding bold+underline, and one
    // on a double-width character, where the width rule and the mark offset
    // interact.
    ("combining-marks", 40, 6, Source::Vtrec("combining-marks")),
    (
        "width-change",
        40,
        6,
        // **No recording in the corpus changes width mid-stream**, which is how
        // the reflow rule stayed latent (#139): a width change renumbers every
        // absolute line id, the keyframe after it carries the new numbering,
        // and everything a client banked — scrollback rows, evicted blocks —
        // still carries the old one with no mapping on the wire. A client that
        // keeps that state passes every other fixture here and misjoins rows
        // into the wrong blocks the first time a real window is dragged
        // narrower. Hand-built like `combining-marks`, and for the same reason:
        // the path under test is the plain reflow every host shares, not a
        // ConPTY restatement gesture, so literal input reaches it directly.
        //
        // The shape matters: real banked history and a finished block *before*
        // the resize (a fixture must reach the state under test — the harness
        // gap that let #291 escape), a line long enough to rewrap at the new
        // width so the reflow provably renumbers, and output *after* it so
        // re-banking under the new numbering is exercised too.
        Source::Script(&[
            Step::Text("\u{1b}]7;file:///tmp/w\u{7}\u{1b}]133;A\u{7}$ "),
            Step::Text("\u{1b}]133;B\u{7}make\u{1b}]133;C\u{7}\r\n"),
            Step::Text("a line that is long enough to rewrap\r\n"),
            Step::Text("out 1\r\nout 2\r\nout 3\r\n"),
            Step::Text("out 4\r\nout 5\r\nout 6\r\n"),
            Step::Text("\u{1b}]133;D;0\u{7}\u{1b}]133;A\u{7}$ "),
            Step::Resize(20, 6),
            Step::Text("\u{1b}]133;B\u{7}ls\u{1b}]133;C\u{7}\r\n"),
            Step::Text("post 1\r\npost 2\r\npost 3\r\n"),
            Step::Text("\u{1b}]133;D;0\u{7}"),
        ]),
    ),
    // Astral-plane emoji through a real ConPTY (#17), replacing the synthetic
    // byte list this entry started as. Every other wide character in the
    // corpus is CJK — three UTF-8 bytes and one UTF-16 code unit — which hides
    // the single most JavaScript-specific bug a client can have: `text.length`
    // counts UTF-16 code units, so one emoji counts as two and every cell
    // after it shifts left. With an astral character the two wrong readings
    // even fail differently: `text.length` over-counts the WIDE run, and the
    // character count under-counts the spacer run that carries no text at all.
    ("astral", 40, 6, Source::Vtrec("astral")),
    // `scroll-flood` is deliberately *not* exported. Its 115 chunks make a
    // megabyte of golden JSON, and what it adds — SCROLL ordering at a natural
    // viewport — the `-short` exports above already hold a TypeScript client
    // to. It earns its keep in `conformance.rs` and `chaos_resync.rs`, where
    // the replay costs nothing to commit.
];

/// Stands in for a real host, so the fixtures are byte-stable.
///
/// The decoder does not care which host sent a frame, but `SessionAddr` is not
/// optional on the wire and a random id would rewrite every file on every run.
const FIXTURE_HOST: HostId = HostId::from_bytes([0x2e; 32]);

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let only = flag(&args, "--only");
    let print: Option<usize> = flag(&args, "--print").and_then(|s| s.parse().ok());

    if args.iter().any(|a| a == "--help" || a == "-h") {
        eprintln!(
            "usage: fixture_dump [--only <recording>] [--print <frame>]\n\
             \n\
             With no arguments, rewrites every file in crates/zest-proto/fixtures.\n\
             --print dumps one frame to stdout and writes nothing."
        );
        return std::process::ExitCode::SUCCESS;
    }

    let mut coverage = Coverage::default();
    let mut written = 0usize;

    for (name, cols, rows, source) in CORPUS {
        if only.is_some_and(|o| o != *name) {
            continue;
        }
        let fixture = replay(name, *cols, *rows, source, &mut coverage);

        if let Some(n) = print {
            let frame = fixture.frames.get(n).unwrap_or_else(|| {
                panic!("{name} has {} frames; {n} is out of range", fixture.frames.len())
            });
            println!("{}", serde_json::to_string_pretty(frame).expect("a frame serializes"));
            return std::process::ExitCode::SUCCESS;
        }

        let bytes: usize = fixture.frames.iter().map(|f| f.wire.len() / 2).sum();
        println!("{name}: {} frames, {bytes} bytes of wire", fixture.frames.len());
        write(&fixtures_dir().join(format!("{name}.json")), &fixture);
        written += 1;
    }

    if written == 0 {
        eprintln!(
            "no such recording: {}\nknown: {}",
            only.unwrap_or("<none>"),
            CORPUS.iter().map(|(n, ..)| *n).collect::<Vec<_>>().join(", ")
        );
        return std::process::ExitCode::FAILURE;
    }

    // A fixture set that exercises nothing is the failure mode a golden corpus
    // must not have, and it is invisible from the outside: the files exist, the
    // suite is green, and no interesting path was ever taken. `conformance.rs`
    // guards its own inputs the same way.
    //
    // Only meaningful over the whole corpus, so a single-recording run skips it.
    if only.is_none() {
        coverage.assert_the_corpus_is_worth_replaying();
        write_bits(&fixtures_dir().join("bits.json"));
        write_client_messages(&fixtures_dir().join("client-messages.json"));
        written += 2;
    }

    println!("wrote {written} files to {}", fixtures_dir().display());
    std::process::ExitCode::SUCCESS
}

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    let i = args.iter().position(|a| a == name)?;
    args.get(i + 1).map(String::as_str)
}

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures")
}

fn write<T: Serialize>(path: &Path, value: &T) {
    let mut json = serde_json::to_string_pretty(value).expect("a fixture serializes");
    json.push('\n');
    std::fs::create_dir_all(path.parent().expect("a fixture path has a parent"))
        .unwrap_or_else(|e| panic!("create {}: {e}", path.display()));
    std::fs::write(path, json).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
}

// ---------------------------------------------------------------------------
// The corpus
// ---------------------------------------------------------------------------

/// A `.vtrec` is timestamped chunks; the timing is irrelevant here.
///
/// Lifted from `tests/conformance.rs`, including both guards: chunk boundaries
/// are where the parser was interrupted mid-sequence in the real session, and a
/// recording that parses to nothing would make every assertion downstream pass
/// vacuously.
fn chunks(name: &str) -> Vec<Vec<u8>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../zest-core/tests/corpus")
        .join(format!("{name}.vtrec"));
    let bytes = std::fs::read(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

    const MAGIC: &[u8] = b"VTREC1\n";
    assert!(
        bytes.starts_with(MAGIC),
        "{} is not a .vtrec -- without this check a mis-parse reads the header as \
         a timestamp and the whole corpus exports as garbage",
        path.display()
    );

    let mut out = Vec::new();
    let mut i = MAGIC.len();
    while i + 12 <= bytes.len() {
        // u64 timestamp, u32 length, then the payload.
        let len =
            u32::from_le_bytes([bytes[i + 8], bytes[i + 9], bytes[i + 10], bytes[i + 11]]) as usize;
        i += 12;
        if i + len > bytes.len() {
            break;
        }
        out.push(bytes[i..i + len].to_vec());
        i += len;
    }

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

/// Replay one recording, capturing every frame and what the terminal held after it.
///
/// The three participants are `conformance.rs`'s, for a reason that is not
/// symmetry: the host `Terminal` is what `expect` is read from, and `GridView`
/// and `Applier` are here to refuse to write a fixture the Rust decoders
/// themselves disagree with. That is not circular — it means each file states
/// "this is the terminal's truth, and the reference already matches it".
fn replay(
    name: &str,
    cols: usize,
    rows: usize,
    source: &Source,
    cov: &mut Coverage,
) -> Fixture {
    let addr = SessionAddr { host: FIXTURE_HOST, session: SessionId(1) };
    enum Play {
        Bytes(Vec<u8>),
        Resize(u16, u16),
    }
    let input: Vec<Play> = match source {
        Source::Vtrec(recording) => chunks(recording).into_iter().map(Play::Bytes).collect(),
        Source::Script(steps) => steps
            .iter()
            .map(|s| match s {
                Step::Text(t) => Play::Bytes(t.as_bytes().to_vec()),
                Step::Resize(c, r) => Play::Resize(*c, *r),
            })
            .collect(),
    };

    let mut term = Terminal::new(cols, rows, 2000);
    let mut enc = Encoder::new();
    let mut view = GridView::new();
    let mut client = Terminal::new(cols, rows, 2000);
    let mut applier = Applier::new();

    let mut frames = Vec::new();

    let k = enc.keyframe(term.grid(), cursor(&term), term.modes(), term.title(), term.blocks());
    view.apply_keyframe(&k);
    applier.apply_keyframe(&mut client, &k, term.seq());
    cov.see_attrs(&k.attrs);

    frames.push(keyframe_frame(addr, &term, &k));

    for (step, play) in input.iter().enumerate() {
        let chunk = match play {
            Play::Bytes(bytes) => bytes,
            Play::Resize(cols, rows) => {
                // A resize reflows the host's grid, renumbering every line id,
                // and the keyframe below is a subscriber's only account of it —
                // the wire has no resize op. Through all three participants
                // like every other frame, so the file still states "the
                // reference already matches the terminal's truth".
                cov.width_changes += usize::from(term.grid().cols() != usize::from(*cols));
                term.resize(usize::from(*cols), usize::from(*rows));
                let k =
                    enc.keyframe(term.grid(), cursor(&term), term.modes(), term.title(), term.blocks());
                view.apply_keyframe(&k);
                applier.apply_keyframe(&mut client, &k, term.seq());
                cov.see_attrs(&k.attrs);

                let frame = keyframe_frame(addr, &term, &k);
                agrees(&view, &frame.expect, name, step);
                frames.push(frame);
                continue;
            }
        };
        term.advance(chunk);
        let d = enc.delta(term.grid(), cursor(&term), term.modes(), term.title(), term.blocks());

        assert!(d.scrolls_come_first(), "{name} step {step}: {:?}", d.ops);
        assert!(d.screen_switch_comes_first(), "{name} step {step}: {:?}", d.ops);
        cov.see_delta(&d);

        let base = applier.applied();
        view.apply_delta(&d);
        assert_eq!(
            applier.apply_delta(&mut client, &d, base, term.seq()),
            Applied::Ok,
            "{name} step {step}: the applier could not apply a delta the encoder produced"
        );

        let e = expect(&term);
        cov.blocks_expected += usize::from(!e.blocks.is_empty());
        // Refuse to write a fixture the Rust reference does not already satisfy.
        // A golden file generated from a broken encoder is worse than none: it
        // pins the bug and then fails the client that decoded it correctly.
        agrees(&view, &e, name, step);

        frames.push(Frame {
            kind: "update",
            base: Some(base),
            seq: term.seq(),
            wire: hex(&zest_proto::frame::encode(&HostMessage::Update {
                session: addr,
                base: Seq(base),
                seq: Seq(term.seq()),
                delta: d,
            })
            .expect("an update frames")),
            expect: e,
        });
    }

    Fixture {
        schema: FIXTURE_SCHEMA,
        protocol: PROTOCOL_VERSION,
        recording: name.to_string(),
        source: match source {
            Source::Vtrec(_) => "vtrec",
            Source::Script(_) => "synthetic",
        },
        cols: u16::try_from(cols).unwrap_or(u16::MAX),
        rows: u16::try_from(rows).unwrap_or(u16::MAX),
        frames,
    }
}

/// One keyframe as a fixture frame.
///
/// The daemon flattens `encode::Keyframe` into the wire message rather than
/// sending it, so the fixture carries a real `HostMessage` and not an envelope
/// invented here. Mirrors `zest-daemon/src/server.rs`.
fn keyframe_frame(addr: SessionAddr, term: &Terminal, k: &zest_proto::encode::Keyframe) -> Frame {
    Frame {
        kind: "keyframe",
        base: None,
        seq: term.seq(),
        wire: hex(&zest_proto::frame::encode(&HostMessage::Keyframe {
            session: addr,
            seq: Seq(term.seq()),
            cols: k.cols,
            rows: k.rows,
            rows_data: k.rows_data.clone(),
            attrs: k.attrs.clone(),
            cursor: k.cursor,
            modes: k.modes.bits(),
            blocks: k.blocks.clone(),
            blocks_from: k.blocks_from,
            title: k.title.clone(),
            history_clears: k.history_clears,
        })
        .expect("a keyframe frames")),
        expect: expect(term),
    }
}

/// What the host's grid holds, cell by cell.
fn expect(term: &Terminal) -> Expect {
    let g = term.grid();
    let mut rows = Vec::with_capacity(g.rows());

    for r in 0..g.rows() {
        let row = g.row(r);
        let cells = row.cells();

        let text: String = cells.iter().map(|c| c.ch).collect();

        // Run-length encoded purely to keep the file reviewable; the client
        // expands it back to one entry per cell before comparing, so the
        // grouping here carries no meaning of its own.
        let mut styles: Vec<Style> = Vec::new();
        for c in cells {
            match styles.last_mut() {
                Some(last) if last.1 == c.fg && last.2 == c.bg && last.3 == c.flags.bits() => {
                    last.0 += 1;
                }
                _ => styles.push(Style(1, c.fg, c.bg, c.flags.bits())),
            }
        }

        let marks: Vec<Mark> = cells
            .iter()
            .enumerate()
            .filter_map(|(col, c)| {
                let extra = row.extra(c)?;
                if extra.zerowidth.is_empty() {
                    return None;
                }
                Some(Mark {
                    col: u16::try_from(col).unwrap_or(u16::MAX),
                    marks: extra.zerowidth.iter().collect(),
                })
            })
            .collect();

        rows.push(RowExpect {
            line: i64::try_from(row.id).unwrap_or(i64::MAX),
            wrapped: row.wrapped(),
            text,
            styles,
            marks,
        });
    }

    Expect {
        cursor: cursor(term),
        modes: term.modes().bits(),
        alt_screen: term.modes().contains(Modes::ALT_SCREEN),
        title: term.title().to_string(),
        rows,
        // Read off the terminal's own block index, like everything else here.
        // `from_block` is a plain projection, not the encoder's diffing — so
        // this is still "Reference 2", not the encoder grading its own work.
        blocks: term.blocks().blocks().iter().map(BlockPayload::from_block).collect(),
    }
}

/// The Rust decoder must already read this frame the way the fixture states it.
///
/// Text only, and trailing blanks trimmed: that is the projection `GridView`
/// can be compared on without reimplementing the padding rule here. The strict
/// cell-for-cell check is `conformance.rs`'s job and runs on every `cargo test`.
fn agrees(view: &GridView, e: &Expect, name: &str, step: usize) {
    assert_eq!(
        view.rows().len(),
        e.rows.len(),
        "{name} step {step}: the reference decoder holds a different number of rows"
    );
    for (r, (got, want)) in view.rows().iter().zip(e.rows.iter()).enumerate() {
        let flat: String = got
            .runs
            .iter()
            .flat_map(|run| {
                let mut chars = run.text.chars();
                (0..run.cells).map(move |_| chars.next().unwrap_or(' ')).collect::<Vec<_>>()
            })
            .collect();
        assert_eq!(
            flat.trim_end(),
            want.text.trim_end(),
            "{name} step {step} row {r}: refusing to write a fixture the reference \
             decoder does not agree with"
        );
        assert_eq!(
            got.line, want.line,
            "{name} step {step} row {r}: line ids diverged"
        );
    }
    assert_eq!(
        view.blocks, e.blocks,
        "{name} step {step}: the reference decoder's blocks disagree with the terminal's"
    );
}

fn hex(bytes: &[u8]) -> String {
    // `crate::hex` is private to zest-proto and this is an example, not a member
    // of it. Sixteen lines beats a dependency in a workspace that has none for
    // this.
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(char::from_digit(u32::from(b >> 4), 16).expect("a nibble is a hex digit"));
        s.push(char::from_digit(u32::from(b & 0x0f), 16).expect("a nibble is a hex digit"));
    }
    s
}

// ---------------------------------------------------------------------------
// The bit tables
// ---------------------------------------------------------------------------

/// Write the flag and mode bit values a client has to hard-code.
///
/// The recordings compare `flags` and `modes` as raw integers, so a client whose
/// table maps `ITALIC` to the wrong bit passes every one of them. Nothing else
/// in the fixture set can catch that, which is the entire reason this file
/// exists.
fn write_bits(path: &Path) {
    #[derive(Serialize)]
    struct Bits {
        schema: u32,
        // Maps, and so alphabetical rather than in bit order: a client compares
        // name to value, and sorted keys make a renamed or removed flag a
        // one-line diff instead of a reflowed list.
        cell_flags: BTreeMap<String, u16>,
        modes: BTreeMap<String, u32>,
    }

    write(
        path,
        &Bits {
            schema: FIXTURE_SCHEMA,
            cell_flags: CellFlags::all()
                .iter_names()
                .map(|(n, f)| (n.to_string(), f.bits()))
                .collect(),
            modes: Modes::all().iter_names().map(|(n, m)| (n.to_string(), m.bits())).collect(),
        },
    );
}

// ---------------------------------------------------------------------------
// Coverage
// ---------------------------------------------------------------------------

/// What the corpus actually exercised, so "green" cannot mean "empty".
#[derive(Default)]
struct Coverage {
    scrolls: usize,
    sb_pushes: usize,
    screen_switches: usize,
    titles: usize,
    coloured: usize,
    wide: usize,
    astral: usize,
    marks: usize,
    blocks: usize,
    /// Frames whose *expectation* carries blocks — distinct from `blocks`,
    /// which counts wire payloads: the wire proves decode, the expectation
    /// proves application.
    blocks_expected: usize,
    /// Mid-stream keyframes whose `cols` differ from the grid's before them.
    width_changes: usize,
    ops: BTreeSet<&'static str>,
}

impl Coverage {
    fn see_attrs(&mut self, attrs: &[zest_proto::AttrDef]) {
        for a in attrs {
            if a.bg != Color::Default || a.flags != CellFlags::empty() {
                self.coloured += 1;
            }
        }
    }

    fn see_delta(&mut self, d: &Delta) {
        self.see_attrs(&d.attrs);
        // A field rather than an op, so it is invisible to the `ops` set below
        // and needs counting of its own.
        self.blocks += d.blocks.len();
        for op in &d.ops {
            match op {
                DeltaOp::Scroll { .. } => {
                    self.scrolls += 1;
                    self.ops.insert("scroll");
                }
                DeltaOp::SbPush { .. } => {
                    self.sb_pushes += 1;
                    self.ops.insert("sb_push");
                }
                DeltaOp::AltScreen { .. } => {
                    self.screen_switches += 1;
                    self.ops.insert("alt_screen");
                }
                DeltaOp::Title { .. } => {
                    self.titles += 1;
                    self.ops.insert("title");
                }
                DeltaOp::Row { payload, .. } => {
                    self.ops.insert("row");
                    for run in &payload.runs {
                        // A run whose cell count exceeds its character count is
                        // a double-width character's spacer, and it is the one
                        // thing a client cannot get right by counting glyphs.
                        if usize::from(run.cells) > run.text.chars().count() {
                            self.wide += 1;
                        }
                        // Anything past the BMP is a surrogate pair in
                        // JavaScript, which is where `text.length` and a code
                        // point count stop agreeing.
                        self.astral += run.text.chars().filter(|c| *c as u32 > 0xFFFF).count();
                        self.marks += run.marks.len();
                    }
                }
                DeltaOp::Cursor { .. } => {
                    self.ops.insert("cursor");
                }
                DeltaOp::Modes { .. } => {
                    self.ops.insert("modes");
                }
                DeltaOp::Erase { .. } => {
                    self.ops.insert("erase");
                }
            }
        }
    }

    fn assert_the_corpus_is_worth_replaying(&self) {
        println!(
            "coverage: {} scrolls, {} sb_push, {} screen switches, {} titles, \
             {} styled attrs, {} wide runs, {} marks, {} block updates, {} width changes",
            self.scrolls,
            self.sb_pushes,
            self.screen_switches,
            self.titles,
            self.coloured,
            self.wide,
            self.marks,
            self.blocks,
            self.width_changes
        );
        println!("ops seen: {:?}", self.ops);

        // Each of these is a decoder rule with no other check on it. A corpus
        // that stopped exercising one would leave that rule untested while
        // every fixture still passed.
        assert!(self.scrolls > 0, "no SCROLL op -- the ordering rule is untested");
        assert!(self.sb_pushes > 0, "no SB_PUSH op -- scrollback is untested");
        assert!(self.screen_switches > 0, "no ALT_SCREEN op -- the switch is untested");
        assert!(self.coloured > 0, "every attribute was the default -- colour is untested");
        assert!(
            self.wide > 0,
            "no run has more cells than characters -- the width rule is untested, \
             which is the one rule a client is most likely to get wrong"
        );
        assert!(
            self.astral > 0,
            "nothing past the BMP -- a JavaScript client counting UTF-16 code units \
             instead of code points would pass every fixture, and that is the one \
             bug this corpus exists to make impossible"
        );
        assert!(
            self.marks > 0,
            "no combining marks -- the side table that 4b3152e added is untested"
        );
        assert!(
            self.blocks > 0,
            "no command blocks -- `Delta::blocks` is a field rather than an op, so a \
             client that ignored it entirely would pass every fixture here"
        );
        assert!(
            self.blocks_expected > 0,
            "no frame *expects* blocks -- the wire carried them but the expectations \
             would hold no client to applying them, which is the half that matters"
        );
        assert!(
            self.width_changes > 0,
            "no mid-stream width change -- a reflow renumbers every line id, and a \
             client that keeps state banked under the old numbering would pass every \
             fixture here, which is exactly how #139 stayed latent"
        );
    }
}

// ---------------------------------------------------------------------------
// The client-message goldens
// ---------------------------------------------------------------------------

/// One canonical encoding of every `ClientMessage` variant, framed, as hex.
///
/// The TypeScript encoder asserts **byte equality** against these — not
/// round-trip acceptance, which any two self-consistent encoders pass. The
/// values are chosen to pin the choices that could silently drift: a `cols`
/// that needs the u16 form, a `seq` past u32, a negative `from_line`, input
/// bytes past the fixint range, and the `t` tag leading every map.
fn write_client_messages(path: &Path) {
    #[derive(Serialize)]
    struct Golden {
        schema: u32,
        protocol: u16,
        messages: Vec<Entry>,
    }
    #[derive(Serialize)]
    struct Entry {
        name: &'static str,
        wire: String,
    }

    let addr = SessionAddr { host: FIXTURE_HOST, session: SessionId(7) };
    let messages: Vec<(&'static str, ClientMessage)> = vec![
        (
            "hello",
            ClientMessage::Hello {
                version: PROTOCOL_VERSION,
                client: ClientId::from_bytes([0xab; 32]),
                label: "golden".into(),
                nonce: Nonce32::from_bytes([0x5c; 32]),
                dh: zest_proto::Pub32::from_bytes([0x2d; 32]),
                watch_sessions: true,
                watch_pairings: false,
                // `true` rather than the default, so the golden actually
                // exercises the field #262 added: a flag that is always false
                // in the one canonical encoding proves only that it can be
                // omitted, which is not the property this fixture is for.
                watch_hosts: true,
                // …and the same for #383's, for the same reason.
                watch_signals: true,
            },
        ),
        ("auth", ClientMessage::Auth { signature: Sig64::from_bytes([0xef; 64]) }),
        (
            "pairing_decision",
            ClientMessage::PairingDecision {
                client: ClientId::from_bytes([0xab; 32]),
                approve: false,
            },
        ),
        (
            "enroll",
            ClientMessage::Enroll { code: "GOLDCODE".into() },
        ),
        ("request_keyframe", ClientMessage::RequestKeyframe { session: addr }),
        ("list_sessions", ClientMessage::ListSessions),
        (
            "create_session",
            ClientMessage::CreateSession {
                command: "htop".into(),
                cwd: "/tmp".into(),
                // Past u8, so the u16 encoding is pinned.
                cols: 300,
                rows: 80,
                env: Vec::new(),
            },
        ),
        ("attach", ClientMessage::Attach { session: addr, cols: 120, rows: 40, observe: false }),
        // Both spellings, because the flag is the whole difference between a
        // client that shrinks somebody's window and one that does not (#274),
        // and a second implementation that dropped it would encode a valid
        // `attach` that silently votes.
        (
            "attach_observing",
            ClientMessage::Attach { session: addr, cols: 120, rows: 40, observe: true },
        ),
        ("detach", ClientMessage::Detach { session: addr }),
        (
            "input",
            ClientMessage::Input {
                session: addr,
                // Arrow-up, then bytes past the fixint range: the seq-of-uints
                // form (never `bin`) is the exact thing this pins.
                bytes: vec![0x1b, 0x5b, 0x41, 0x80, 0xff],
            },
        ),
        ("resize", ClientMessage::Resize { session: addr, cols: 80, rows: 24 }),
        // Past u32, so the eight-byte form is pinned.
        ("ack", ClientMessage::Ack { session: addr, seq: Seq(4_294_967_296) }),
        (
            "request_scrollback",
            ClientMessage::RequestScrollback { session: addr, from_line: -1200, count: 500 },
        ),
        ("close_session", ClientMessage::CloseSession { session: addr }),
    ];

    write(
        path,
        &Golden {
            schema: FIXTURE_SCHEMA,
            protocol: PROTOCOL_VERSION,
            messages: messages
                .into_iter()
                .map(|(name, m)| Entry {
                    name,
                    wire: hex(&zest_proto::frame::encode(&m).expect("a client message frames")),
                })
                .collect(),
        },
    );
}

// ---------------------------------------------------------------------------
// The file
// ---------------------------------------------------------------------------

#[derive(Serialize)]
struct Fixture {
    schema: u32,
    /// `PROTOCOL_VERSION`, so a bump rewrites every file and the diff is
    /// impossible to miss in review.
    protocol: u16,
    recording: String,
    /// `vtrec` for a real recording, `synthetic` for input written to reach a
    /// path no recording does. Stated so nobody mistakes the second kind for
    /// evidence about what real programs emit.
    source: &'static str,
    /// The *initial* size. `width-change` shrinks mid-stream via a keyframe
    /// whose `cols` is smaller — never larger, because a consumer that pads
    /// rows to this width would truncate a grid that outgrew it.
    cols: u16,
    rows: u16,
    frames: Vec<Frame>,
}

#[derive(Serialize)]
struct Frame {
    kind: &'static str,
    /// The sequence this update builds on. Absent on a keyframe, which builds
    /// on nothing.
    #[serde(skip_serializing_if = "Option::is_none")]
    base: Option<u64>,
    seq: u64,
    /// The complete framed message, length prefix included, as hex.
    wire: String,
    expect: Expect,
}

#[derive(Serialize)]
struct Expect {
    cursor: CursorState,
    modes: u32,
    alt_screen: bool,
    title: String,
    rows: Vec<RowExpect>,
    /// The host's block index after this frame, in the wire shape — so a
    /// client compares its applied blocks against the terminal's truth, not
    /// against what happened to ride this frame.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    blocks: Vec<BlockPayload>,
}

#[derive(Serialize)]
struct RowExpect {
    line: i64,
    wrapped: bool,
    /// One code point per cell, including the blank a wide character's spacer
    /// holds. Exactly `cols` of them.
    text: String,
    styles: Vec<Style>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    marks: Vec<Mark>,
}

/// `[cells, fg, bg, flags]`.
#[derive(Serialize)]
struct Style(u16, Color, Color, u16);

#[derive(Serialize)]
struct Mark {
    col: u16,
    marks: String,
}

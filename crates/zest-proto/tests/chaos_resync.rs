//! Dropping messages at random, and landing in the same place anyway.
//!
//! The conformance spine proves the applier is right when nothing goes wrong.
//! This proves it recovers when something does, which is the case the whole
//! delta design exists for — ADR-004 rejected shipping raw PTY bytes precisely
//! because *"resync is unsolvable"* on a reconnecting link.
//!
//! Each iteration picks a recording and a point in it, drops one delta, and
//! then — before the keyframe that repairs things — delivers **one stale
//! update whose `base` no longer matches**. That second part is the half that
//! matters: a client which merely ignores a gap is not the same as one which
//! recognises that what it is being sent no longer applies to what it holds.
//! Exercising it here is the difference between the rule being tested and the
//! rule being a comment.
//!
//! # Determinism
//!
//! A seeded SplitMix64 written inline, because the workspace has no `rand` and
//! a random test that cannot be replayed is a random test that gets deleted the
//! first time it fails in CI. The seed is printed on failure and can be pinned
//! with `ZEST_CHAOS_SEED`.
//!
//! # All ten thousand, every run
//!
//! The full 10,000 disconnect points take under a second, so they are **not**
//! behind `--ignored`. A test CI does not run protects nothing, which is the
//! same reason `--startup-probe` is a failing command rather than a `#[test]`
//! that gets silently skipped.

use zest_core::{Modes, Terminal};
use zest_proto::apply::{Applied, Applier};
use zest_proto::delta::CursorState;
use zest_proto::encode::Encoder;

const CORPUS: &[&str] = &["basic-echo", "dir-colors", "git-log", "unicode-wide", "vim-macos"];

/// Deterministic, replayable, and about eight lines. Quality is irrelevant
/// here — the requirement is a reproducible spread of drop points.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            usize::try_from(self.next() % n as u64).unwrap_or(0)
        }
    }
}

fn corpus_path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../zest-core/tests/corpus")
        .join(format!("{name}.vtrec"))
}

fn chunks(name: &str) -> Vec<Vec<u8>> {
    let bytes = std::fs::read(corpus_path(name)).expect("read recording");
    const MAGIC: &[u8] = b"VTREC1\n";
    assert!(bytes.starts_with(MAGIC), "{name} is not a .vtrec");

    let mut out = Vec::new();
    let mut i = MAGIC.len();
    while i + 12 <= bytes.len() {
        let len =
            u32::from_le_bytes([bytes[i + 8], bytes[i + 9], bytes[i + 10], bytes[i + 11]]) as usize;
        i += 12;
        if i + len > bytes.len() {
            break;
        }
        out.push(bytes[i..i + len].to_vec());
        i += len;
    }
    assert!(!out.is_empty(), "{name} produced no chunks");
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

fn cells_agree(host: &Terminal, client: &Terminal) -> Result<(), String> {
    let (hg, cg) = (host.grid(), client.grid());
    if hg.rows() != cg.rows() || hg.cols() != cg.cols() {
        return Err(format!(
            "size diverged: host {}x{} client {}x{}",
            hg.cols(),
            hg.rows(),
            cg.cols(),
            cg.rows()
        ));
    }
    for r in 0..hg.rows() {
        for c in 0..hg.cols() {
            let (Some(h), Some(k)) = (hg.cell(r, c), cg.cell(r, c)) else { continue };
            if h.ch != k.ch || h.bg != k.bg || h.flags != k.flags {
                return Err(format!(
                    "row {r} col {c}: host {:?}/{:?} client {:?}/{:?}",
                    h.ch, h.bg, k.ch, k.bg
                ));
            }
        }
    }
    Ok(())
}

/// Replay one recording, dropping the delta at `drop_at`.
///
/// Returns an error describing the first frame where the two disagree.
fn run_one(name: &str, chunks: &[Vec<u8>], drop_at: usize) -> Result<(), String> {
    let mut host = Terminal::new(80, 24, 2000);
    let mut client = Terminal::new(80, 24, 2000);
    let mut enc = Encoder::new();
    let mut app = Applier::new();

    let k = enc.keyframe(host.grid(), cursor(&host), host.modes(), "");
    app.apply_keyframe(&mut client, &k, 0);

    // What the *host* believes the client holds, and therefore what it names as
    // `base` on the next update. It advances whether or not the frame arrives —
    // the host has no way to know a write was lost, which is exactly why the
    // base is on the wire at all.
    let mut host_base = 0u64;
    let mut dropped = false;

    for (step, chunk) in chunks.iter().enumerate() {
        host.advance(chunk);
        let seq = host.seq();
        let delta = enc.delta(host.grid(), cursor(&host), host.modes(), "");

        if step == drop_at {
            // The frame never arrives. The encoder's shadow has already moved
            // on, which is exactly the situation on a real dropped write.
            //
            // `dropped` only when the frame carried something: a chunk that
            // moved no sequence -- a mode set the client already had, a cursor
            // save -- produces a delta whose loss costs nothing, and demanding
            // a resync for it would be asserting the wrong rule.
            dropped = seq != host_base;
            host_base = seq;
            continue;
        }

        let outcome = app.apply_delta(&mut client, &delta, host_base, seq);

        if dropped {
            // The first update after the drop is built against a state this
            // client never received, so it must be refused. Asserting it here
            // means the rule is exercised on every iteration rather than once
            // in a hand-built fixture.
            if outcome != Applied::NeedsKeyframe {
                return Err(format!(
                    "{name} drop@{drop_at} step {step}: an update built on a state \
                     the client never received was accepted"
                ));
            }
            dropped = false;
        }

        if outcome == Applied::NeedsKeyframe {
            // Recovery: a full state, which is what `RequestKeyframe` fetches.
            let k = enc.keyframe(host.grid(), cursor(&host), host.modes(), "");
            app.apply_keyframe(&mut client, &k, seq);
        }
        host_base = seq;

        // Converged at this step, and at every step after the drop -- not just
        // at the end, where two compensating errors would hide each other.
        cells_agree(&host, &client)
            .map_err(|e| format!("{name} drop@{drop_at} step {step}: {e}"))?;
        if client.seq() != host.seq() {
            return Err(format!(
                "{name} drop@{drop_at} step {step}: client would ack {} not {}",
                client.seq(),
                host.seq()
            ));
        }
    }
    Ok(())
}

fn chaos(iterations: usize) {
    let seed: u64 = std::env::var("ZEST_CHAOS_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0x5EED_1234_ABCD_0001);
    chaos_with(seed, iterations);
}

fn chaos_with(seed: u64, iterations: usize) {
    let mut rng = Rng(seed);

    let loaded: Vec<(&str, Vec<Vec<u8>>)> =
        CORPUS.iter().map(|n| (*n, chunks(n))).collect();

    for i in 0..iterations {
        let (name, cs) = &loaded[rng.below(loaded.len())];
        let drop_at = rng.below(cs.len());
        if let Err(e) = run_one(name, cs, drop_at) {
            panic!(
                "iteration {i} diverged: {e}\n\
                 replay with ZEST_CHAOS_SEED={seed}"
            );
        }
    }
}

/// The run the roadmap asks for: 10,000 random disconnect points.
#[test]
fn ten_thousand_disconnect_points() {
    chaos(10_000);
}

/// The same, from three unrelated seeds.
///
/// One seed proves the applier survives one particular walk through the corpus.
/// Three make it much harder for a passing run to be a coincidence of which
/// frames happened to be chosen.
#[test]
fn the_result_does_not_depend_on_the_seed() {
    for seed in [1u64, 0xDEAD_BEEF, 0x0123_4567_89AB_CDEF] {
        chaos_with(seed, 500);
    }
}

#[test]
fn a_stale_update_is_never_applied() {
    // Direct, rather than as a side effect of the loop above: if a client ever
    // applies an update whose base it does not hold, the grid is corrupt from
    // that point on with nothing to indicate it.
    let mut host = Terminal::new(20, 3, 100);
    let mut client = Terminal::new(20, 3, 100);
    let mut enc = Encoder::new();
    let mut app = Applier::new();

    host.advance(b"REAL CONTENT");
    let k = enc.keyframe(host.grid(), cursor(&host), host.modes(), "");
    app.apply_keyframe(&mut client, &k, host.seq());

    let before = client.screen_text();
    let applied_seq = client.seq();

    // An update naming a base the client does not hold.
    let stale = zest_proto::Delta {
        attrs: vec![zest_proto::AttrDef {
            id: zest_proto::AttrId(0),
            fg: zest_core::Color::Default,
            bg: zest_core::Color::Default,
            flags: zest_core::CellFlags::empty(),
        }],
        ops: vec![zest_proto::DeltaOp::Row {
            row: 0,
            payload: zest_proto::RowPayload {
                line: 0,
                runs: vec![zest_proto::Run {
                    attr: zest_proto::AttrId(0),
                    cells: 5,
                    text: "STALE".into(),
                    marks: Vec::new(),
                }],
                wrapped: false,
            },
        }],
    };

    let stale_base = applied_seq + 99;
    assert_ne!(stale_base, applied_seq, "the fixture must actually be stale");

    let outcome = app.apply_delta(&mut client, &stale, stale_base, stale_base + 1);
    assert_eq!(outcome, Applied::NeedsKeyframe, "a stale update must be refused");
    assert_eq!(client.screen_text(), before, "a refused update must change nothing at all");
    assert!(
        !client.screen_text().contains("STALE"),
        "the refused content reached the grid: {}",
        client.screen_text()
    );
    assert_eq!(
        client.seq(),
        applied_seq,
        "a refused update must not move the sequence, or the client would \
         acknowledge a state it does not hold"
    );

    // And the same delta, correctly based, does apply -- otherwise this test
    // would pass against an applier that refuses everything.
    let outcome = app.apply_delta(&mut client, &stale, applied_seq, applied_seq + 1);
    assert_eq!(outcome, Applied::Ok);
    assert!(client.screen_text().contains("STALE"), "a well-based update must apply");
}

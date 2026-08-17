//! The replica, driven by real recorded wire frames. No daemon, no pty, no shell.
//!
//! `crates/zest-proto/fixtures/*.json` carry the **complete framed bytes** of a
//! real recorded session as the host actually sent them, plus what a correct
//! client should hold afterwards. Replaying them here means the thing an agent
//! reads through `screen`, `blocks` and `output` is checked against a genuine
//! zsh session on every CI platform, with nothing to spawn and nothing to time
//! out — which is what makes the shell-shaped half of this crate testable at
//! all. (`blocks-zsh` is the one with OSC 133; `blocks-pwsh.vtrec` is its
//! Windows twin in the corpus.)
//!
//! These go through `FrameReader` and `frame::decode` rather than being handed
//! pre-parsed structures, so a mistake in how this crate reads the wire fails
//! here rather than only against a live daemon.

use zest_mcp::run::{self, Anchor, Progress};
use zest_mcp::Replica;
use zest_proto::{frame, BlockPayload, BlockState, FrameReader, HostMessage, SessionAddr, SessionId, HostId};

/// One recorded session, as the host sent it.
struct Fixture {
    cols: u16,
    rows: u16,
    frames: Vec<Vec<u8>>,
}

fn load(name: &str) -> Fixture {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../zest-proto/fixtures")
        .join(format!("{name}.json"));
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let v: serde_json::Value = serde_json::from_str(&text).expect("the fixture is JSON");

    assert_eq!(
        v["protocol"].as_u64(),
        Some(u64::from(zest_proto::PROTOCOL_VERSION)),
        "{name}.json was written for a different protocol; regenerate with `cargo xtask fixtures`"
    );

    let frames = v["frames"]
        .as_array()
        .expect("frames is an array")
        .iter()
        .map(|f| {
            let hex = f["wire"].as_str().expect("a frame carries hex wire bytes");
            (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).expect("hex"))
                .collect()
        })
        .collect();

    Fixture {
        cols: v["cols"].as_u64().expect("cols") as u16,
        rows: v["rows"].as_u64().expect("rows") as u16,
        frames,
    }
}

/// Replay every frame into a replica, exactly as the streaming reader will.
fn replay(name: &str) -> Replica {
    replay_inner(name, |_| ()).0
}

/// Replay, sampling something after every frame.
///
/// For properties that are about a *transition* rather than the end state --
/// entering and leaving the alternate screen is the one that matters, because
/// a replica that latched on would refuse commands for ever afterwards.
fn replay_watching<T>(name: &str, sample: impl Fn(&Replica) -> T) -> Vec<T> {
    replay_inner(name, sample).1
}

fn replay_inner<T>(name: &str, sample: impl Fn(&Replica) -> T) -> (Replica, Vec<T>) {
    let fx = load(name);
    let addr = SessionAddr { host: HostId::from_bytes([0x2e; 32]), session: SessionId(1) };

    let mut reader = FrameReader::new();
    let mut replica: Option<Replica> = None;
    let mut samples = Vec::new();

    for raw in &fx.frames {
        reader.feed(raw);
        while let Some(body) = reader.next_frame().expect("the fixture frames are well formed") {
            match frame::decode::<HostMessage>(&body).expect("a fixture frame decodes") {
                HostMessage::Keyframe {
                    seq,
                    cols,
                    rows,
                    rows_data,
                    attrs,
                    cursor,
                    modes,
                    blocks,
                    blocks_from,
                    title,
                    ..
                } => {
                    let k = zest_proto::Keyframe {
                        cols,
                        rows,
                        rows_data,
                        attrs,
                        cursor,
                        modes: zest_core::Modes::from_bits_truncate(modes),
                        blocks,
                        blocks_from,
                        title: title.clone(),
                    };
                    match replica.as_mut() {
                        Some(r) => r.reset(&k, seq.0),
                        None => replica = Some(Replica::new(addr, &k, seq.0)),
                    }
                    if let Some(r) = replica.as_mut() {
                        r.set_title(title);
                    }
                }
                HostMessage::Update { base, seq, delta, .. } => {
                    let r = replica.as_mut().expect("an update before any keyframe");
                    assert!(
                        r.apply(&delta, base.0, seq.0),
                        "{name}: a recorded delta was refused, so this replica and the \
                         host have diverged -- the fixtures are replayed in order and \
                         every base matches by construction"
                    );
                }
                other => panic!("{name}: unexpected frame {other:?}"),
            }
            if let Some(r) = replica.as_ref() {
                samples.push(sample(r));
            }
        }
    }

    let r = replica.expect("the fixture carried no keyframe");
    assert_eq!(
        r.size(),
        (usize::from(fx.cols), usize::from(fx.rows)),
        "{name}: the replica ended at a different shape than the recording"
    );
    (r, samples)
}

#[test]
fn a_recorded_zsh_session_reads_back_as_the_commands_that_ran() {
    // The whole pitch in one assertion: an agent asking "what has happened in
    // this session" gets commands and exit codes, not a byte stream to parse.
    let r = replay("blocks-zsh");
    let blocks = r.blocks();

    assert!(!blocks.is_empty(), "a session recorded with OSC 133 must produce blocks");

    let commands: Vec<&str> = blocks.iter().map(|b| b.command.as_str()).collect();
    assert!(
        commands.iter().any(|c| c.contains("echo hello")),
        "the recording runs `echo hello`; the replica's blocks say {commands:?}"
    );

    // The exit code is the shell's OSC 133;D report -- which is exactly why
    // every tool result labels where an exit code came from. This asserts it
    // survives the wire, not that it is trustworthy.
    let failed = blocks.iter().find(|b| b.command.contains("false"));
    if let Some(b) = failed {
        assert!(
            matches!(b.state, zest_proto::BlockState::Finished { exit_code: Some(c) } if c != 0),
            "`false` must come back as a non-zero exit, not as unknown: {:?}",
            b.state
        );
    }
}

#[test]
fn a_finished_block_hands_back_the_rows_it_printed() {
    // `output` is the only bulk-text tool, and it is scoped to one block by its
    // line ids. If this returned the whole screen the token claim would be
    // false in exactly the case that matters -- a long build.
    let r = replay("blocks-zsh");
    let blocks = r.blocks();

    let with_output = blocks
        .iter()
        .find(|b| b.output_line.is_some() && b.end_line.is_some())
        .expect("the recording contains at least one finished command");

    let rows = r
        .block_rows(with_output.id)
        .unwrap_or_else(|| panic!("block {} is in the list but has no rows", with_output.id));

    assert!(
        !rows.is_empty(),
        "a finished block whose lines are still held must hand back its rows; \
         an empty vec means the caller has to ask the host for scrollback"
    );
    assert!(
        !rows.iter().any(|line| line.contains(&with_output.command) && line.contains('$')),
        "block_rows must return the command's *output*, never the prompt line it \
         was typed on"
    );
}

#[test]
fn a_command_that_printed_nothing_answers_empty_rather_than_echoing_itself() {
    // The recorded `false` is exactly this case, and the shape is worth knowing:
    // it comes back with `output_line: 6` and `end_line: 5` -- an empty range,
    // because `133;C` fires before the shell echoes the newline and `133;D`
    // after the trailing one, and the parser corrects both. So "printed
    // nothing" is an inverted range here, not an absent `output_line`.
    //
    // Either way the answer must be no rows. Anchoring on `prompt_line` when
    // there is nothing to show hands back the command line itself, and an agent
    // reading that sees its own command echoed as the command's output --
    // indistinguishable from a program that really did print it.
    let r = replay("blocks-zsh");

    let silent: Vec<_> = r
        .blocks()
        .into_iter()
        .filter(|b| match (b.output_line, b.end_line) {
            (None, _) => true,
            (Some(out), Some(end)) => out > end,
            (Some(_), None) => false,
        })
        .collect();

    assert!(
        !silent.is_empty(),
        "the recording is expected to contain a command that printed nothing \
         (`false`); without one this test proves nothing"
    );

    for b in silent {
        assert_eq!(
            r.block_rows(b.id),
            Some(Vec::new()),
            "block {} printed nothing, so its command ({:?}) must not come back \
             as its own output",
            b.id,
            b.command
        );
    }
}

#[test]
fn an_unknown_block_is_none_rather_than_an_empty_answer() {
    // The two have to be distinguishable: `None` is "no such block", an empty
    // vec is "the block exists and its rows have scrolled out of this replica".
    // Collapsing them would make an agent retry a block that will never exist.
    let r = replay("blocks-zsh");
    assert!(r.block_rows(u32::MAX).is_none(), "an id no block has must not answer with rows");
}

#[test]
fn the_screen_is_text_with_no_trailing_blank_rows() {
    // What `screen` returns. A 30-row grid showing a few lines of shell is
    // mostly nothing, and the nothing is paid for on every call.
    let r = replay("blocks-zsh");
    let text = r.screen_text();

    assert!(!text.is_empty(), "a replayed session must have visible text");
    assert!(
        !text.ends_with('\n') && !text.ends_with(' '),
        "trailing blank rows and trailing spaces are pure token cost: {:?}",
        &text[text.len().saturating_sub(40)..]
    );
    assert!(
        text.lines().count() <= 30,
        "the screen must not exceed the grid it came from"
    );
}

#[test]
fn every_recording_replays_without_the_replica_diverging() {
    // The applier is the thing this crate is Rust for. If any recording is
    // refused, the grid an agent reads is not the grid the shell drew -- and
    // that failure is silent everywhere else.
    for name in ["basic-echo", "dir-colors", "git-log", "unicode-wide", "blocks-zsh"] {
        let r = replay(name);
        assert!(
            r.size().0 > 0 && r.size().1 > 0,
            "{name}: replayed to an empty grid"
        );
    }
}

#[test]
fn a_vim_session_reports_entering_and_leaving_the_alternate_screen() {
    // `run` refuses on alt screen rather than typing into somebody's editor,
    // so this flag being right is a precondition for it -- and *both*
    // transitions matter. A replica that latched on would refuse every command
    // for the rest of the session after someone once opened vim.
    //
    // The recording opens vim and quits it, which the fixture's own
    // expectations state: false, then true for the body, then false again.
    let seen = replay_watching("vim-macos", Replica::alt_screen);

    assert!(
        seen.iter().any(|&alt| alt),
        "a recorded vim session must put the replica on the alternate screen"
    );
    assert_eq!(
        seen.last(),
        Some(&false),
        "the recording quits vim, so the replica must come back off the alternate screen"
    );
}

/// One frame's worth of everything the correlation looks at.
type Snapshot = (Vec<BlockPayload>, u32, bool);

fn snapshots(name: &str) -> Vec<Snapshot> {
    replay_watching(name, |r| (r.blocks(), r.blocks_from(), r.alt_screen()))
}

/// Take an anchor at the first frame where a `run` could have written.
fn anchor_at(snaps: &[Snapshot], from: usize) -> Option<(usize, Anchor)> {
    snaps.iter().enumerate().skip(from).find_map(|(i, (b, _, alt))| {
        run::anchor(b, *alt).ok().map(|a| (i, a))
    })
}

/// Walk forward until the anchored command closes, and say where.
fn settle_at(snaps: &[Snapshot], from: usize, a: &Anchor) -> Option<(usize, BlockPayload)> {
    snaps.iter().enumerate().skip(from).find_map(|(i, (b, f, _))| match run::progress(b, *f, a) {
        Progress::Finished(blk) => Some((i, blk)),
        Progress::NotStarted | Progress::Running(_) | Progress::Lost => None,
    })
}

/// The correlation, against a genuine interactive zsh recorded off a real pty.
///
/// The property that makes `run` work at all, and the one #274's plan had
/// backwards. `begin_output` mutates `blocks.last_mut()`, so the command lands in
/// the trailing prompt block and **the id does not move** — this recording
/// anchors at block 0 and settles at block 0. A correlation waiting for an id
/// above the high-water mark would walk this whole recording and find nothing,
/// which is a `run` that always reports a timeout on a command that finished
/// instantly, with nothing anywhere saying why.
///
/// Replayed rather than run: no pty, no shell, no OSC 133 to arrange, and it
/// asserts identically on Windows, macOS and Linux.
#[test]
fn a_command_settles_in_the_block_that_was_already_there() {
    let snaps = snapshots("blocks-zsh");

    let (start, anchor) =
        anchor_at(&snaps, 0).expect("the recording reaches a live zsh prompt a `run` could use");
    assert_eq!(
        anchor.id, 0,
        "the anchor is the trailing prompt block itself, not the id after it"
    );

    let (at, blk) = settle_at(&snaps, start, &anchor)
        .expect("`echo hello` finishes in this recording, so the correlation must fire");

    assert_eq!(
        blk.id, anchor.id,
        "the command landed in the anchor's own block. An `id > high_water` test never \
         fires here, and that is the whole reason this module exists"
    );
    assert_eq!(blk.command, "echo hello");
    assert_eq!(
        blk.state,
        BlockState::Finished { exit_code: Some(0) },
        "the shell reported 0 and it must survive the wire as 0, not as unknown"
    );

    // It was seen running first: a correlation that only ever observes the final
    // state would pass this test while being unable to report a partial result
    // on a timeout, which is the case `run` exists to handle.
    assert!(
        snaps[start..at]
            .iter()
            .any(|(b, f, _)| matches!(run::progress(b, *f, &anchor), Progress::Running(_))),
        "the block must be seen `running` before it is seen `finished`"
    );
    assert!(
        matches!(run::progress(&snaps[start].0, snaps[start].1, &anchor), Progress::NotStarted),
        "and before that, not started at all -- the three states are distinguishable"
    );

    // The command the shell recorded is the one that was sent, so nothing is
    // flagged. `command_mismatch` firing on a clean recording would make the
    // warning worthless in the case it exists for.
    assert!(
        run::warnings(&snaps[at].0, &anchor, "echo hello", &blk).is_empty(),
        "a command that ran exactly as sent must carry no warnings"
    );
}

/// The second command in the same session, and the printed-nothing shape.
///
/// Its block is minted by the prompt that followed the first, so this is the case
/// where the id *does* move — the same code path has to accept both, which is why
/// [`run::progress`] matches `>=` and not `==`.
///
/// `false` also prints nothing, which the parser records as an *inverted* range
/// (`output_line: 6`, `end_line: 5`) rather than an absent `output_line`, so the
/// correlation must not mistake it for a command that never started.
#[test]
fn the_next_command_is_correlated_to_the_block_the_next_prompt_minted() {
    let snaps = snapshots("blocks-zsh");
    let (first_start, first) = anchor_at(&snaps, 0).expect("the first prompt");
    let (first_done, _) = settle_at(&snaps, first_start, &first).expect("`echo hello` finishes");

    let (start, anchor) = anchor_at(&snaps, first_done)
        .expect("zsh draws a fresh prompt after a command, and it is runnable again");
    assert_eq!(anchor.id, 1, "the prompt after a finished command is a new block");

    let (_, blk) = settle_at(&snaps, start, &anchor).expect("`false` finishes in this recording");
    assert_eq!(blk.command, "false");
    assert_eq!(
        blk.state,
        BlockState::Finished { exit_code: Some(1) },
        "a non-zero status must arrive as the number, not as `finished` with nothing"
    );
    assert!(
        blk.output_line > blk.end_line,
        "`false` printed nothing, which is an inverted range rather than an absent \
         `output_line` -- and it must still settle: {blk:?}"
    );

    // And it settles as *finished*, not as never-started, even though it owns no
    // output rows. Keying "did it start" off the row count rather than off
    // `output_line` is the mistake this pins.
    let r = replay("blocks-zsh");
    assert_eq!(
        r.block_rows(blk.id).expect("the block is known"),
        Vec::<String>::new(),
        "a command that printed nothing owns no rows, and that is not the same as \
         a command that never ran"
    );
}

/// A `run` into an editor is refused, at the moment it would have been written.
///
/// No OSC 133 marker is recorded on the alternate screen at all, so a command
/// submitted there could never settle — and the bytes would reach vim. Asserted
/// against the recording rather than a hand-built list, because the flag being
/// right frame by frame is what the refusal rests on.
#[test]
fn a_run_is_refused_for_every_frame_a_recording_spends_in_an_editor() {
    let snaps = snapshots("vim-macos");
    let alt: Vec<&Snapshot> = snaps.iter().filter(|(_, _, alt)| *alt).collect();
    assert!(!alt.is_empty(), "the recording opens vim; something is wrong upstream of this");

    for (b, _, a) in alt {
        assert_eq!(
            run::anchor(b, *a).expect_err("the alternate screen is never runnable"),
            run::Refusal::AltScreen,
            "an alt-screen frame must be refused by name, not fall through to `NoBlocks`"
        );
    }
}

/// `text_head_tail` bounds what it builds, not just what it returns.
///
/// `run_isolated` reads a session that gets the daemon's full scrollback, so a
/// chatty command leaves thousands of rows in the replica. Collecting them all
/// and truncating afterwards allocates a `String` per row only to drop it on
/// the next line; the cost has to be the size of the answer rather than the
/// size of the buffer. Asserted through the contract that makes that possible —
/// `total` counts everything while `shown` is bounded — because the allocation
/// itself is not observable from a test.
#[test]
fn the_whole_buffer_is_counted_but_only_the_ends_are_built() {
    let r = replay("blocks-zsh");

    let (all, total, omitted) = r.text_head_tail(usize::MAX);
    assert!(total > 0, "the recording has output; something is wrong upstream of this");
    assert_eq!(omitted, 0, "nothing is dropped when everything fits");
    assert_eq!(all.len(), total, "an unbounded read returns every line it counted");

    let (shown, total_again, omitted) = r.text_head_tail(4);
    assert_eq!(total_again, total, "the total is the buffer, not what was returned");
    assert!(shown.len() <= 4, "the bound is a bound: {} lines came back", shown.len());
    assert_eq!(
        shown.len() + omitted,
        total,
        "every line is either shown or counted as omitted -- a caller that adds them \
         up must get the whole output back"
    );
    assert_eq!(shown[0], all[0], "the beginning must survive: the command that failed is there");
    assert_eq!(
        shown[shown.len() - 1],
        all[total - 1],
        "and the end, where the error is. A tail-only truncation loses one of the two"
    );
}

/// Neither end of the output is a blank row, in any recording.
///
/// A grid is mostly empty space, and `run_isolated` returns rows rather than a
/// screen — twenty blank lines before the first word is pure token cost on
/// every call, and it makes a short command look like a long one.
///
/// **Every fixture, not one.** This first ran against `blocks-zsh` alone and so
/// did not notice that "blank" was being decided two different ways: rows were
/// kept or dropped by `Row::trimmed_len`, which counts a styled row with no
/// glyphs as content, while the text they turned into was trimmed of trailing
/// spaces and could come back `""`. A row of coloured background at either end
/// would have reappeared as the empty line this test exists to forbid, and one
/// recording is not enough grid to find that.
#[test]
fn no_recording_begins_or_ends_with_a_blank_row() {
    for name in ["basic-echo", "dir-colors", "git-log", "unicode-wide", "blocks-zsh", "vim-macos"] {
        let (shown, total, _) = replay(name).text_head_tail(usize::MAX);
        assert_eq!(shown.len(), total, "{name}: an unbounded read returns what it counted");

        // `vim-macos` opens vim and quits, which restores a primary screen with
        // nothing on it -- so a session holding no text at all is a legitimate
        // state, not a broken replica. `run_isolated` reports it as
        // `total_lines: 0`, the same answer a command that printed nothing
        // gets. There is no head or tail to check, and inventing one would be
        // asserting that every terminal has content in it.
        if shown.is_empty() {
            continue;
        }

        assert!(
            !shown[0].is_empty(),
            "{name}: leading blanks must be trimmed, and a kept row must not render \
             empty -- the trim rule and the text rule have to agree: {:?}",
            &shown[..2.min(shown.len())]
        );
        assert!(
            !shown[shown.len() - 1].is_empty(),
            "{name}: and trailing ones, which is most of a grid: {:?}",
            &shown[shown.len().saturating_sub(2)..]
        );
    }
}

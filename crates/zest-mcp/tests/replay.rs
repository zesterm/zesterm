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

use zest_mcp::Replica;
use zest_proto::{frame, FrameReader, HostMessage, SessionAddr, SessionId, HostId};

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

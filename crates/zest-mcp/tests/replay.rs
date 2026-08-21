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

use zest_mcp::run::{self, Progress};
use zest_mcp::tools::{block_anchor, finished_since};
use zest_mcp::Replica;
use zest_proto::{frame, BlockState, FrameReader, HostMessage, SessionAddr, SessionId, HostId};

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
                    history_clears,
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
                        history_clears,
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

/// The property `screen(after_seq:)` rests on, held against real host bytes.
///
/// A wait says "answer when the sequence moves past N". It is sound exactly
/// while nothing an agent would want to see can change without the sequence
/// moving — so that is what is asserted, against what a host really sent:
///
/// - **A sequence never goes backwards**, or a wait fires on a frame that undid
///   what it was waiting for.
/// - **A frame at an *unchanged* sequence changes no row, attribute or block.**
///   This is the one that matters, and it is not what reasoning first said. A
///   repeat happens: `blocks-zsh` carries two of them. `seq` is the terminal's
///   own version counter, bumped by `TermState::touch` on every observable
///   mutation, so a frame that does not advance it is restating state the
///   replica already holds — and a wait that sleeps through one has therefore
///   missed nothing.
///
/// **These frames come from `Encoder` directly** (`fixture_dump` drives it per
/// parse chunk), not from `Session::poll`, which additionally drops an update
/// whose delta turned out to carry nothing. So this is the *weaker* stream —
/// a daemon sends a subset of it — which is the right direction for a test:
/// pass here and the wire cannot be worse.
///
/// Written after this test first asserted "every frame advances the sequence"
/// and failed on `blocks-zsh` at 153.
#[test]
fn a_recorded_frame_that_repeats_a_sequence_restates_rather_than_changes() {
    for name in ["basic-echo", "dir-colors", "git-log", "unicode-wide", "blocks-zsh", "vim-macos"] {
        let fx = load(name);
        let mut reader = FrameReader::new();
        let mut previous: Option<u64> = None;
        let mut repeats = 0usize;

        for raw in &fx.frames {
            reader.feed(raw);
            while let Some(body) = reader.next_frame().expect("the fixture frames are well formed")
            {
                // `(seq, what it changed)`. A keyframe changes everything by
                // definition, so it is only ever checked for going backwards.
                let (seq, changes) =
                    match frame::decode::<HostMessage>(&body).expect("a fixture frame decodes") {
                        HostMessage::Keyframe { seq, .. } => (seq.0, true),
                        HostMessage::Update { seq, delta, .. } => (
                            seq.0,
                            !delta.ops.is_empty()
                                || !delta.attrs.is_empty()
                                || !delta.blocks.is_empty(),
                        ),
                        other => panic!("{name}: unexpected frame {other:?}"),
                    };

                if let Some(prev) = previous {
                    assert!(
                        seq >= prev,
                        "{name}: the sequence went backwards, {prev} then {seq} -- an \
                         `after_seq` wait would fire on a frame that undid what it waited for"
                    );
                    if seq == prev {
                        assert!(
                            !changes,
                            "{name}: a frame changed rows, attributes or blocks without \
                             advancing sequence {seq}. `after_seq` would sleep through a \
                             real change, which is the one way a long-poll can be silently \
                             wrong -- it answers late rather than answering incorrectly"
                        );
                        repeats += 1;
                    }
                }
                previous = Some(seq);
            }
        }

        assert!(previous.is_some(), "{name}: the recording produced no frames to check");
        // Printed rather than asserted: a corpus that stopped containing
        // repeats would make the arm above vacuous, and the next person should
        // be able to see that from a test run instead of inferring it.
        println!("{name}: {repeats} frame(s) at an unchanged sequence");
    }
}

/// The block wait fires exactly at the transition, on a real zsh recording.
///
/// This is the assertion that could not be reasoned to. `tools::finished_since`
/// anchors on the *tail* block rather than on the highest id already seen,
/// because OSC 133;C mutates `blocks.last_mut()` — and whether that is really
/// what a shell does is a question about zsh, not about this crate. So it is
/// asked of a capture, and the naive predicate is measured beside it rather
/// than merely described as wrong.
#[test]
fn a_block_wait_fires_when_the_recorded_command_ends_and_a_newer_id_would_not() {
    let frames = replay_watching("blocks-zsh", |r| r.block_states().collect::<Vec<_>>());

    // A moment an agent could have started a wait: the tail block is open, so
    // something is at the prompt or running in it.
    let start = frames
        .iter()
        .position(|f| block_anchor(f.iter().copied()).is_some_and(|(_, done)| !done))
        .expect("the recording has a moment with an unfinished command");
    let (anchor, was_finished) =
        block_anchor(frames[start].iter().copied()).expect("just checked it is Some");

    let fired = (start..frames.len())
        .find(|&i| finished_since(frames[i].iter().copied(), anchor, was_finished).is_some())
        .expect("the recorded command does finish");
    let id = finished_since(frames[fired].iter().copied(), anchor, was_finished)
        .expect("the frame that fired names a block");

    assert!(fired > start, "a wait must not answer with the state it began in");
    assert!(
        !frames[fired - 1].iter().any(|&(i, done)| i == id && done),
        "block {id} was already finished one frame earlier, so the wait fired late"
    );
    assert!(
        frames[fired].iter().any(|&(i, done)| i == id && done),
        "the frame that fired must be the one where block {id} finished"
    );

    // The trap, measured. A wait keyed on `id > anchor` -- the obvious reading
    // of `since_id`, and the one #274's plan had -- is looking for a block the
    // *next prompt* mints, which does not exist until after the command it is
    // waiting for has already ended.
    let naive = (start..frames.len())
        .find(|&i| frames[i].iter().any(|&(bid, done)| done && bid > anchor));
    assert!(
        naive.is_none_or(|n| n > fired),
        "the naive predicate fired at frame {naive:?}, no later than the correct one at \
         {fired} -- this recording no longer exercises the trap, so the assertion above \
         is no longer evidence of anything"
    );
}


/// `run`'s own states, walked over the same recording.
///
/// The layer above [`finished_since`]: a wait only has to know when something
/// ended, where `run` has just *written* and must also say whether the shell
/// started the command at all, and refuse the states a write would be swallowed
/// by. Held against the capture for the same reason — whether a submitted
/// command reuses the trailing prompt block's id is a question about zsh, and
/// the answer here is that it does: this anchors at block 0 and settles at
/// block 0.
#[test]
fn a_run_walks_not_started_then_running_then_finished_without_the_id_moving() {
    let frames = replay_watching("blocks-zsh", |r| (r.blocks(), r.blocks_from(), r.alt_screen()));

    let (start, anchor) = frames
        .iter()
        .enumerate()
        .find_map(|(i, (b, _, alt))| run::anchor(b, *alt).ok().map(|a| (i, a)))
        .expect("the recording reaches a live zsh prompt a `run` could write at");
    assert_eq!(anchor.id, 0, "the anchor is the trailing prompt block, not the id after it");

    let progress_at = |i: usize| {
        let (b, from, _) = &frames[i];
        run::progress(b, *from, &anchor)
    };

    assert!(
        matches!(progress_at(start), Progress::NotStarted),
        "nothing has been submitted yet, and that is distinguishable from running"
    );

    let (fired, blk) = (start..frames.len())
        .find_map(|i| match progress_at(i) {
            Progress::Finished(b) => Some((i, b)),
            _ => None,
        })
        .expect("`echo hello` finishes in this recording");

    assert_eq!(
        blk.id, anchor.id,
        "the command landed in the anchor's own block. An `id > high_water` test never \
         fires here, which is the whole reason the anchor is the tail block"
    );
    assert_eq!(blk.command, "echo hello");
    assert_eq!(blk.state, BlockState::Finished { exit_code: Some(0) });
    assert!(
        (start..fired).any(|i| matches!(progress_at(i), Progress::Running(_))),
        "the block must be seen `running` before it is seen `finished` -- a correlation \
         that only ever observes the end state cannot report a partial result on a timeout"
    );
    assert!(
        run::warnings(&frames[fired].0, &anchor, "echo hello", &blk).is_empty(),
        "a command that ran exactly as sent must carry no warnings"
    );
}

/// A `run` into an editor is refused, at the moment it would have written.
///
/// No OSC 133 marker is recorded on the alternate screen at all, so a command
/// submitted there could never settle — and the bytes would reach vim. Asserted
/// frame by frame against the recording rather than a hand-built list, because
/// the flag being right at every frame is what the refusal rests on.
#[test]
fn a_run_is_refused_for_every_frame_a_recording_spends_in_an_editor() {
    let frames = replay_watching("vim-macos", |r| (r.blocks(), r.alt_screen()));
    let alt: Vec<_> = frames.iter().filter(|(_, alt)| *alt).collect();
    assert!(!alt.is_empty(), "the recording opens vim; something is wrong upstream of this");

    for (b, a) in alt {
        assert_eq!(
            run::anchor(b, *a).expect_err("the alternate screen is never runnable"),
            run::Refusal::AltScreen,
            "an alt-screen frame must be refused by name, not fall through to `NoBlocks`"
        );
    }
}

/// Attributes survive the replay, and name the rows the text names.
///
/// The whole point of #348: flattened to characters, text an application is
/// *offering* is identical to text the user committed. These assert against
/// real recordings rather than a hand-built grid, because what matters is that
/// the bits arrive over the wire and land on the right cells -- a synthetic
/// terminal would prove only that the mask works.
#[test]
fn a_recorded_session_reports_where_its_attributes_are() {
    // `git log --color` paints its hashes and refs. Bold is the one it uses,
    // and it is what a `dim` ghost would look like structurally.
    let r = replay("git-log");
    let (spans, omitted) = r.styled_spans(400);
    assert_eq!(omitted, 0, "a 30-row screen must fit well under the ceiling");
    assert!(!spans.is_empty(), "a coloured git log carries attributes: {spans:?}");
    assert!(
        spans.iter().any(|s| s.names().contains(&"bold")),
        "git log --color emits bold; got {:?}",
        spans.iter().map(zest_mcp::StyledSpan::names).collect::<Vec<_>>()
    );
}

#[test]
fn a_span_never_names_a_line_the_text_does_not_have() {
    // The failure this prevents is the worst one available here: an attribute
    // reported against somebody else's line, which is *more* misleading than
    // reporting nothing. `screen_text` and `styled_spans` trim through one
    // function so they cannot disagree; this holds that across real sessions.
    for name in ["git-log", "vim-macos", "dir-colors", "blocks-zsh", "unicode-wide"] {
        let r = replay(name);
        let text = r.screen_text();
        let lines: Vec<&str> = text.lines().collect();
        let (spans, _) = r.styled_spans(4_000);
        for s in &spans {
            assert!(
                s.row < lines.len(),
                "{name}: span names row {} of {} lines: {s:?}",
                s.row,
                lines.len()
            );
            assert!(s.len > 0, "{name}: an empty span says nothing: {s:?}");
            assert!(!s.names().is_empty(), "{name}: a span with no attribute: {s:?}");
        }
    }
}

#[test]
fn layout_bits_are_not_reported_as_styling() {
    // `WIDE`, `WIDE_SPACER` and `WRAPLINE` share the flags word with the visual
    // bits. 250 of 274 flagged runs in the vim recording are `WRAPLINE` alone,
    // so reporting them would bury the signal in exactly the case the caller
    // most needs it. `unicode-wide` is the one that would show `WIDE` leaking.
    for name in ["vim-macos", "unicode-wide", "astral", "combining-marks"] {
        let r = replay(name);
        let (spans, _) = r.styled_spans(4_000);
        for s in &spans {
            assert!(
                zest_mcp::StyledSpan::VISUAL.contains(s.flags),
                "{name}: a layout bit reached a span: {s:?}"
            );
        }
    }
}

#[test]
fn a_plain_screen_reports_no_spans_at_all() {
    // The common case -- a shell at a prompt -- must cost nothing, which is
    // what lets this ride every `screen` call instead of being asked for.
    let r = replay("basic-echo");
    let (spans, omitted) = r.styled_spans(400);
    assert!(spans.is_empty(), "unstyled output must produce no spans: {spans:?}");
    assert_eq!(omitted, 0);
}

#[test]
fn the_recorded_corpus_is_almost_free_of_visual_attributes_and_that_is_the_measurement() {
    // Not a formality -- it is the number that makes this affordable on every
    // `screen` call rather than behind a flag. These recordings are rich in
    // *colour*, which is deliberately not reported; what is left is a handful
    // of bold runs across five sessions.
    //
    // It is also the honest statement of a blind spot: no recording here
    // contains reverse video at all, so the selection-bar case #348 turns on is
    // covered by the VT-driven tests in `src/session.rs` and cannot be covered
    // from a fixture. Same shape as #17.
    for name in ["git-log", "vim-macos", "dir-colors", "blocks-zsh", "basic-echo"] {
        let r = replay(name);
        let (spans, omitted) = r.styled_spans(400);
        assert_eq!(omitted, 0, "{name}: a recorded screen must not reach the ceiling");
        assert!(
            spans.len() <= 8,
            "{name}: {} spans -- if a recording ever gets rich, re-measure the claim that \
             this is free rather than quietly paying for it",
            spans.len()
        );
    }
}

/// The fast blankness test is the slow one, on every recorded row.
///
/// `visible_rows` stopped building a `String` per row to decide where the
/// screen ends (a prompt on an 80x24 grid asks about twenty-two empty rows to
/// find two full ones). The risk in that swap is not speed, it is *definition*:
/// a predicate that disagreed with `Row::text().trim().is_empty()` by one row
/// would move where `screen_text` stops and renumber every `styled` span with
/// it, silently. So the optimization is held against the thing it replaced,
/// across every row of every recording rather than the last frame alone.
#[test]
fn a_blank_row_is_the_one_the_text_would_have_dropped() {
    for name in ["git-log", "vim-macos", "dir-colors", "blocks-zsh", "basic-echo", "astral", "combining-marks", "unicode-wide"] {
        replay_watching(name, |r| {
            let text = r.screen_text();
            let lines = text.lines().count();
            let (spans, _) = r.styled_spans(4_000);
            for s in &spans {
                assert!(
                    s.row < lines,
                    "{name}: the blankness rule moved -- span names row {} of {lines}",
                    s.row
                );
            }
            // `screen_text` drops *trailing* blanks only, so the last line it
            // keeps must itself be non-blank. That is the property the two
            // predicates have to agree on, and the one a wrong rule breaks.
            if let Some(last) = text.lines().next_back() {
                assert!(
                    !last.trim().is_empty(),
                    "{name}: a trailing blank row survived, so the rule disagrees with `text()`"
                );
            }
        });
    }
}

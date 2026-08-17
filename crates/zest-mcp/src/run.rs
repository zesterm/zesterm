//! Correlating a command an agent submitted with the block the shell minted.
//!
//! Pure: block payloads in, a verdict out. No connection, no replica, no clock.
//! That is what makes every refusal and every edge below testable with nothing
//! spawned, on every CI platform — the same reason `tools.rs` shapes its JSON in
//! functions that take a [`BlockPayload`] rather than a session.
//!
//! # The anchor is the tail block, not the next id
//!
//! The obvious design is to record `max(block.id)` before writing and wait for
//! something above it. **That never fires.** OSC 133 `C` reaches
//! `BlockIndex::begin_output`, which mutates `blocks.last_mut()` in place — it
//! sets `output_line`, `command` and `state`, and mints no id at all. So the
//! command lands in the *existing* trailing prompt block, at an id **at or
//! below** the high-water mark, and only the prompt that follows it pushes a new
//! one. `begin_prompt` then re-anchors that trailing block rather than pushing
//! whenever it ran nothing (#193), so the id can legitimately never move for the
//! whole life of a session.
//!
//! The anchor is therefore the tail block's *identity* before the write, and the
//! thing to wait for is that block's **state** advancing. A `>` where this has a
//! `>=` is a `run` that always reports a timeout, on a command that finished
//! instantly, with nothing anywhere saying why.
//!
//! # Why `>=` rather than `==`
//!
//! zsh emits `C` from `preexec` and `D` only from `precmd`-when-something-ran, so
//! an empty Enter is a bare `A` and the id is reused. pwsh brackets even an empty
//! line with `C`/`D`, so every block closes and the next prompt genuinely pushes a
//! fresh id. Both shells are supported, so the correlation has to accept either —
//! the first block at or above the anchor that gained output is ours.

use zest_proto::{BlockPayload, BlockState};

/// The tail of the block list as it stood immediately before the write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchor {
    /// The trailing block's id. The command is expected to land *in* this block,
    /// not above it. See the module doc.
    pub id: u32,
}

/// How far a submitted command has got.
///
/// Four states rather than a `bool` and an `Option`, because the middle two are
/// different facts an agent acts on differently: a command still running can be
/// waited on or interrupted, while one the shell never started means the line was
/// not submitted at all — an unterminated quote, or a shell that is not where it
/// was thought to be.
#[derive(Debug, Clone, PartialEq)]
pub enum Progress {
    /// No OSC 133 `C` yet: the shell has not marked a command as started.
    NotStarted,
    /// Started and still producing output.
    Running(BlockPayload),
    /// Closed by OSC 133 `D`, carrying the shell's status if it reported one.
    Finished(BlockPayload),
    /// The anchor block is gone — a screen clear, a reflow that dropped it, or a
    /// `RIS` that reset the ids. Nothing this call can correlate any more, and
    /// saying so beats waiting out the caller's deadline in silence.
    Lost,
}

/// Why a session cannot be run in, or a command submitted.
///
/// Each names what to do instead, because the caller is a model: a refusal it
/// cannot act on costs the same tokens as one it can.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error(
        "this session is showing a full-screen program (alt screen), where no command \
         markers are emitted at all -- read `screen`, or run in a session of your own"
    )]
    AltScreen,
    #[error(
        "this session has no command blocks, so its shell has no integration (bash, fish \
         and cmd.exe emit no markers) or has not drawn a prompt yet -- use `run_isolated`, \
         which needs none and returns the process's own exit status"
    )]
    NoBlocks,
    /// Between OSC 133 `D` and the `A` that follows it.
    ///
    /// Momentary in an ordinary shell and its own variant for exactly that
    /// reason: a caller can wait this one out, where [`Self::Busy`] may not
    /// resolve for an hour. Two `run`s back to back land here almost every time —
    /// the first returns the instant `D` closes its block, and zsh emits the next
    /// prompt's `A` from `precmd` a moment later.
    #[error(
        "this session's shell has just finished a command and has not drawn its next \
         prompt yet; if it stays this way the shell has exited or is not interactive"
    )]
    NoPrompt,
    #[error(
        "a command is already running in this session; typing now would go into *its* \
         input, not the shell's -- wait for it, or `interrupt` it first"
    )]
    Busy,
    #[error(
        "`command` must be a single line: a newline submits more than one command and \
         there is then no single block to report -- use `input` for multi-line text"
    )]
    MultiLine,
    #[error(
        "`command` must not contain control characters -- use `input` to type them, or \
         `interrupt` for Ctrl+C"
    )]
    ControlBytes,
}

/// Take the anchor, refusing every state a submitted line would be swallowed by.
///
/// The gate mirrors the web client's `atShellPrompt`, whose doc gives the reason
/// in one line: during a running command the trailing block **is** that command,
/// so text plus a carriage return lands in its stdin rather than the shell's.
/// Getting this wrong does not fail — it silently answers a `Password:` prompt
/// with a `cargo build`.
pub fn anchor(blocks: &[BlockPayload], alt: bool) -> Result<Anchor, Refusal> {
    if alt {
        return Err(Refusal::AltScreen);
    }
    let tail = blocks.last().ok_or(Refusal::NoBlocks)?;
    // Three tails, and telling them apart is what decides whether waiting helps.
    // A *finished* tail is the gap between `D` and the next `A`, which is over in
    // a moment; a running one is somebody's command and may not end today.
    // Collapsing them into one refusal makes two `run`s in a row fail almost
    // every time, because the first returns the instant `D` arrives.
    //
    // `output_line` as well as the state: a block is `Prompt` before *and* after
    // `B`, and only the absence of output says nothing has been submitted. The
    // two agree today; requiring both means a future state that forgets to clear
    // one cannot be mistaken for an idle prompt.
    match tail.state {
        BlockState::Finished { .. } => Err(Refusal::NoPrompt),
        BlockState::Running => Err(Refusal::Busy),
        BlockState::Prompt if tail.output_line.is_some() => Err(Refusal::Busy),
        BlockState::Prompt => Ok(Anchor { id: tail.id }),
    }
}

/// Where the run has got to now.
///
/// # The anchor block still being there is the thing to check
///
/// Not the index's floor, which looks as though it should answer this and
/// cannot. `BlockIndex::authoritative_from` rises past *evicted* blocks and
/// falls for *destroyed* ones — but it falls with `min(lowest_gone)`, and the
/// floor of a young session is already 0, so a screen clear that erases the
/// anchor outright moves it by nothing at all.
///
/// The presence of the block is exact instead, because a new prompt **pushes**
/// rather than replacing: `begin_prompt` appends, and re-anchors the trailing
/// block only when that block is the one it would have replaced. So the anchor
/// is always still in the list unless something destroyed it — `ESC[2J`, which
/// drops every block not entirely above the cleared region and therefore always
/// takes an open prompt block with it, or `RIS`, which replaces the index and
/// restarts the ids at 0.
///
/// Reading the floor instead answered `NotStarted` for a `run clear`, which then
/// waited out the caller's whole deadline on a block that had been gone since the
/// first millisecond.
pub fn progress(blocks: &[BlockPayload], blocks_from: u32, a: &Anchor) -> Progress {
    if !blocks.iter().any(|b| b.id == a.id) {
        return Progress::Lost;
    }
    // And the floor having risen past it: the block is present but its lines
    // have aged out of scrollback, so the index no longer claims to be complete
    // there and nothing it says about the block can be relied on.
    if blocks_from > a.id {
        return Progress::Lost;
    }

    let Some(b) = ours(blocks, a) else {
        // The anchor block still exists and has no output: the shell has not
        // marked anything as started. Distinguished from `Lost` by the checks
        // above, which is the whole reason they come first.
        return Progress::NotStarted;
    };
    match b.state {
        BlockState::Finished { .. } => Progress::Finished(b.clone()),
        BlockState::Prompt | BlockState::Running => Progress::Running(b.clone()),
    }
}

/// The block this run's command landed in, if the shell has started one.
///
/// The *first* at or above the anchor that gained output. Later ones are somebody
/// else's — a human typing in the same session, or a forged marker — and
/// [`warnings`] says so rather than this quietly picking the newest.
fn ours<'a>(blocks: &'a [BlockPayload], a: &Anchor) -> Option<&'a BlockPayload> {
    blocks.iter().find(|b| b.id >= a.id && b.output_line.is_some())
}

/// What the caller should know but which does not make the result wrong.
///
/// Warnings rather than refusals because both cases are only ever detectable
/// *after* the fact. A pty is a byte stream: nothing can see a half-typed line
/// before it is submitted, and nothing can tell a shell's own OSC 133 from one a
/// program printed. Failing the call would throw away a real result to report a
/// suspicion; saying so beside the result does not.
#[must_use]
pub fn warnings(blocks: &[BlockPayload], a: &Anchor, requested: &str, got: &BlockPayload) -> Vec<String> {
    let mut out = Vec::new();

    // The shell's record of what ran is not what we asked to run. Two ordinary
    // causes and one alarming one: a person had already typed part of a line in
    // this session and ours was appended to it; the shell rewrote it (an alias,
    // a correction prompt); or a program forged the markers. All three are worth
    // the caller knowing before it believes the exit code.
    if !got.command.trim().is_empty() && got.command.trim() != requested.trim() {
        out.push(format!(
            "command_mismatch: this block records `{}`, not the command that was sent. \
             Something else was on the input line, the shell rewrote it, or the markers \
             were printed by a program",
            got.command.trim()
        ));
    }

    let started = blocks.iter().filter(|b| b.id >= a.id && b.output_line.is_some()).count();
    if started > 1 {
        out.push(format!(
            "multiple_blocks: {started} commands have started in this session since the \
             one that was sent; the result below is the first of them"
        ));
    }
    out
}

/// A command line, or a refusal naming the tool that takes what was passed.
///
/// **Refused rather than sanitized**, for the reason `opt_u16` refuses a
/// four-billion-cell grid where `max_lines` merely clamps: a bound on a response
/// can be silently tightened and still answer the question, while a newline
/// changes *what runs*. Stripping it would submit a different command than the
/// one asked for and then report the difference as a `command_mismatch` warning,
/// which is a strange way to spell "I did something else".
pub fn check_command(command: &str) -> Result<&str, Refusal> {
    let trimmed = command.trim();
    if trimmed.contains('\n') || trimmed.contains('\r') {
        return Err(Refusal::MultiLine);
    }
    // Every other C0, plus DEL. `\t` is deliberately allowed: it is ordinary in a
    // command line and the shell's own completion is not triggered by a byte
    // arriving in one write.
    if trimmed.chars().any(|c| (c.is_control() && c != '\t') || c == '\u{7f}') {
        return Err(Refusal::ControlBytes);
    }
    Ok(trimmed)
}

/// The name of a [`Progress`] for the wire, and the only place it is spelled.
///
/// `run` and `wait` report the same states and must not drift in what they call
/// them; a model that learns `finished` from one and reads `done` from the other
/// has to be told twice.
#[must_use]
pub fn state_name(p: &Progress) -> &'static str {
    match p {
        Progress::NotStarted => "prompt",
        Progress::Running(_) => "running",
        Progress::Finished(_) => "finished",
        Progress::Lost => "lost",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(id: u32, state: BlockState, output: Option<i64>, command: &str) -> BlockPayload {
        BlockPayload {
            id,
            prompt_line: i64::from(id) * 10,
            output_line: output,
            end_line: matches!(state, BlockState::Finished { .. }).then_some(i64::from(id) * 10 + 5),
            state,
            command: command.into(),
            cwd: "/tmp".into(),
            started_ms: None,
            ended_ms: None,
        }
    }

    fn prompt(id: u32) -> BlockPayload {
        block(id, BlockState::Prompt, None, "")
    }

    #[test]
    fn a_command_lands_in_the_prompt_block_that_was_already_there() {
        // The whole point of this module. `begin_output` mutates `last_mut()`, so
        // the id does not move: a correlation waiting for one above the anchor
        // waits for ever, on a command that finished instantly, and reports a
        // timeout with nothing anywhere saying why.
        let before = vec![block(0, BlockState::Finished { exit_code: Some(0) }, Some(1), "ls"), prompt(1)];
        let a = anchor(&before, false).expect("a live prompt is runnable");
        assert_eq!(a.id, 1, "the anchor is the trailing prompt, not the next id");

        let running = vec![before[0].clone(), block(1, BlockState::Running, Some(11), "cargo test")];
        assert!(
            matches!(progress(&running, 0, &a), Progress::Running(b) if b.id == 1),
            "the command lands in the anchor's own block, at the same id"
        );

        let done = vec![
            before[0].clone(),
            block(1, BlockState::Finished { exit_code: Some(3) }, Some(11), "cargo test"),
        ];
        let Progress::Finished(b) = progress(&done, 0, &a) else {
            panic!("a block closed by OSC 133;D must settle: {:?}", progress(&done, 0, &a))
        };
        assert_eq!(b.state, BlockState::Finished { exit_code: Some(3) });
    }

    #[test]
    fn a_new_id_also_counts_because_pwsh_brackets_an_empty_line() {
        // zsh reuses the trailing block's id; pwsh emits `C`/`D` even for an empty
        // Enter, so its next prompt genuinely pushes a fresh one. Both shells are
        // supported, so an `==` here would work on macOS and fail on Windows --
        // the asymmetry that makes this whole class invisible if tested on one.
        let a = Anchor { id: 4 };
        let blocks = vec![
            prompt(4),
            block(5, BlockState::Finished { exit_code: Some(0) }, Some(51), "echo hi"),
        ];
        assert!(
            matches!(progress(&blocks, 0, &a), Progress::Finished(b) if b.id == 5),
            "a command that landed in a block above the anchor is still ours"
        );
    }

    #[test]
    fn a_session_showing_a_full_screen_program_is_refused_by_name() {
        // No markers are recorded on the alternate screen at all, so a `run` into
        // vim could never settle -- and the bytes would reach the editor.
        let err = anchor(&[prompt(0)], true).expect_err("alt screen is not runnable");
        assert_eq!(err, Refusal::AltScreen);
        assert!(err.to_string().contains("screen"), "the refusal must name the way out: {err}");
    }

    #[test]
    fn a_session_with_no_blocks_at_all_names_run_isolated() {
        // bash, fish and cmd.exe emit nothing -- most Linux hosts, not an edge
        // case -- and the answer is a different tool rather than a retry.
        let err = anchor(&[], false).expect_err("no blocks means no correlation");
        assert_eq!(err, Refusal::NoBlocks);
        assert!(
            err.to_string().contains("run_isolated"),
            "a refusal a model cannot act on costs the same as one it can: {err}"
        );
    }

    #[test]
    fn a_session_with_a_command_already_running_is_refused_rather_than_typed_into() {
        // The failure this prevents is silent and specific: the text plus CR goes
        // into the running command's stdin, so a `cargo build` becomes the answer
        // to somebody's `Password:` prompt.
        let running = vec![block(0, BlockState::Running, Some(1), "sleep 30")];
        assert_eq!(anchor(&running, false).expect_err("busy"), Refusal::Busy);

        // A `Prompt` block that already has output is the same hazard wearing the
        // other field, which is why both are checked.
        let odd = vec![block(0, BlockState::Prompt, Some(1), "")];
        assert_eq!(anchor(&odd, false).expect_err("output means submitted"), Refusal::Busy);
    }

    #[test]
    fn a_shell_between_two_prompts_is_a_different_refusal_from_a_busy_one() {
        // The window two `run`s back to back land in almost every time: the first
        // returns the instant OSC 133;D closes its block, and the `A` for the next
        // prompt follows from `precmd` a moment later. Collapsing this into
        // `Busy` makes the second call fail on a shell that is doing nothing --
        // and it is not a hypothetical, it is what the live test caught.
        //
        // The two must stay apart because only one is worth waiting out: a
        // running command may not end for an hour.
        let settling = vec![block(0, BlockState::Finished { exit_code: Some(0) }, Some(1), "ls")];
        assert_eq!(
            anchor(&settling, false).expect_err("no prompt is showing yet"),
            Refusal::NoPrompt
        );
        let busy = vec![block(0, BlockState::Running, Some(1), "sleep 30")];
        assert_eq!(anchor(&busy, false).expect_err("a command is in flight"), Refusal::Busy);
    }

    #[test]
    fn a_command_carrying_a_newline_is_refused_rather_than_submitted_twice() {
        // Refused, not stripped: a newline changes *what runs*, so sanitizing it
        // would submit a different command and then report the difference as a
        // mismatch warning -- a strange way to spell "I did something else".
        assert_eq!(check_command("a\nb").expect_err("two commands"), Refusal::MultiLine);
        assert_eq!(check_command("a\rb").expect_err("CR submits too"), Refusal::MultiLine);
        assert_eq!(check_command("a\u{3}b").expect_err("^C"), Refusal::ControlBytes);
        assert_eq!(
            check_command("  cargo test  ").expect("an ordinary line is taken, trimmed"),
            "cargo test"
        );
        assert_eq!(
            check_command("grep -P '\\ta'").expect("a tab is ordinary in a command line"),
            "grep -P '\\ta'"
        );
    }

    #[test]
    fn a_screen_clear_under_a_running_command_is_lost_rather_than_a_timeout() {
        // What `run clear` does, and the case reading the *floor* got wrong.
        // `erase_screen` drops every block not entirely above the cleared region
        // -- which always includes an open prompt block -- and lowers the floor
        // with `min(lowest_gone)`. A young session's floor is already 0, so it
        // moves by nothing at all, and the next prompt then pushes an id *above*
        // the anchor. Keyed off the floor this read as `NotStarted` and waited out
        // the caller's whole deadline; keyed off the anchor still being in the
        // list it is `Lost` at once.
        let a = Anchor { id: 5 };
        let cleared = vec![
            block(0, BlockState::Finished { exit_code: Some(0) }, Some(1), "ls"),
            // Minted by the prompt drawn after the clear. Not ours, however
            // plausible its id looks.
            block(6, BlockState::Running, Some(61), "whoami"),
        ];
        assert_eq!(
            progress(&cleared, 0, &a),
            Progress::Lost,
            "the anchor block was erased, so nothing here is the command that was sent"
        );

        assert_eq!(progress(&[], 0, &a), Progress::Lost, "an emptied index cannot be correlated");
        assert_eq!(
            progress(&[prompt(0)], 0, &a),
            Progress::Lost,
            "`RIS` restarts the ids at 0, so a lower tail is a different session's numbering"
        );
        assert_eq!(
            progress(&[prompt(5)], 6, &a),
            Progress::Lost,
            "and a floor above the anchor: the block is here but the index no longer \
             claims to be complete where it sits"
        );

        // And the case that must *not* be lost: the anchor is still there and the
        // shell simply has not started anything.
        assert_eq!(progress(&[prompt(5)], 0, &a), Progress::NotStarted);
    }

    #[test]
    fn a_block_whose_command_is_not_the_one_asked_for_warns_rather_than_fails() {
        // A person had already typed `rm -rf /tm` when `ls` was sent, so the shell
        // ran `rm -rf /tmls`. Only ever detectable afterwards -- nothing can see a
        // half-typed line before it is submitted -- and throwing the result away
        // to report the suspicion would lose the only evidence of what happened.
        let a = Anchor { id: 1 };
        let got = block(1, BlockState::Finished { exit_code: Some(0) }, Some(11), "rm -rf /tmls");
        let w = warnings(std::slice::from_ref(&got), &a, "ls", &got);
        assert_eq!(w.len(), 1, "one warning, and it must quote what really ran: {w:?}");
        assert!(w[0].starts_with("command_mismatch:"), "{}", w[0]);
        assert!(w[0].contains("rm -rf /tmls"), "{}", w[0]);

        // The ordinary case is silent.
        let same = block(1, BlockState::Finished { exit_code: Some(0) }, Some(11), "ls");
        assert!(warnings(std::slice::from_ref(&same), &a, " ls ", &same).is_empty());
    }

    #[test]
    fn a_second_command_starting_in_the_same_session_is_reported_beside_the_first() {
        // The forged-OSC-133 case is detectable precisely here, and so is a human
        // typing while the agent waits. The result stays the *first* block rather
        // than the newest, because that is the one the caller asked about.
        let a = Anchor { id: 1 };
        let blocks = vec![
            block(1, BlockState::Finished { exit_code: Some(0) }, Some(11), "ls"),
            block(2, BlockState::Running, Some(21), "whoami"),
        ];
        assert!(
            matches!(progress(&blocks, 0, &a), Progress::Finished(b) if b.id == 1),
            "the first started block is ours; the newest is somebody else's"
        );
        let w = warnings(&blocks, &a, "ls", &blocks[0]);
        assert_eq!(w.len(), 1);
        assert!(w[0].starts_with("multiple_blocks: 2"), "{}", w[0]);
    }

    #[test]
    fn every_state_has_one_name_shared_by_both_tools() {
        // `run` and `wait` report the same states, and a model told `finished` by
        // one and `done` by the other has to learn the vocabulary twice.
        assert_eq!(state_name(&Progress::NotStarted), "prompt");
        assert_eq!(state_name(&Progress::Running(prompt(0))), "running");
        assert_eq!(state_name(&Progress::Finished(prompt(0))), "finished");
        assert_eq!(state_name(&Progress::Lost), "lost");
    }
}

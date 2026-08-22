//! Doing something with a command block.
//!
//! Copy what a command printed, run it again, find where it started. The three
//! things a block is *for* — a scrollback that knows where commands begin is
//! only interesting if you can act on that.
//!
//! # Why this is not in `zest-core`
//!
//! A block is a fact about the session and belongs there; *acting* on one is a
//! decision this window made, and two clients looking at the same session
//! should be able to disagree about it. The same reasoning that keeps the
//! selection per-viewport rather than per-session.
//!
//! # Why re-run needs no protocol type
//!
//! It is the command text typed again — `SessionSource::write` with the bytes
//! and a carriage return. A `ClientMessage::ReRun` would be a second way to do
//! what `Input` already does, and the host would have to trust the client about
//! what the command was anyway.

use zest_core::{AbsPos, Block, LineId, Selection, SelectionMode, Terminal};

/// The rows a fold would hide, or `None` when there is nothing to hide.
///
/// The single definition of "this block can be folded". The header pass draws
/// its chevron from the same answer, so an affordance can never be offered
/// that the fold then declines — which is what a command that printed nothing
/// (`cd ..`) and every still-running command used to get.
///
/// **Returns a half-open range, `[output_line, end_line + 1)`**, because that is
/// what the row test below wants. The `+ 1` is the conversion, not slack: a
/// `Block`'s `end_line` is the last output row *inclusive* — the parser already
/// pulled `D` back onto it — so a one-line output has `end_line == output_line`
/// and must still fold, which an unconverted half-open range would drop.
#[must_use]
pub fn fold_range(b: &Block) -> Option<(LineId, LineId)> {
    let (o, e) = (b.output_line?, b.end_line?);
    (e >= o).then_some((o, e + 1))
}

/// The fold view: for each visual row, the absolute storage index the
/// renderer should draw there (`usize::MAX` = blank filler, which trails once
/// history runs out), or `None` when nothing folded is in play — the everyday
/// fast path, which must stay allocation-free.
///
/// Built by walking upward from the viewport's bottom row, skipping every row
/// inside a folded block's output range (half-open, from [`fold_range`]), until
/// the screen is full — which is exactly "the rows compact and more scrollback
/// shows" (ROADMAP, WS-E). What remains after history is exhausted is padded
/// *after* the reversal, so the content sits at the top and the blanks below
/// it; padding first put them above and sank the whole view.
#[must_use]
pub fn fold_row_map(
    term: &Terminal,
    folded: &std::collections::BTreeSet<u32>,
) -> Option<Vec<usize>> {
    if folded.is_empty() || term.in_alt_screen() {
        return None;
    }
    let ranges: Vec<(u64, u64)> = term
        .blocks()
        .blocks()
        .iter()
        .filter(|b| folded.contains(&b.id.0))
        .filter_map(fold_range)
        .collect();
    if ranges.is_empty() {
        return None;
    }
    let grid = term.grid();
    let rows = grid.rows();
    let mut picked = Vec::with_capacity(rows);
    let mut i = grid.abs_index(rows.saturating_sub(1)) as i64;
    while i >= 0 && picked.len() < rows {
        #[allow(clippy::cast_sign_loss, reason = "guarded by the loop condition")]
        let idx = i as usize;
        if let Some(row) = grid.line(idx) {
            if !ranges.iter().any(|&(o, e)| row.id >= o && row.id < e) {
                picked.push(idx);
            }
        }
        i -= 1;
    }
    // Reverse first, then pad. Padding before the reverse put the blank filler
    // at the *top*, so when a fold outran the history there was nothing to pull
    // in with, the surviving rows sank by exactly the number of lines hidden --
    // fold the only command in a fresh session and its header landed on the
    // last rows of an empty screen. A terminal fills from the top.
    picked.reverse();
    while picked.len() < rows {
        picked.push(usize::MAX);
    }
    Some(picked)
}

/// The block a viewport row belongs to, in the *plain* (unfolded) view.
///
/// `None` above the first prompt of the session, which is ordinary: output that
/// arrived before any shell integration was loaded is not part of a command.
///
/// The app's click path no longer calls this directly — a click's row means
/// whatever the fold view drew there, so `App::visual_line_at` does the row
/// half and `block_at` the rest. This stays as the documented plain mapping
/// the tests below exercise.
#[cfg(test)]
#[must_use]
pub fn at_row(term: &Terminal, row: usize) -> Option<Block> {
    let line = term.grid().line_id_at(row)?;
    term.blocks().block_at(line).cloned()
}

/// The most recent block that actually printed something.
///
/// What a keyboard shortcut should act on, and not the same as the block the
/// cursor is in: sitting at a fresh prompt, the cursor's block has been started
/// and has printed nothing, so "copy the output of the command I just ran"
/// would copy nothing at all — which is the state a terminal spends most of its
/// life in.
#[must_use]
pub fn last_with_output(term: &Terminal) -> Option<Block> {
    term.blocks().blocks().iter().rev().find(|b| b.output_line.is_some()).cloned()
}

/// What a command printed, without the prompt or the command itself.
///
/// `None` rather than an empty string when there is nothing to copy, so a
/// mis-aimed shortcut cannot silently clobber the clipboard — the same rule
/// [`Terminal::selection_text`] follows.
///
/// A running command copies what it has printed *so far*. Refusing until it
/// finishes would be useless for the case that matters: a build you want to
/// paste into a bug report while it is still going.
#[must_use]
pub fn output_text(term: &Terminal, block: &Block) -> Option<String> {
    let text = term.grid().selection_text(&output_selection(term, block)?);
    let text = text.trim_end_matches('\n');
    (!text.is_empty()).then(|| text.to_string())
}

/// The rows a block's output occupies, as a selection.
///
/// The one the clipboard copies *and* the one "select block text" installs, so
/// the two verbs cannot come to describe different rows — they were one
/// expression buried inside [`output_text`] until a second caller needed it.
///
/// Absolute line ids, not viewport rows, which is what lets it survive
/// scrolling and folding without translation: [`fold_row_map`] is a question
/// about *drawing*, and a selection is stored below that.
///
/// Handles the running case, where `end_line` is `None` and the range ends at
/// the newest line the grid holds. [`fold_range`] deliberately does not:
/// folding needs a finished block, selecting does not.
#[must_use]
pub fn output_selection(term: &Terminal, block: &Block) -> Option<Selection> {
    // From where output began, not from the prompt: the point of the block
    // index is knowing the difference. Copying the prompt back into a bug
    // report is what "select the rough area with the mouse" already does badly.
    let from = block.output_line?;
    let to = block.end_line.unwrap_or_else(|| last_line(term));
    if to < from {
        return None;
    }
    Some(Selection {
        anchor: AbsPos::new(from, 0),
        head: AbsPos::new(to, term.grid().cols().saturating_sub(1)),
        mode: SelectionMode::Line,
    })
}

/// A block as one paste: what ran, then what it printed.
///
/// For the bug report and the chat message, which is the case the block index
/// exists to serve — and the reason there is **no `$` sigil** on the command.
/// A prompt character makes the result unpasteable, and [`output_text`]'s own
/// contract is that a copied block never carries one.
///
/// `None` only when both halves are empty; a command that printed nothing is
/// still worth copying, since "it printed nothing" is often the point.
#[must_use]
pub fn command_and_output(term: &Terminal, block: &Block) -> Option<String> {
    let command = block.command.trim();
    match (command.is_empty(), output_text(term, block)) {
        (true, None) => None,
        (true, Some(out)) => Some(out),
        (false, None) => Some(command.to_string()),
        (false, Some(out)) => Some(format!("{command}\n{out}")),
    }
}

/// The bytes that would re-run this command.
///
/// `None` when the command text was never captured — a block whose `C` marker
/// arrived without a `B` before it, which a partial shell integration produces.
/// Sending a bare carriage return in that case would run whatever happens to be
/// on the prompt already, which is somebody else's command.
#[must_use]
pub fn rerun_bytes(block: &Block) -> Option<Vec<u8>> {
    let command = block.command.trim();
    if command.is_empty() {
        return None;
    }
    // CR, not LF: a pty expects the carriage return a keyboard produces.
    let mut bytes = command.as_bytes().to_vec();
    bytes.push(b'\r');
    Some(bytes)
}

/// The bytes that would `cd` the shell to `path` (#426).
///
/// Single quotes are the one wrapping posix shells and PowerShell both read
/// literally, so one spelling serves whatever is on the far end — *except*
/// for a path containing a single quote, whose escape the two families
/// spell differently. Those return `None`, and the menu excludes them
/// upstream: a `cd` that lands somewhere wrong is worse than a row missing.
#[must_use]
pub fn cd_bytes(path: &str) -> Option<Vec<u8>> {
    if path.is_empty() || path.contains('\'') {
        return None;
    }
    // CR, not LF: a pty expects the carriage return a keyboard produces.
    let mut bytes = format!("cd '{path}'").into_bytes();
    bytes.push(b'\r');
    Some(bytes)
}

/// Whether the shell is the thing reading: primary screen, trailing block an
/// open prompt. The web client's `atShellPrompt`, whose doc has the reason —
/// during a running command typed bytes land in that command's stdin, and in
/// the alt screen they land in a full-screen program's document.
#[must_use]
pub fn at_shell_prompt(term: &Terminal) -> bool {
    !term.in_alt_screen()
        && term.blocks().last().is_some_and(|b| b.output_line.is_none())
}

/// The newest line the grid holds, for a block that has not ended.
///
/// Active space: "what has this command printed so far" is a question about the
/// live screen. Read through the display it would truncate copy-output at
/// wherever the user happened to have scrolled to, and return nothing at all
/// once they scrolled above the command's first output row.
fn last_line(term: &Terminal) -> LineId {
    let grid = term.grid();
    grid.active_line_id_at(grid.rows().saturating_sub(1)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cd_bytes_wraps_once_and_declines_the_path_it_cannot_quote_portably() {
        assert_eq!(
            cd_bytes("/Users/andy/My Code").as_deref(),
            Some(b"cd '/Users/andy/My Code'\r".as_slice()),
            "single quotes are the one wrapping both shell families read literally"
        );
        assert!(
            cd_bytes("/Users/andy's stuff").is_none(),
            "a quoted quote is spelled differently per shell; declining beats guessing"
        );
        assert!(cd_bytes("").is_none());
    }

    #[test]
    fn at_shell_prompt_is_the_trailing_open_prompt_and_nothing_else() {
        let mut t = Terminal::new(40, 8, 200);
        assert!(!at_shell_prompt(&t), "no markers means no prompt anyone can vouch for");
        t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07");
        assert!(at_shell_prompt(&t), "an open trailing prompt is the one green state");
        t.advance(b"make\x1b]133;C\x07\r\n");
        assert!(!at_shell_prompt(&t), "typed bytes now land in make's stdin");
    }

    /// A session with one finished command and one still running.
    fn session() -> Terminal {
        let mut t = Terminal::new(40, 8, 200);
        t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07echo hi\x1b]133;C\x07\r\n");
        t.advance(b"hi\r\n\x1b]133;D;0\x07");
        t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07make\x1b]133;C\x07\r\n");
        t.advance(b"building\r\n");
        t
    }

    #[test]
    fn folding_hides_exactly_the_output_rows_and_pulls_history_in() {
        // The compaction rule: a folded block's output rows leave the view,
        // everything else keeps its order, and the freed rows come from
        // scrollback above (or blank filler when there is none).
        let t = session();
        let finished = t.blocks().blocks()[0].clone();
        let folded = std::collections::BTreeSet::from([finished.id.0]);

        let map = fold_row_map(&t, &folded).expect("a fold produces a map");
        assert_eq!(map.len(), t.grid().rows(), "one entry per visual row");

        let hidden: Vec<u64> =
            (finished.output_line.unwrap()..=finished.end_line.unwrap()).collect();
        for &idx in &map {
            if idx == usize::MAX {
                continue;
            }
            let id = t.grid().line(idx).expect("mapped rows exist").id;
            assert!(
                !hidden.contains(&id),
                "line {id} is inside the folded output and must not be drawn"
            );
        }
        let drawn: Vec<usize> = map.iter().copied().filter(|&i| i != usize::MAX).collect();
        assert!(drawn.windows(2).all(|w| w[0] < w[1]), "visual order is grid order");

        // And with nothing folded, no map at all: the fast path stays free.
        assert!(fold_row_map(&t, &std::collections::BTreeSet::new()).is_none());
    }

    #[test]
    fn a_fold_that_outruns_history_leaves_its_blank_rows_at_the_bottom() {
        // Filler was pushed before the reverse, so it landed at the *top* and
        // the surviving rows sank by exactly the number of lines hidden: fold
        // the only command in a fresh session and the header ends up on the
        // last rows of an otherwise empty screen. A terminal fills from the
        // top; blank space belongs below the prompt, not above the first block.
        let t = session();
        let finished = t.blocks().blocks()[0].clone();
        let folded = std::collections::BTreeSet::from([finished.id.0]);
        let map = fold_row_map(&t, &folded).expect("a fold produces a map");

        assert_ne!(map[0], usize::MAX, "the first visual row must hold content");
        let first_blank = map.iter().position(|&i| i == usize::MAX).unwrap_or(map.len());
        assert!(
            map[first_blank..].iter().all(|&i| i == usize::MAX),
            "blank filler must be one trailing run, got {map:?}"
        );
    }

    #[test]
    fn the_output_is_copied_without_the_prompt_or_the_command() {
        // The whole reason to have a block index rather than a mouse: dragging
        // over the output picks up the prompt line more often than not, and a
        // pasted bug report then starts with somebody's shell prompt.
        let t = session();
        let finished = &t.blocks().blocks()[0];
        assert_eq!(output_text(&t, finished).as_deref(), Some("hi"));
    }

    #[test]
    fn a_running_command_copies_what_it_has_printed_so_far() {
        // The case that matters: a build you want to paste somewhere while it
        // is still going. Waiting for the end line would make this useless
        // exactly when it is wanted.
        let t = session();
        let running = t.blocks().last().expect("a running block");
        assert!(running.is_running());
        assert_eq!(output_text(&t, running).as_deref(), Some("building"));
    }

    #[test]
    fn a_command_that_printed_nothing_copies_nothing() {
        // `None`, not `""`. A mis-aimed shortcut must not clobber the clipboard
        // -- the rule `Terminal::selection_text` already follows.
        let mut t = Terminal::new(40, 8, 200);
        t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07true\x1b]133;C\x07\r\n\x1b]133;D;0\x07");
        assert_eq!(output_text(&t, t.blocks().last().expect("one block")), None);
    }

    #[test]
    fn the_selection_that_copies_the_output_is_the_one_select_installs() {
        // Why `output_selection` exists at all. "Copy output" and "select block
        // text" are two verbs over one range, and the range was an expression
        // buried inside `output_text` — a second caller would have written its
        // own, and the two would have drifted a row apart at the first edge
        // case, silently, in whichever direction nobody tested.
        let t = session();
        let finished = &t.blocks().blocks()[0];
        let sel = output_selection(&t, finished).expect("a finished block has a range");
        assert_eq!(
            t.grid().selection_text(&sel).trim_end_matches('\n'),
            output_text(&t, finished).expect("the same block copies"),
            "the installed selection must read back exactly what the clipboard gets"
        );
    }

    #[test]
    fn a_running_block_can_still_be_selected_to_its_last_printed_row() {
        // `fold_range` declines a running block — folding needs a finished one
        // — so a selection built off that would refuse the case people most
        // want: highlighting a build's output while it runs.
        let t = session();
        let running = t.blocks().last().expect("a running block");
        assert!(running.is_running() && fold_range(running).is_none());
        let sel = output_selection(&t, running).expect("a running block still selects");
        assert_eq!(t.grid().selection_text(&sel).trim_end_matches('\n'), "building");
    }

    #[test]
    fn copying_command_and_output_puts_the_command_first_and_no_prompt() {
        // For the bug report: what ran, then what it printed. No `$`, because
        // a sigil makes the result unpasteable — the same rule `output_text`
        // already follows for the prompt line.
        let t = session();
        assert_eq!(
            command_and_output(&t, &t.blocks().blocks()[0]).as_deref(),
            Some("echo hi\nhi")
        );
    }

    #[test]
    fn a_command_that_printed_nothing_still_copies_as_a_block() {
        // Unlike `output_text`, which returns `None` so a mis-aimed chord
        // cannot clobber the clipboard: here the command *is* the content, and
        // "it printed nothing" is frequently the thing being reported.
        let mut t = Terminal::new(40, 8, 200);
        t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07true\x1b]133;C\x07\r\n\x1b]133;D;0\x07");
        let b = t.blocks().last().expect("one block");
        assert_eq!(output_text(&t, b), None);
        assert_eq!(command_and_output(&t, b).as_deref(), Some("true"));
    }

    #[test]
    fn re_running_sends_the_command_and_a_carriage_return() {
        let t = session();
        assert_eq!(rerun_bytes(&t.blocks().blocks()[0]), Some(b"echo hi\r".to_vec()));
    }

    #[test]
    fn a_block_with_no_command_text_will_not_be_re_run() {
        // A `C` with no `B` before it -- a partial integration. Sending a bare
        // carriage return would run whatever is sitting on the prompt, which is
        // somebody else's command, and re-run is a button people press quickly.
        let mut t = Terminal::new(40, 8, 200);
        t.advance(b"\x1b]133;A\x07\x1b]133;C\x07\r\nout\r\n\x1b]133;D;0\x07");
        assert_eq!(rerun_bytes(t.blocks().last().expect("one block")), None);
    }

    #[test]
    fn a_row_maps_to_the_block_that_produced_it() {
        let t = session();
        // Row 1 is `hi`, printed by the first command.
        assert_eq!(at_row(&t, 1).map(|b| b.command), Some("echo hi".to_string()));
        // The cursor's own row is in the running one.
        assert_eq!(
            at_row(&t, t.cursor().row).map(|b| b.command),
            Some("make".to_string())
        );
    }

    #[test]
    fn at_a_fresh_prompt_the_target_is_the_command_that_just_ran() {
        // The state a terminal spends most of its life in. The cursor's own
        // block is the one the prompt just opened, which has printed nothing --
        // so a shortcut wired to it would copy nothing, almost always.
        let mut t = Terminal::new(40, 8, 200);
        t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07echo hi\x1b]133;C\x07\r\nhi\r\n\x1b]133;D;0\x07");
        t.advance(b"\x1b]133;A\x07$ ");

        assert_eq!(
            at_row(&t, t.cursor().row).map(|b| b.command),
            Some(String::new()),
            "the cursor's own block is the fresh prompt, which has printed nothing"
        );
        let target = last_with_output(&t).expect("the command that just ran");
        assert_eq!(target.command, "echo hi");
        assert_eq!(output_text(&t, &target).as_deref(), Some("hi"));
    }

    #[test]
    fn a_session_with_no_integration_has_no_blocks_to_act_on() {
        // Every action must be a no-op rather than a panic when the shell emits
        // nothing, which is every shell until integration is loaded.
        let mut t = Terminal::new(40, 8, 200);
        t.advance(b"just output\r\n");
        assert!(at_row(&t, t.cursor().row).is_none());
        assert!(at_row(&t, 0).is_none());
        assert!(last_with_output(&t).is_none());
    }
}

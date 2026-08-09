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

/// The fold view: for each visual row, the absolute storage index the
/// renderer should draw there (`usize::MAX` = blank filler at the top when
/// history ran out), or `None` when nothing folded is in play — the everyday
/// fast path, which must stay allocation-free.
///
/// Built by walking upward from the viewport's bottom row, skipping every row
/// inside a folded block's output range, until the screen is full — which is
/// exactly "the rows compact and more scrollback shows" (ROADMAP, WS-E).
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
        .filter_map(|b| {
            let o = b.output_line?;
            let e = b.end_line?;
            // Inclusive: the parser already adjusted `D` back onto the last
            // output row, so a one-line output has `e == o` and still folds.
            (e >= o).then_some((o, e + 1))
        })
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
    while picked.len() < rows {
        picked.push(usize::MAX);
    }
    picked.reverse();
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
    // From where output began, not from the prompt: the point of the block
    // index is knowing the difference. Copying the prompt back into a bug
    // report is what "select the rough area with the mouse" already does badly.
    let from = block.output_line?;
    let to = block.end_line.unwrap_or_else(|| last_line(term));
    if to < from {
        return None;
    }

    let sel = Selection {
        anchor: AbsPos::new(from, 0),
        head: AbsPos::new(to, term.grid().cols().saturating_sub(1)),
        mode: SelectionMode::Line,
    };
    let text = term.grid().selection_text(&sel);
    let text = text.trim_end_matches('\n');
    (!text.is_empty()).then(|| text.to_string())
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

/// The newest line the grid holds, for a block that has not ended.
fn last_line(term: &Terminal) -> LineId {
    let grid = term.grid();
    grid.line_id_at(grid.rows().saturating_sub(1)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

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

//! A block's ⋯ menu (design §3): what you can do to one command, as data.
//!
//! The launcher discipline: rows and their actions are parallel lists built in
//! one pass, so index `n` means the same thing to the renderer and the input
//! path by construction. Pure — no window, no terminal lock — which is what
//! lets the enable rules live under `cargo test`.
//!
//! # Why every row is enabled by the helper that performs it
//!
//! Each predicate below calls the *same* function the action calls, rather
//! than a second reading of the block that happens to agree today. A menu that
//! offers "re-run" for a block whose command was never captured is the fold
//! chevron's old bug wearing a different hat — an affordance that answers a
//! click by doing nothing, which reads as the app being broken rather than as
//! the row being inapplicable.

use zest_core::Block;

use crate::block_actions;
use crate::chrome::model::BlockMenuRow;

/// What one row of a block menu does, parallel to the drawn rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockMenuAction {
    /// Fold or unfold this block's output.
    Fold,
    CopyOutput,
    CopyCommand,
    /// The command and what it printed, as one paste.
    CopyBoth,
    Rerun,
    /// Run it again in a fresh tab, in the block's own directory.
    RerunInNewTab,
    /// Put the grid selection over the block's output rows.
    SelectText,
    /// A divider; Enter does nothing and the selection skips it.
    None,
}

/// The rows for one block, and what each does.
///
/// `folded` is the pane's fold state for this block — the label flips rather
/// than the menu growing a second row, because "fold" and "unfold" are one
/// affordance in one place. The chords arrive platform-spelled from
/// `keymap::chord_for`.
///
/// Takes the terminal because that is what the *actions* take: `output_line`
/// being set is not the same question as "there is output to copy" — the `C`
/// marker sets it before anything is printed, so `true` and `cd ..` have one
/// and print nothing. Asking the block alone got that wrong; asking
/// `output_text` cannot.
#[must_use]
pub fn build_rows(
    term: &zest_core::Terminal,
    block: &Block,
    folded: bool,
    copy_chord: &str,
    rerun_chord: &str,
) -> (Vec<BlockMenuRow>, Vec<BlockMenuAction>) {
    let has_command = !block.command.trim().is_empty();

    let spec: [(&str, &str, bool, BlockMenuAction); 10] = [
        (
            if folded { "Unfold" } else { "Fold" },
            "",
            // The single definition of "this block can be folded", shared with
            // the header's chevron — so the menu cannot offer what the fold
            // then declines.
            block_actions::fold_range(block).is_some(),
            BlockMenuAction::Fold,
        ),
        ("", "", false, BlockMenuAction::None),
        (
            "Copy output",
            copy_chord,
            block_actions::output_text(term, block).is_some(),
            BlockMenuAction::CopyOutput,
        ),
        ("Copy command", "", has_command, BlockMenuAction::CopyCommand),
        (
            "Copy command + output",
            "",
            block_actions::command_and_output(term, block).is_some(),
            BlockMenuAction::CopyBoth,
        ),
        ("", "", false, BlockMenuAction::None),
        // `rerun_bytes`, not `has_command`: it also refuses a block whose `C`
        // marker arrived without a `B`, where sending a bare carriage return
        // would run whatever is on the prompt already — somebody else's command.
        (
            "Re-run",
            rerun_chord,
            block_actions::rerun_bytes(block).is_some(),
            BlockMenuAction::Rerun,
        ),
        (
            "Re-run in new tab",
            "",
            block_actions::rerun_bytes(block).is_some(),
            BlockMenuAction::RerunInNewTab,
        ),
        ("", "", false, BlockMenuAction::None),
        (
            "Select block text",
            "",
            block_actions::output_selection(term, block).is_some(),
            BlockMenuAction::SelectText,
        ),
    ];

    let mut rows = Vec::with_capacity(spec.len());
    let mut actions = Vec::with_capacity(spec.len());
    for (label, chord, enabled, action) in spec {
        rows.push(if action == BlockMenuAction::None {
            BlockMenuRow::Divider
        } else {
            BlockMenuRow::Action {
                label: label.to_string(),
                chord: chord.to_string(),
                enabled,
            }
        });
        actions.push(action);
    }
    (rows, actions)
}

/// Move the keyboard selection to the next row that does something.
///
/// Stops at the ends rather than wrapping, and steps over dividers *and*
/// disabled rows — the same list the click path refuses, so the keyboard
/// cannot reach a row the mouse cannot.
#[must_use]
pub fn step(rows: &[BlockMenuRow], actions: &[BlockMenuAction], from: usize, down: bool) -> usize {
    let actionable = |i: &usize| is_actionable(rows, actions, *i);
    if down {
        (from + 1..actions.len()).find(actionable).unwrap_or(from)
    } else {
        (0..from).rev().find(actionable).unwrap_or(from)
    }
}

/// Whether row `i` can be chosen: not a divider, and not drawn faint.
#[must_use]
pub fn is_actionable(rows: &[BlockMenuRow], actions: &[BlockMenuAction], i: usize) -> bool {
    matches!(actions.get(i), Some(a) if *a != BlockMenuAction::None)
        && matches!(rows.get(i), Some(BlockMenuRow::Action { enabled: true, .. }))
}

/// The first row the keyboard should land on when the menu opens.
///
/// Not simply `0`: the first row is `Fold`, which a block with nothing to fold
/// draws faint — opening onto it and pressing Enter would do nothing at all.
#[must_use]
pub fn first_actionable(rows: &[BlockMenuRow], actions: &[BlockMenuAction]) -> usize {
    (0..actions.len()).find(|i| is_actionable(rows, actions, *i)).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zest_core::Terminal;

    /// A session with one finished command that printed, and one still running.
    fn session() -> Terminal {
        let mut t = Terminal::new(40, 8, 200);
        t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07echo hi\x1b]133;C\x07\r\n");
        t.advance(b"hi\r\n\x1b]133;D;0\x07");
        t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07make\x1b]133;C\x07\r\n");
        t.advance(b"building\r\n");
        t
    }

    fn rows_for(
        term: &Terminal,
        block: &Block,
        folded: bool,
    ) -> (Vec<BlockMenuRow>, Vec<BlockMenuAction>) {
        build_rows(term, block, folded, "\u{2318}\u{21e7}O", "\u{2318}\u{21e7}R")
    }

    fn label_of(rows: &[BlockMenuRow], i: usize) -> Option<&str> {
        match &rows[i] {
            BlockMenuRow::Action { label, .. } => Some(label),
            BlockMenuRow::Divider => None,
        }
    }

    fn enabled_label(rows: &[BlockMenuRow], label: &str) -> bool {
        rows.iter().any(
            |r| matches!(r, BlockMenuRow::Action { label: l, enabled: true, .. } if l == label),
        )
    }

    #[test]
    fn the_rows_and_their_actions_are_one_list() {
        // The property the whole module is shaped around: a click resolves by
        // index, so a row and its action drifting apart would silently run the
        // wrong verb — re-run instead of copy is not a cosmetic failure.
        let t = session();
        let (rows, actions) = rows_for(&t, &t.blocks().blocks()[0], false);
        assert_eq!(rows.len(), actions.len(), "one action per drawn row");
        for (i, action) in actions.iter().enumerate() {
            assert_eq!(
                *action == BlockMenuAction::None,
                matches!(rows[i], BlockMenuRow::Divider),
                "row {i} and its action disagree about being a divider"
            );
        }
    }

    #[test]
    fn stepping_never_lands_on_a_divider_or_a_faint_row() {
        // The keyboard must not reach what the click path refuses, or Enter
        // does nothing on a row the user deliberately moved onto.
        let mut t = Terminal::new(40, 8, 200);
        // Printed nothing and cannot fold: the first two rows are both dead.
        t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07true\x1b]133;C\x07\r\n\x1b]133;D;0\x07");
        let (rows, actions) = rows_for(&t, t.blocks().last().expect("one block"), false);

        let mut at = first_actionable(&rows, &actions);
        assert!(is_actionable(&rows, &actions, at), "the menu opens on a live row");
        for _ in 0..actions.len() * 2 {
            at = step(&rows, &actions, at, true);
            assert!(is_actionable(&rows, &actions, at), "stepping down landed on a dead row");
        }
        for _ in 0..actions.len() * 2 {
            at = step(&rows, &actions, at, false);
            assert!(is_actionable(&rows, &actions, at), "stepping up landed on a dead row");
        }
    }

    #[test]
    fn a_block_with_no_command_offers_no_re_run() {
        // A `C` marker with no `B` before it — what a partial shell
        // integration produces. Re-running would send a bare carriage return
        // and execute whatever happens to be on the prompt already.
        let mut t = Terminal::new(40, 8, 200);
        t.advance(b"\x1b]133;A\x07$ \x1b]133;C\x07\r\nout\r\n\x1b]133;D;0\x07");
        let (rows, _) = rows_for(&t, t.blocks().last().expect("one block"), false);
        assert!(!enabled_label(&rows, "Re-run"), "no command text, no re-run");
        assert!(!enabled_label(&rows, "Re-run in new tab"), "same rule in a new tab");
        assert!(enabled_label(&rows, "Copy output"), "it still printed something");
    }

    #[test]
    fn a_block_that_printed_nothing_offers_no_copy_but_still_copies_as_a_block() {
        let mut t = Terminal::new(40, 8, 200);
        t.advance(b"\x1b]133;A\x07$ \x1b]133;B\x07true\x1b]133;C\x07\r\n\x1b]133;D;0\x07");
        let (rows, _) = rows_for(&t, t.blocks().last().expect("one block"), false);
        assert!(!enabled_label(&rows, "Copy output"), "nothing printed, nothing to copy");
        assert!(!enabled_label(&rows, "Select block text"), "and nothing to select");
        assert!(
            enabled_label(&rows, "Copy command + output"),
            "the command is still worth copying — 'it printed nothing' is often the report"
        );
    }

    #[test]
    fn a_running_block_cannot_be_folded_but_can_be_copied() {
        // `fold_range` needs a finished block; copying deliberately does not,
        // because the case that matters is a build you want to paste while it
        // is still going.
        let t = session();
        let running = t.blocks().last().expect("a running block");
        let (rows, _) = rows_for(&t, running, false);
        assert!(!enabled_label(&rows, "Fold"), "a running block has no end to fold to");
        assert!(enabled_label(&rows, "Copy output"), "what it has printed so far");
    }

    #[test]
    fn a_folded_block_says_unfold() {
        let t = session();
        let (rows, _) = rows_for(&t, &t.blocks().blocks()[0], true);
        assert_eq!(label_of(&rows, 0), Some("Unfold"));
        let (rows, _) = rows_for(&t, &t.blocks().blocks()[0], false);
        assert_eq!(label_of(&rows, 0), Some("Fold"), "one affordance, two labels");
    }

    #[test]
    fn the_chords_are_drawn_where_the_verbs_they_repeat_are() {
        // The menu labels the two rows that have keyboard equivalents and no
        // others, so an absent chip means "no chord", not "we forgot".
        let t = session();
        let (rows, _) = rows_for(&t, &t.blocks().blocks()[0], false);
        let chord = |label: &str| {
            rows.iter().find_map(|r| match r {
                BlockMenuRow::Action { label: l, chord, .. } if l == label => Some(chord.clone()),
                _ => None,
            })
        };
        assert_eq!(chord("Copy output").as_deref(), Some("\u{2318}\u{21e7}O"));
        assert_eq!(chord("Re-run").as_deref(), Some("\u{2318}\u{21e7}R"));
        assert_eq!(chord("Copy command").as_deref(), Some(""), "no chord, no chip");
    }
}

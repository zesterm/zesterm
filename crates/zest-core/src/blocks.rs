//! Command blocks — where a command started, what it printed, how it ended.
//!
//! **Frozen contract, skeleton implementation.** WS-E fills in the OSC 133
//! parsing; the types are here because WS-F puts blocks on the wire and WS-G
//! renders them, and all three need to agree before any of them starts.
//!
//! # Why this lives in core rather than in the UI
//!
//! A block is not a visual grouping — it is a fact about the session that the
//! shell told us, and it has to survive being sent to a phone, folded in a
//! desktop window, and re-run from either. It is also the reason this project
//! owns its grid instead of depending on `alacritty_terminal`: blocks need a
//! side index that survives scrollback eviction and OSC handler hooks, and every
//! one of those would be a fork of an explicitly-unstable crate.
//!
//! # Why line ids and not row numbers
//!
//! A block outlives the viewport. Output scrolls, history is evicted, and the
//! window is resized — through all of it, "the command that started on line
//! 91,442" stays true. Row numbers would be stale within a second of a build
//! starting, which is exactly when someone wants to fold the output.

use alloc::string::String;
use alloc::vec::Vec;

use crate::grid::LineId;

/// A block, unique within a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct BlockId(pub u32);

/// How a command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum BlockState {
    /// The prompt is showing; nothing has been submitted.
    Prompt,
    /// Submitted, still producing output.
    Running,
    /// Finished, with the shell's exit status if it reported one.
    ///
    /// `None` is not the same as zero: a shell that emits OSC 133 without the
    /// status parameter is common, and showing a green tick for a command that
    /// actually failed is worse than showing nothing.
    Finished { exit_code: Option<i32> },
}

/// One command and its output.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Block {
    pub id: BlockId,
    /// First line of the prompt (OSC 133;A).
    pub prompt_line: LineId,
    /// First line of command output (OSC 133;C), once it starts.
    pub output_line: Option<LineId>,
    /// Last line, once the command has finished (OSC 133;D).
    pub end_line: Option<LineId>,
    pub state: BlockState,
    /// The command text, from OSC 133;B or read back from the grid.
    pub command: String,
    /// Working directory, from OSC 7.
    pub cwd: String,
    /// Wall clock at OSC 133;C, milliseconds since the Unix epoch.
    ///
    /// Caller-supplied ([`crate::Terminal::set_now_ms`]) because this crate is
    /// `no_std` and has no clock. Wall time rather than a monotonic instant
    /// because blocks cross the wire, and "2m ago" on another machine only
    /// means anything against a shared epoch. `None` when the embedder never
    /// stamped one.
    pub started_ms: Option<u64>,
    /// Wall clock at OSC 133;D, same epoch. `ended - started` is the
    /// duration a header prints; computed by readers, never stored, so a
    /// running block's payload does not change every tick.
    pub ended_ms: Option<u64>,
}

impl Block {
    /// The lines this block covers, as far as is known.
    ///
    /// A running block has no end yet, so the range is open — callers render to
    /// the bottom rather than waiting, which is what makes a long build readable
    /// while it runs instead of only afterwards.
    #[must_use]
    pub fn line_range(&self) -> (LineId, Option<LineId>) {
        (self.prompt_line, self.end_line)
    }

    /// Whether a line belongs to this block.
    #[must_use]
    pub fn contains(&self, line: LineId) -> bool {
        line >= self.prompt_line && self.end_line.is_none_or(|end| line <= end)
    }

    #[must_use]
    pub fn is_running(&self) -> bool {
        matches!(self.state, BlockState::Running)
    }

    /// Whether the command reported a non-zero status.
    ///
    /// A block that finished without reporting one is *not* failed. See
    /// [`BlockState::Finished`].
    #[must_use]
    pub fn failed(&self) -> bool {
        matches!(self.state, BlockState::Finished { exit_code: Some(c) } if c != 0)
    }
}

/// The blocks of one session, oldest first.
///
/// **Skeleton — WS-E owns the implementation.** Kept as a side table rather than
/// as cell data: a block spans thousands of lines, and storing its identity per
/// cell would be both enormous and wrong the moment a line is evicted while the
/// block is still running.
#[derive(Debug, Default, Clone)]
pub struct BlockIndex {
    blocks: Vec<Block>,
    next_id: u32,
    /// The lowest id from which this index is complete.
    ///
    /// Two different reasons a block is absent, and a client has to tell them
    /// apart. **Evicted** — its lines fell out of scrollback — is not the
    /// client's business: one keeping more history than the host should keep
    /// it, so the floor *rises* past what was evicted. **Destroyed** — a screen
    /// clear erased the rows it described — must reach the client, so the floor
    /// *falls* to the lowest id that went, and a keyframe carrying that floor
    /// says "nothing here any more" in the one case an empty list otherwise
    /// could not distinguish from "nothing changed".
    authoritative_from: u32,
}

impl BlockIndex {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    /// The block a line belongs to.
    ///
    /// Linear for now. Blocks are few — hundreds, not millions — and a binary
    /// search over a list whose last entry has an open end is easy to get subtly
    /// wrong. Revisit when a profile says to, not before.
    #[must_use]
    pub fn block_at(&self, line: LineId) -> Option<&Block> {
        self.blocks.iter().rev().find(|b| b.contains(line))
    }

    #[must_use]
    pub fn get(&self, id: BlockId) -> Option<&Block> {
        self.blocks.iter().find(|b| b.id == id)
    }

    /// The most recent block, which is the one still running if any is.
    #[must_use]
    pub fn last(&self) -> Option<&Block> {
        self.blocks.last()
    }

    /// Begin a block at a prompt (OSC 133;A).
    pub fn begin_prompt(&mut self, line: LineId, cwd: String) -> BlockId {
        let id = BlockId(self.next_id);
        self.next_id += 1;
        self.blocks.push(Block {
            id,
            prompt_line: line,
            output_line: None,
            end_line: None,
            state: BlockState::Prompt,
            command: String::new(),
            cwd,
            started_ms: None,
            ended_ms: None,
        });
        id
    }

    /// A command was submitted and output begins (OSC 133;C).
    ///
    /// `now_ms` is the embedder's wall clock (see [`Block::started_ms`]);
    /// `None` when it never provided one.
    pub fn begin_output(&mut self, line: LineId, command: String, now_ms: Option<u64>) {
        if let Some(b) = self.blocks.last_mut() {
            b.output_line = Some(line);
            b.command = command;
            b.state = BlockState::Running;
            b.started_ms = now_ms;
        }
    }

    /// A command finished (OSC 133;D).
    ///
    /// A block finishes once. The guard is not defensive tidiness: a screen
    /// clear can delete the block a `D` was about to close — pwsh's hook emits
    /// `D` from its prompt function, which runs *after* `Clear-Host` — and
    /// `last_mut` is then somebody else's block, sitting untouched in
    /// scrollback. Reopening it would move its `end_line` onto a post-clear
    /// line, and since `contains` is `line >= prompt_line && line <= end`, it
    /// would go on to claim every row printed afterwards.
    pub fn finish(&mut self, line: LineId, exit_code: Option<i32>, now_ms: Option<u64>) {
        if let Some(b) = self.blocks.last_mut() {
            if matches!(b.state, BlockState::Finished { .. }) {
                return;
            }
            b.end_line = Some(line);
            b.state = BlockState::Finished { exit_code };
            b.ended_ms = now_ms;
        }
    }

    /// Insert or replace a block a host computed.
    ///
    /// The client half of the index. A remote session's blocks are parsed on the
    /// machine the shell runs on and arrive whole, so there are no markers to
    /// replay — see [`crate::RemoteWriter`], which is the only thing that should
    /// call this.
    ///
    /// Ordering is maintained rather than assumed: [`Self::block_at`] searches
    /// from the newest end and [`Self::evict_before`] trims from the oldest, and
    /// both are wrong if a late-arriving block lands out of order.
    pub fn upsert(&mut self, block: Block) {
        match self.blocks.binary_search_by_key(&block.id, |b| b.id) {
            Ok(i) => self.blocks[i] = block,
            Err(i) => {
                // Keep the counter ahead of anything received, so a client that
                // later becomes a host — which is what a detach and a local
                // reattach amount to — cannot reissue an id.
                self.next_id = self.next_id.max(block.id.0 + 1);
                self.blocks.insert(i, block);
            }
        }
    }

    /// Re-anchor every block after a width change renumbered the lines.
    ///
    /// Reflow renumbers because rewrapping changes how many rows a logical line
    /// occupies, so a block's `prompt_line` names different text afterwards
    /// unless it is mapped. The selection is *cleared* in the same situation and
    /// this is not, deliberately: losing the block for a build because the
    /// window was widened while it ran is exactly the case blocks exist for, and
    /// a block names only a line where a selection names a column too.
    ///
    /// See [`crate::grid::Reindex`] for what the mapping can and cannot answer.
    pub fn reanchor(&mut self, reindex: &crate::grid::Reindex) {
        let Some(oldest) = reindex.oldest() else {
            // Nothing survived the rewrap, so neither does any block.
            self.blocks.clear();
            return;
        };
        self.blocks.retain_mut(|b| {
            // A block whose *end* did not survive was dropped by the scrollback
            // bound during the rewrap — the same fate as [`Self::evict_before`],
            // and the same rule, so the two cannot disagree about what a live
            // block is.
            if let Some(end) = b.end_line {
                match reindex.lookup(end) {
                    Some(new) => b.end_line = Some(new),
                    None => return false,
                }
            }
            // A block whose *start* did not survive is merely partly in history,
            // which is ordinary: you can no longer scroll to its top, and it is
            // still the block that produced the output below. Clamping to the
            // oldest surviving line is what eviction already leaves behind.
            b.prompt_line = reindex.lookup(b.prompt_line).unwrap_or(oldest);
            b.output_line = b.output_line.map(|l| reindex.lookup(l).unwrap_or(oldest));
            true
        });
    }

    /// Drop blocks whose lines have all fallen out of scrollback.
    ///
    /// Called with the oldest line still held. Without this the index grows
    /// without bound across a long-lived session, which is precisely the session
    /// that matters most in a fleet — the one that has been running for weeks.
    pub fn evict_before(&mut self, oldest_line: LineId) {
        self.blocks.retain(|b| b.end_line.is_none_or(|end| end >= oldest_line));
        // Rises: what fell out of scrollback here is not something a client
        // has to be told to forget.
        let floor = self.blocks.first().map_or(self.next_id, |b| b.id.0);
        self.authoritative_from = self.authoritative_from.max(floor);
    }

    /// Forget every block the screen just erased, keeping those wholly above it.
    ///
    /// `ESC[2J` blanks cells where RIS blanks everything, but the consequence
    /// for the index is identical: a block anchored at a line whose content is
    /// gone describes nothing. Worse than nothing, in fact — line ids survive
    /// an erase, so the shell reuses the very ids the old blocks still claim,
    /// and the header pass then paints a stale command over the live prompt.
    ///
    /// The rule is "entirely above the erased region", and `end_line` alone
    /// decides it: [`Self::finish`] clamps `end_line` to at least `prompt_line`,
    /// so a block ending below `first` began above it too. A block with no
    /// `end_line` is still open, which means it lives on the screen that was
    /// just wiped.
    ///
    /// The caller must pass the first *erased* line, and must only call this
    /// for a full-screen erase — see the note in `erase_in_display`.
    pub fn erase_screen(&mut self, first: LineId) {
        let before = self.blocks.len();
        self.blocks.retain(|b| b.end_line.is_some_and(|end| end < first));
        if self.blocks.len() == before {
            return;
        }
        // Falls to the lowest id that went. Survivors are a prefix — every
        // block below the erased region ends below it — so that is one past
        // the last survivor.
        let lowest_gone = self.blocks.last().map_or(0, |b| b.id.0 + 1);
        self.authoritative_from = self.authoritative_from.min(lowest_gone);
    }

    /// The lowest id from which this index is complete; see
    /// [`Self::authoritative_from`](BlockIndex::authoritative_from).
    #[must_use]
    pub fn authoritative_from(&self) -> u32 {
        self.authoritative_from
    }

    /// Keep only blocks older than `first`, for a keyframe about to restate the
    /// rest. See [`crate::RemoteWriter::drop_blocks_from`].
    pub fn retain_below(&mut self, first: BlockId) {
        self.blocks.retain(|b| b.id < first);
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    fn index_with_one_finished_block() -> BlockIndex {
        let mut idx = BlockIndex::new();
        idx.begin_prompt(10, "/home".into());
        idx.begin_output(11, "cargo build".into(), None);
        idx.finish(40, Some(0), None);
        idx
    }

    #[test]
    fn a_block_spans_from_its_prompt_to_its_end() {
        let idx = index_with_one_finished_block();
        let b = idx.last().expect("one block");
        assert_eq!(b.line_range(), (10, Some(40)));
        assert!(b.contains(10));
        assert!(b.contains(25));
        assert!(b.contains(40));
        assert!(!b.contains(41));
    }

    #[test]
    fn a_running_block_extends_to_wherever_output_has_reached() {
        // What makes a long build readable while it runs rather than only after
        // it finishes.
        let mut idx = BlockIndex::new();
        idx.begin_prompt(5, "/x".into());
        idx.begin_output(6, "cargo test".into(), None);
        let b = idx.last().expect("one block");
        assert!(b.is_running());
        assert!(b.contains(9_999), "a running block has no end yet");
    }

    #[test]
    fn a_missing_exit_status_is_not_success() {
        // Plenty of shells emit OSC 133 without the status parameter. Showing a
        // green tick for a command that actually failed is worse than showing
        // nothing at all.
        let mut idx = BlockIndex::new();
        idx.begin_prompt(0, String::new());
        idx.begin_output(1, "flaky".into(), None);
        idx.finish(2, None, None);
        let b = idx.last().expect("one block");
        assert!(!b.failed(), "unknown is not failure");
        assert!(!matches!(b.state, BlockState::Finished { exit_code: Some(0) }), "nor success");
    }

    #[test]
    fn a_nonzero_status_is_a_failure() {
        let mut idx = BlockIndex::new();
        idx.begin_prompt(0, String::new());
        idx.begin_output(1, "false".into(), None);
        idx.finish(2, Some(1), None);
        assert!(idx.last().expect("one block").failed());
    }

    #[test]
    fn lines_map_back_to_their_block() {
        let mut idx = index_with_one_finished_block();
        idx.begin_prompt(41, "/home".into());
        idx.begin_output(42, "ls".into(), None);
        idx.finish(45, Some(0), None);

        assert_eq!(idx.block_at(25).map(|b| b.command.as_str()), Some("cargo build"));
        assert_eq!(idx.block_at(43).map(|b| b.command.as_str()), Some("ls"));
        assert!(idx.block_at(100).is_none());
    }

    #[test]
    fn a_block_whose_start_was_evicted_by_a_rewrap_is_kept_and_clamped() {
        // Partly in history is the ordinary case, not a broken one: you can no
        // longer scroll to the block's top, and it is still the block that
        // produced the output below it. Dropping it would make widening a
        // window delete history that widening a window just made *more*
        // readable.
        let mut idx = BlockIndex::new();
        idx.begin_prompt(2, "/x".into());
        idx.begin_output(3, "cargo build".into(), None);
        idx.finish(9, Some(0), None);

        // Lines 0-2 did not survive the rewrap; 3 onwards did, renumbered down.
        let reindex = crate::grid::Reindex::from_pairs(&[(3, 0), (9, 6)]);
        idx.reanchor(&reindex);

        let b = idx.last().expect("the block survived");
        assert_eq!(b.prompt_line, 0, "clamped to the oldest line that still exists");
        assert_eq!(b.output_line, Some(0));
        assert_eq!(b.end_line, Some(6));
    }

    #[test]
    fn a_block_whose_end_did_not_survive_a_rewrap_is_dropped() {
        // The same rule as `evict_before`, so the two cannot disagree about
        // what a live block is.
        let mut idx = index_with_one_finished_block();
        idx.begin_prompt(41, "/home".into());
        idx.begin_output(42, "ls".into(), None);
        idx.finish(45, Some(0), None);

        // Only the newer block's lines came through the rewrap.
        let reindex = crate::grid::Reindex::from_pairs(&[(41, 0), (42, 1), (45, 4)]);
        idx.reanchor(&reindex);

        assert_eq!(idx.blocks().len(), 1, "the older block's end line is gone");
        assert_eq!(idx.last().expect("one block").command, "ls");
    }

    #[test]
    fn a_rewrap_that_kept_nothing_leaves_no_blocks() {
        let mut idx = index_with_one_finished_block();
        idx.reanchor(&crate::grid::Reindex::default());
        assert!(idx.blocks().is_empty(), "no surviving line means no surviving block");
    }

    #[test]
    fn evicted_blocks_are_dropped_but_running_ones_survive() {
        // A session running for weeks is the case a fleet makes normal, so an
        // index that only grows is a leak with a long fuse. A running block has
        // no end line and must not be evicted by a scrollback bound.
        let mut idx = index_with_one_finished_block();
        idx.begin_prompt(41, "/home".into());
        idx.begin_output(42, "tail -f".into(), None);

        idx.evict_before(41);
        assert_eq!(idx.blocks().len(), 1, "the finished block should be gone");
        assert!(idx.last().expect("running block").is_running());
    }
}

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
        });
        id
    }

    /// A command was submitted and output begins (OSC 133;C).
    pub fn begin_output(&mut self, line: LineId, command: String) {
        if let Some(b) = self.blocks.last_mut() {
            b.output_line = Some(line);
            b.command = command;
            b.state = BlockState::Running;
        }
    }

    /// A command finished (OSC 133;D).
    pub fn finish(&mut self, line: LineId, exit_code: Option<i32>) {
        if let Some(b) = self.blocks.last_mut() {
            b.end_line = Some(line);
            b.state = BlockState::Finished { exit_code };
        }
    }

    /// Drop blocks whose lines have all fallen out of scrollback.
    ///
    /// Called with the oldest line still held. Without this the index grows
    /// without bound across a long-lived session, which is precisely the session
    /// that matters most in a fleet — the one that has been running for weeks.
    pub fn evict_before(&mut self, oldest_line: LineId) {
        self.blocks.retain(|b| b.end_line.is_none_or(|end| end >= oldest_line));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index_with_one_finished_block() -> BlockIndex {
        let mut idx = BlockIndex::new();
        idx.begin_prompt(10, "/home".into());
        idx.begin_output(11, "cargo build".into());
        idx.finish(40, Some(0));
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
        idx.begin_output(6, "cargo test".into());
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
        idx.begin_output(1, "flaky".into());
        idx.finish(2, None);
        let b = idx.last().expect("one block");
        assert!(!b.failed(), "unknown is not failure");
        assert!(!matches!(b.state, BlockState::Finished { exit_code: Some(0) }), "nor success");
    }

    #[test]
    fn a_nonzero_status_is_a_failure() {
        let mut idx = BlockIndex::new();
        idx.begin_prompt(0, String::new());
        idx.begin_output(1, "false".into());
        idx.finish(2, Some(1));
        assert!(idx.last().expect("one block").failed());
    }

    #[test]
    fn lines_map_back_to_their_block() {
        let mut idx = index_with_one_finished_block();
        idx.begin_prompt(41, "/home".into());
        idx.begin_output(42, "ls".into());
        idx.finish(45, Some(0));

        assert_eq!(idx.block_at(25).map(|b| b.command.as_str()), Some("cargo build"));
        assert_eq!(idx.block_at(43).map(|b| b.command.as_str()), Some("ls"));
        assert!(idx.block_at(100).is_none());
    }

    #[test]
    fn evicted_blocks_are_dropped_but_running_ones_survive() {
        // A session running for weeks is the case a fleet makes normal, so an
        // index that only grows is a leak with a long fuse. A running block has
        // no end line and must not be evicted by a scrollback bound.
        let mut idx = index_with_one_finished_block();
        idx.begin_prompt(41, "/home".into());
        idx.begin_output(42, "tail -f".into());

        idx.evict_before(41);
        assert_eq!(idx.blocks().len(), 1, "the finished block should be gone");
        assert!(idx.last().expect("running block").is_running());
    }
}

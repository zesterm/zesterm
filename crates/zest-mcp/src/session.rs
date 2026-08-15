//! One attached session, as this process holds it.
//!
//! A real [`zest_core::Terminal`] with [`zest_proto::Applier`] driving it — the
//! same pair `zest-app` uses, and the reason this crate is Rust rather than a
//! second implementation. That applier is held cell-for-cell identical to the
//! host across six recorded sessions by the conformance corpus, so what an
//! agent reads through `screen` is the grid the shell actually drew rather than
//! a best-effort reconstruction of it.
//!
//! # Everything an agent reads is post-VT, and that is the whole saving
//!
//! A build with a progress bar writes the same row hundreds of times with
//! `\r`; a `cat` of a megabyte is a megabyte. The emulator has already
//! collapsed both by the time anything here looks, so `screen` is bounded by
//! the size of the grid rather than by how chatty the command was. ADR-004
//! measures the transport half of this (~1 MB of pty bytes against ~3 KB of
//! delta for `cat 1MB`); this is the other half, and they are different
//! numbers.

use zest_core::{Modes, Terminal};
use zest_proto::{Applied, Applier, BlockPayload, CursorState, Delta, Keyframe, SessionAddr};

/// The host's own reading of a terminal's cursor, mirrored.
///
/// `zest-daemon`'s `session.rs` has the same eight lines and keeps them
/// private. Duplicated rather than exported because the two are answering
/// different questions about different terminals — the host describes the
/// session it owns, this describes a replica it was handed — and a shared
/// helper would only be shared until one of them needed to differ. The one
/// thing worth carrying across is the `shape` comment: a program that asked
/// for a bar cursor with DECSCUSR must not be reported as a block.
fn cursor_of(term: &Terminal) -> CursorState {
    let c = term.cursor();
    CursorState {
        row: u16::try_from(c.row).unwrap_or(0),
        col: u16::try_from(c.col).unwrap_or(0),
        visible: term.modes().contains(Modes::SHOW_CURSOR),
        shape: u8::try_from(term.cursor_style().to_decscusr()).unwrap_or(0),
    }
}

/// How much scrollback a replica keeps.
///
/// The host keeps its own and answers `RequestScrollback` from it, so this is
/// only what can be read without asking. Generous enough that a finished
/// command's output is usually still here, small enough that a fleet of
/// followed sessions is not a memory story.
const REPLICA_SCROLLBACK: usize = 2_000;

/// A session this process is attached to.
pub struct Replica {
    pub addr: SessionAddr,
    term: Terminal,
    applier: Applier,
    /// The sequence the replica currently holds.
    seq: u64,
    /// Set once `HostMessage::Exited` arrives. `Some(None)` is "it ended and
    /// the host reported no status", which is not the same as zero.
    exited: Option<Option<i32>>,
    title: String,
}

impl Replica {
    /// Build a replica from the keyframe an attach returned.
    ///
    /// The size comes from the keyframe rather than from what was asked for:
    /// `Keyframe.cols/rows` is the sole authority on shape, and an observer
    /// asked for nothing at all.
    #[must_use]
    pub fn new(addr: SessionAddr, k: &Keyframe, seq: u64) -> Self {
        // `Terminal::new` then `apply_keyframe`, which resizes through
        // `remote()` -- the door that marks this a replica so a later grow does
        // not pull scrollback down to meet a keyframe that is about to restate
        // every row anyway (#247, ADR-013).
        let mut term = Terminal::new(
            usize::from(k.cols).max(1),
            usize::from(k.rows).max(1),
            REPLICA_SCROLLBACK,
        );
        let mut applier = Applier::new();
        applier.apply_keyframe(&mut term, k, seq);
        Self { addr, term, applier, seq, exited: None, title: String::new() }
    }

    /// Replace everything, after a `NeedsKeyframe` or a reattach.
    pub fn reset(&mut self, k: &Keyframe, seq: u64) {
        self.applier.apply_keyframe(&mut self.term, k, seq);
        self.seq = seq;
    }

    /// Apply one update. `false` means the caller must fetch a keyframe.
    #[must_use]
    pub fn apply(&mut self, d: &Delta, base: u64, seq: u64) -> bool {
        match self.applier.apply_delta(&mut self.term, d, base, seq) {
            Applied::Ok => {
                self.seq = seq;
                true
            }
            // Deliberately not "reattach": `RequestKeyframe` exists so a client
            // can recover without tearing down a subscriber, which would also
            // drop and re-cast its size vote.
            Applied::NeedsKeyframe => false,
        }
    }

    #[must_use]
    pub fn seq(&self) -> u64 {
        self.seq
    }

    #[must_use]
    pub fn size(&self) -> (usize, usize) {
        (self.term.grid().cols(), self.term.grid().rows())
    }

    #[must_use]
    pub fn cursor(&self) -> CursorState {
        cursor_of(&self.term)
    }

    #[must_use]
    pub fn alt_screen(&self) -> bool {
        self.term.in_alt_screen()
    }

    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    pub fn set_title(&mut self, title: String) {
        self.title = title;
    }

    #[must_use]
    pub fn exited(&self) -> Option<Option<i32>> {
        self.exited
    }

    pub fn set_exited(&mut self, code: Option<i32>) {
        self.exited = Some(code);
    }

    /// The visible screen as text, one row per line.
    ///
    /// Trailing blank rows are dropped: an 80x24 shell showing a prompt is two
    /// lines of content and twenty-two of nothing, and the nothing is pure cost
    /// on every single call. Trailing *spaces* within a row go too, which is
    /// what `Grid::screen_text` already does.
    #[must_use]
    pub fn screen_text(&self) -> String {
        let full = self.term.screen_text();
        let trimmed: Vec<&str> = full.lines().collect();
        let end = trimmed.iter().rposition(|l| !l.trim().is_empty()).map_or(0, |i| i + 1);
        trimmed[..end].join("\n")
    }

    /// Every block the replica holds, oldest first.
    #[must_use]
    pub fn blocks(&self) -> Vec<BlockPayload> {
        self.term.blocks().blocks().iter().map(BlockPayload::from_block).collect()
    }

    /// The id below which this replica's block list is not authoritative.
    #[must_use]
    pub fn blocks_from(&self) -> u32 {
        self.term.blocks().authoritative_from()
    }

    /// The rows of one block, by the line ids it delimits.
    ///
    /// `None` when the block is unknown. An empty vec means the block is known
    /// and its rows are no longer held here -- the caller should ask the host
    /// with `RequestScrollback`, which is answered from its memory and then
    /// from its sqlite history, so a long build's output outlives the ring.
    /// The two must stay distinguishable: collapsing them makes an agent retry
    /// a block that will never exist.
    ///
    /// **Scrollback *and* the viewport, in that order.** `Grid::lines_by_id` is
    /// deliberately scrollback-only -- it exists for the protocol's paged fetch,
    /// and including the viewport once made clients prepend rows still on
    /// screen to their own history, drawing the screen duplicated above itself.
    /// A *block* has no such rule: the command that just ran is the one an
    /// agent asks about most, and every one of its rows is on screen. So this
    /// walks both and lets the id range decide.
    #[must_use]
    pub fn block_rows(&self, id: u32) -> Option<Vec<String>> {
        let block = self.term.blocks().get(zest_core::BlockId(id))?;
        // Output, not the prompt: an agent asking what a command printed does
        // not want the prompt line, and `Cmd+Shift+O` already copies output
        // alone in the app for the same reason.
        let from = block.output_line.unwrap_or(block.prompt_line);
        let to = block.end_line;
        let grid = self.term.grid();

        let wanted = |id: zest_core::LineId| id >= from && to.is_none_or(|end| id <= end);

        let mut out: Vec<String> = grid
            .lines_by_id(from, usize::MAX)
            .into_iter()
            .filter(|r| wanted(r.id))
            .map(|r| r.text().trim_end().to_string())
            .collect();

        for i in 0..grid.rows() {
            let row = grid.row(i);
            if wanted(row.id) {
                out.push(row.text().trim_end().to_string());
            }
        }

        // A running command's last rows are usually blank -- the screen below
        // where it has got to. Trailing blanks are cost, not information; a
        // blank *between* two lines of output is real and stays.
        let end = out.iter().rposition(|l| !l.is_empty()).map_or(0, |i| i + 1);
        out.truncate(end);
        Some(out)
    }
}

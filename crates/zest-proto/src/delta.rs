//! Grid deltas.
//!
//! Three encoding decisions carry most of the win, and all three are shapes
//! rather than optimizations — they cannot be added later without changing the
//! wire:
//!
//! **Attribute interning.** A syntax-highlighted row has ~50 style changes and
//! perhaps 8 distinct styles. [`AttrDef`] names each style once per session;
//! [`Run`] refers to it by [`AttrId`]. Inline styles would put the colours on
//! the wire fifty times per row.
//!
//! **`Scroll` before `Row`.** A `tail -f` line is one 7-byte [`DeltaOp::Scroll`]
//! plus one [`DeltaOp::Row`], instead of a full repaint. Ordering matters: the
//! scroll must be applied first or the row indices refer to the old grid.
//!
//! **Runs carry an explicit cell count.** Three renderers — wgpu, the browser,
//! Lynx — will not agree on `wcwidth` for every emoji and combining sequence,
//! and a client that computes width itself will drift from the host's grid in a
//! way that is nearly impossible to diagnose. The host has already decided how
//! many cells the text occupies; it says so, and clients obey.

use serde::{Deserialize, Serialize};
use zest_core::{CellFlags, Color};

/// An interned attribute, scoped to one session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct AttrId(pub u16);

/// The style an [`AttrId`] stands for.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct AttrDef {
    pub id: AttrId,
    /// Unresolved, exactly as the grid holds it.
    ///
    /// `Color::Indexed(1)` stays indexed all the way to the client, so the same
    /// session can render under a dark theme on the desktop and a light one on
    /// the phone at the same time. Resolving host-side would make the theme a
    /// property of the session. → ADR-002.
    pub fg: Color,
    pub bg: Color,
    /// Bold, italic, underline, inverse and the rest, as raw bits.
    ///
    /// Pinned to an integer rather than taking `bitflags`' own representation,
    /// which is a names-string (`"BOLD | ITALIC"`) in human-readable formats and
    /// bits otherwise. A wire type that changes shape with the encoding is a
    /// trap for three client implementations, and the names would have to be
    /// kept in sync by hand in each of them.
    #[serde(with = "flags_as_bits")]
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub flags: CellFlags,
}

mod flags_as_bits {
    use super::CellFlags;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(flags: &CellFlags, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_u16(flags.bits())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<CellFlags, D::Error> {
        // Unknown bits are dropped rather than rejected. A newer host that has
        // learned an attribute this client does not know about should render
        // slightly plainer here, not fail to render at all.
        Ok(CellFlags::from_bits_truncate(u16::deserialize(d)?))
    }
}

/// A stretch of cells sharing one attribute.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Run {
    pub attr: AttrId,
    /// How many grid cells this run occupies.
    ///
    /// Not derivable from `text`: a CJK character is one `char` and two cells, a
    /// ZWJ family emoji is seven codepoints and two cells. The host knows;
    /// clients must not recompute. See the module docs.
    pub cells: u16,
    pub text: String,
    /// Combining marks, for the cells in this run that have any.
    ///
    /// A separate field rather than more characters in `text`, and that is the
    /// whole point: `text` stays exactly one `char` per cell, so a client never
    /// has to know which codepoints combine. Appending marks inline would make
    /// every decoder carry a Unicode table and get the grouping right, which is
    /// the same class of mistake `Run::cells` exists to prevent for widths.
    ///
    /// Additive, so a peer that predates it decodes fine and simply renders
    /// `e` where `é` was intended — which is what every peer did before this
    /// existed.
    ///
    /// `ts(optional)` because `skip_serializing_if` means a run with no marks
    /// has no key at all on the wire — the common case, permanently, not just
    /// for old peers — and a required field in the binding sent a decoder
    /// reading `undefined` where it believed an array (#15). The `ts(as)`
    /// detour exists only because `optional` insists on an `Option`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    #[cfg_attr(feature = "ts", ts(as = "Option<Vec<CellMarks>>", optional))]
    pub marks: Vec<CellMarks>,
}

/// Combining marks for one cell of a [`Run`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct CellMarks {
    /// Offset of the cell within its run, in cells.
    pub at: u16,
    /// The marks themselves, in the order they were applied.
    pub marks: String,
}

/// One row, as runs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct RowPayload {
    /// Absolute line id — stable across scrolling and scrollback eviction.
    ///
    /// Absolute rather than viewport-relative so a selection, a command block,
    /// and a scrollback request all name the same thing on every client even as
    /// output pushes the viewport along.
    ///
    /// `number`, not the `bigint` ts-rs would derive: `rmp-serde` writes the
    /// narrowest integer that fits, so every realistic id reaches JavaScript
    /// as a plain `number` — and the one id outside ±2^53 a host actually
    /// sends, the `i64::MIN` a blank row is padded with, is a power of two
    /// and therefore exact as a double (#14).
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub line: i64,
    pub runs: Vec<Run>,
    /// The line continues onto the next one rather than ending.
    ///
    /// Carried so a client can copy a wrapped line without a spurious newline,
    /// and so reflow becomes possible later without a wire change.
    pub wrapped: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct CursorState {
    pub row: u16,
    pub col: u16,
    pub visible: bool,
    /// DECSCUSR shape, as the program asked for it.
    pub shape: u8,
}

/// One change to the grid.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum DeltaOp {
    /// Rows moved. Emitted **before** any `Row` in the same delta.
    Scroll { top: u16, bottom: u16, lines: i16 },
    /// A row's new content.
    Row { row: u16, payload: RowPayload },
    /// A rectangle cleared to an attribute.
    Erase { top: u16, left: u16, bottom: u16, right: u16, attr: AttrId },
    Cursor { cursor: CursorState },
    /// A line left the viewport into scrollback.
    ///
    /// Sent separately from `Scroll` because a client keeps history the host may
    /// have already evicted, and needs to know what to append rather than
    /// inferring it from a scroll it might have coalesced away.
    SbPush { payload: RowPayload },
    /// The program entered or left the alternate screen.
    ///
    /// The phone client switches between blocks view and grid view on this.
    ///
    /// Redundant with the `ALT_SCREEN` bit in [`DeltaOp::Modes`], and kept
    /// anyway: it is the one signal a client acts on structurally rather than
    /// by consulting a bitmask, and it is worth naming. The two cannot disagree
    /// because the encoder derives both from a single `Modes` value in a single
    /// batch — there is one producer, not two.
    AltScreen { active: bool },
    /// The window title changed (OSC 0/2).
    Title { title: String },
    /// The terminal's mode bits changed.
    ///
    /// `zest_core::Modes::bits()`, truncated back with `from_bits_truncate` on
    /// receipt for the same reason [`AttrDef::flags`] is: a newer host that has
    /// learned a mode should leave this client slightly less capable, not
    /// unable to decode.
    ///
    /// # Why this is on the wire at all
    ///
    /// A client encodes its own keystrokes — that is what
    /// [`ClientMessage::Input`](crate::ClientMessage) carries — and it cannot do
    /// that correctly without the host's modes. `APP_CURSOR` decides whether an
    /// arrow key is `ESC [ A` or `ESC O A`, and getting it wrong breaks vim and
    /// readline. `BRACKETED_PASTE` decides whether a paste is wrapped.
    /// `MOUSE_*` decide whether a click is reported at all.
    ///
    /// Without this op an attached client has no mouse reporting, no bracketed
    /// paste and broken arrow keys in every full-screen program — which is to
    /// say tmux, htop and vim are all on the far side of it.
    Modes { bits: u32 },
}

/// How a long job is going, as `OSC 9;4` reports it.
///
/// Mirrors `zest_core::Progress` for [`BlockState`]'s reason: this side
/// carries a `ts_rs` derive, and `zest-core` has to keep building for `wasm32`
/// without one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum Progress {
    /// Nothing to show — cleared, or never reported.
    #[default]
    None,
    /// A percentage, and what flavour of one. `error` and `warning` carry a
    /// number too: a build that failed at 80% is at 80%.
    At { percent: u8, state: ProgressState },
    /// Busy, with no idea how far along.
    Indeterminate,
}

/// The flavour of a determinate [`Progress`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum ProgressState {
    Normal,
    Error,
    Warning,
}

impl From<zest_core::Progress> for Progress {
    fn from(p: zest_core::Progress) -> Self {
        match p {
            zest_core::Progress::None => Self::None,
            zest_core::Progress::Indeterminate => Self::Indeterminate,
            zest_core::Progress::At { percent, state } => {
                Self::At { percent, state: state.into() }
            }
        }
    }
}

impl From<zest_core::ProgressState> for ProgressState {
    fn from(s: zest_core::ProgressState) -> Self {
        match s {
            zest_core::ProgressState::Normal => Self::Normal,
            zest_core::ProgressState::Error => Self::Error,
            zest_core::ProgressState::Warning => Self::Warning,
        }
    }
}

impl From<Progress> for zest_core::Progress {
    fn from(p: Progress) -> Self {
        match p {
            Progress::None => Self::None,
            Progress::Indeterminate => Self::Indeterminate,
            Progress::At { percent, state } => Self::At { percent, state: state.into() },
        }
    }
}

impl From<ProgressState> for zest_core::ProgressState {
    fn from(s: ProgressState) -> Self {
        match s {
            ProgressState::Normal => Self::Normal,
            ProgressState::Error => Self::Error,
            ProgressState::Warning => Self::Warning,
        }
    }
}

/// How a command ended.
///
/// Mirrors `zest_core::BlockState` rather than reusing it, for the same reason
/// [`RowPayload`] mirrors a `Row`: the wire type carries a `ts_rs` derive, and
/// `zest-core` has to keep building for `wasm32` with no such dependency.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub enum BlockState {
    /// The prompt is showing; nothing has been submitted.
    Prompt,
    /// Submitted, still producing output.
    Running,
    /// Finished, with the shell's exit status if it reported one.
    ///
    /// `None` is not zero. A shell that emits OSC 133 D without the status
    /// parameter is common, and a client that renders a green tick for a
    /// command that actually failed is worse than one that renders nothing.
    Finished { exit_code: Option<i32> },
}

/// One command and its output, as the host has it.
///
/// Line ids are `i64` to match [`RowPayload::line`] — a client compares the two
/// to decide which rows a block covers, and two integer types that must agree
/// is a conversion bug waiting for a session long enough to matter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct BlockPayload {
    /// Stable for the life of the session; the key an upsert replaces on.
    pub id: u32,
    /// First line of the prompt (OSC 133;A).
    ///
    /// `ts(type = "number")` for [`RowPayload::line`]'s reason, on all three
    /// line fields — they are compared against it, and two integer types that
    /// must agree is a conversion bug waiting to happen.
    #[cfg_attr(feature = "ts", ts(type = "number"))]
    pub prompt_line: i64,
    /// First line of command output (OSC 133;C), once it starts.
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub output_line: Option<i64>,
    /// Last line, once the command has finished (OSC 133;D).
    ///
    /// `None` while it runs, and a client must render such a block to the
    /// bottom rather than waiting — that is what makes a long build readable
    /// while it happens instead of only afterwards.
    #[cfg_attr(feature = "ts", ts(type = "number | null"))]
    pub end_line: Option<i64>,
    pub state: BlockState,
    pub command: String,
    pub cwd: String,
    /// Wall clock at OSC 133;C, milliseconds since the Unix epoch. Additive
    /// (`serde(default)`), so an old host simply sends blocks without times.
    ///
    /// A *start* stamp, never a live elapsed: `diff_blocks` resends a block
    /// whenever any field changes, and an elapsed field would resend every
    /// running block on every tick. `f64` on the TypeScript side would also be
    /// tempting and wrong — see the `rmp-serde` integer-width trap; epoch
    /// millis stay under 2^53 for the next 285,000 years, so `number` holds.
    /// (`ts(type = "number")`, not `"number | null"`: `skip_serializing_if =
    /// "Option::is_none"` means `None` travels as an absent key, never as
    /// `null` — the binding was overstating the wire, #15's class of bug.)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(type = "number", optional))]
    pub started_ms: Option<u64>,
    /// Wall clock at OSC 133;D, same epoch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(feature = "ts", ts(type = "number", optional))]
    pub ended_ms: Option<u64>,
}

impl BlockPayload {
    /// The wire form of a block the host parsed.
    #[must_use]
    pub fn from_block(b: &zest_core::Block) -> Self {
        // Saturating rather than wrapping, matching `RowPayload::line`. A
        // session would have to print 9.2 quintillion lines to reach it, and a
        // block clamped to the end of time is at least ordered correctly.
        let line = |l: zest_core::LineId| i64::try_from(l).unwrap_or(i64::MAX);
        Self {
            id: b.id.0,
            prompt_line: line(b.prompt_line),
            output_line: b.output_line.map(line),
            end_line: b.end_line.map(line),
            state: match b.state {
                zest_core::BlockState::Prompt => BlockState::Prompt,
                zest_core::BlockState::Running => BlockState::Running,
                zest_core::BlockState::Finished { exit_code } => {
                    BlockState::Finished { exit_code }
                }
            },
            command: b.command.clone(),
            cwd: b.cwd.clone(),
            started_ms: b.started_ms,
            ended_ms: b.ended_ms,
        }
    }

    /// Back into the shape a `Terminal` holds.
    #[must_use]
    pub fn to_block(&self) -> zest_core::Block {
        let line = |l: i64| zest_core::LineId::try_from(l).unwrap_or(0);
        zest_core::Block {
            id: zest_core::BlockId(self.id),
            prompt_line: line(self.prompt_line),
            output_line: self.output_line.map(line),
            end_line: self.end_line.map(line),
            state: match self.state {
                BlockState::Prompt => zest_core::BlockState::Prompt,
                BlockState::Running => zest_core::BlockState::Running,
                BlockState::Finished { exit_code } => {
                    zest_core::BlockState::Finished { exit_code }
                }
            },
            command: self.command.clone(),
            cwd: self.cwd.clone(),
            started_ms: self.started_ms,
            ended_ms: self.ended_ms,
        }
    }
}

/// A batch applied atomically.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export))]
pub struct Delta {
    /// Attributes first used in this batch. Cumulative for the session.
    pub attrs: Vec<AttrDef>,
    pub ops: Vec<DeltaOp>,
    /// Blocks that changed in this batch, to be inserted or replaced by `id`.
    ///
    /// # Why a field and not a `DeltaOp`
    ///
    /// `DeltaOp` is `#[serde(tag = "op")]`, so a new variant is **not**
    /// additive: a peer that predates it fails to decode the whole [`Delta`],
    /// not just the op it does not know. A field with `serde(default)` is what
    /// `Run::marks` and `Keyframe::modes` already are, and it decodes
    /// everywhere.
    ///
    /// It is also the honest shape. The ops are an ordered sequence with two
    /// asserted orderings ([`Delta::scrolls_come_first`],
    /// [`Delta::screen_switch_comes_first`]); block upserts are keyed and
    /// order-independent. They are applied *after* the ops because they name
    /// absolute line ids that the rows in the same batch establish.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocks: Vec<BlockPayload>,
}

impl Delta {
    /// Whether the ops are in an order a client can apply directly.
    ///
    /// Applying a `Row` before a `Scroll` in the same batch writes to the row
    /// the scroll is about to move, so the grid ends up subtly wrong in a way
    /// that only shows under `tail -f`. Asserted in tests rather than sorted at
    /// runtime, because a producer emitting them out of order has a bug that
    /// silently reordering would hide.
    #[must_use]
    pub fn scrolls_come_first(&self) -> bool {
        let last_scroll = self.ops.iter().rposition(|o| matches!(o, DeltaOp::Scroll { .. }));
        let first_row = self.ops.iter().position(|o| matches!(o, DeltaOp::Row { .. }));
        match (last_scroll, first_row) {
            (Some(s), Some(r)) => s < r,
            _ => true,
        }
    }

    /// Whether a screen switch in this batch precedes the rows describing it.
    ///
    /// The same class of ordering bug as [`Self::scrolls_come_first`], and one
    /// that hid for longer because it is invisible to a client that keeps a
    /// flat list of rows. A client applying into a real grid has *two* grids —
    /// primary and alternate — and `AltScreen`/`Modes` chooses which one the
    /// following `Row` ops land in.
    ///
    /// Emit them in the wrong order and the rows describing the alternate
    /// screen are written into the primary grid, after which the alternate grid
    /// is created blank: starting `vim` shows an empty screen, and leaving it
    /// shows scrollback with vim's first frame stamped into it.
    ///
    /// Asserted rather than sorted, for the reason above: silently reordering
    /// would hide a producer bug.
    #[must_use]
    pub fn screen_switch_comes_first(&self) -> bool {
        let last_switch = self.ops.iter().rposition(|o| {
            matches!(o, DeltaOp::AltScreen { .. } | DeltaOp::Modes { .. })
        });
        let first_row = self
            .ops
            .iter()
            .position(|o| matches!(o, DeltaOp::Row { .. } | DeltaOp::SbPush { .. }));
        match (last_switch, first_row) {
            (Some(s), Some(r)) => s < r,
            _ => true,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(line: i64) -> RowPayload {
        RowPayload {
            line,
            runs: vec![Run { attr: AttrId(0), cells: 5, text: "hello".to_string(), marks: Vec::new() }],
            wrapped: false,
        }
    }

    #[test]
    fn a_run_states_its_cell_count_independently_of_its_text() {
        // The property that keeps three renderers agreeing. Two CJK characters
        // are two chars and four cells; a client that counted chars would draw
        // everything after them in the wrong column.
        let run = Run { attr: AttrId(0), cells: 4, text: "世界".to_string(), marks: Vec::new() };
        assert_eq!(run.text.chars().count(), 2);
        assert_eq!(run.cells, 4, "the host's cell count is the truth");
    }

    #[test]
    fn scroll_before_row_is_detectable() {
        let good = Delta {
            blocks: Vec::new(),
            attrs: vec![],
            ops: vec![
                DeltaOp::Scroll { top: 0, bottom: 23, lines: 1 },
                DeltaOp::Row { row: 23, payload: row(100) },
            ],
        };
        assert!(good.scrolls_come_first());

        let bad = Delta {
            blocks: Vec::new(),
            attrs: vec![],
            ops: vec![
                DeltaOp::Row { row: 23, payload: row(100) },
                DeltaOp::Scroll { top: 0, bottom: 23, lines: 1 },
            ],
        };
        assert!(!bad.scrolls_come_first(), "a row written before the scroll that moves it");
    }

    #[test]
    fn a_delta_with_no_scroll_is_trivially_ordered() {
        let d = Delta { blocks: Vec::new(), attrs: vec![], ops: vec![DeltaOp::Row { row: 0, payload: row(1) }] };
        assert!(d.scrolls_come_first());
    }

    #[test]
    fn colours_stay_unresolved_on_the_wire() {
        // Resolving host-side would make the theme a property of the session,
        // so the desktop and the phone could not render it differently.
        let def = AttrDef {
            id: AttrId(1),
            fg: Color::Indexed(4),
            bg: Color::Default,
            flags: CellFlags::BOLD,
        };
        let json = serde_json::to_string(&def).expect("serialize");
        let back: AttrDef = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.fg, Color::Indexed(4), "an index was resolved to rgb");
        assert_eq!(back.bg, Color::Default);
    }

    #[test]
    fn flags_are_an_integer_in_every_encoding() {
        let def = AttrDef {
            id: AttrId(1),
            fg: Color::Default,
            bg: Color::Default,
            flags: CellFlags::BOLD | CellFlags::ITALIC,
        };
        let json = serde_json::to_string(&def).expect("serialize");
        let expected = (CellFlags::BOLD | CellFlags::ITALIC).bits();
        assert!(json.contains(&format!("\"flags\":{expected}")), "{json}");

        let back: AttrDef = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.flags, def.flags);
    }

    #[test]
    fn an_unknown_attribute_bit_renders_plainer_rather_than_failing() {
        // A newer host may know an attribute this client does not. Dropping the
        // bit degrades the look; rejecting the message loses the whole frame.
        let json = r#"{"id":1,"fg":"Default","bg":"Default","flags":65535}"#;
        let parsed: AttrDef = serde_json::from_str(json).expect("tolerated");
        assert!(parsed.flags.contains(CellFlags::BOLD));
    }

    #[test]
    fn rows_are_addressed_by_absolute_line_id() {
        // Viewport-relative ids would make a scrollback request and a selection
        // mean different things on different clients as output arrives.
        let payload = row(-1);
        let json = serde_json::to_string(&payload).expect("serialize");
        assert!(json.contains("\"line\":-1"), "{json}");
    }
}

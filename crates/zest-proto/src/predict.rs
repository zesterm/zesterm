//! Predicted echo: the glyph a keystroke will produce, drawn before the host
//! says so.
//!
//! Over the relay a keystroke round-trips ~60–120 ms before its echo comes
//! back as a [`Delta`]. mosh's answer is to guess — a printable key almost
//! always lands as itself at the cursor — and to draw the guess *provisionally*
//! until the host's state either confirms or contradicts it. This module is
//! that guess, and the rules for taking it back. → ADR-016.
//!
//! # What this is not
//!
//! **Not a second VT emulator.** It handles printable characters and a
//! Backspace over its own predictions, and nothing else; an escape sequence
//! is never interpreted here. ADR-004's "two emulators means two truths" holds,
//! and so does the property that a client never parses attacker-authored bytes.
//!
//! **Not a writer into the grid.** A prediction is an overlay the renderer
//! draws on top of cells — the same seam the IME preedit already uses, for the
//! same reason: the grid is shared with the host and every other attached
//! device, and a guess made by the keyboard in front of one person must never
//! reach anyone else's scrollback, block index or `zest-mcp screen`.
//!
//! **Not wired.** Nothing on the wire says "this delta reflects input up to
//! N"; the engine reconciles from the delta's own content — a `Row` op carries
//! the whole row, a `Cursor` op says where the host has got to — plus a clock.
//! Two independent ports (this one and `clients/web/packages/proto/src/
//! predict.ts`) are held equal by `fixtures/predict.json`.
//!
//! # The rules
//!
//! - **Predict** a printable at the host's cursor advanced past every pending
//!   prediction; stop at the right edge rather than guess at the shell's
//!   wrapping. Backspace pops the newest pending prediction and predicts
//!   nothing otherwise. Every other key flushes: the line is about to be the
//!   shell's business. So does a printable whose *width* only the host knows
//!   — a client never computes cell widths (the rule `Run::cells` exists
//!   for), so a CJK glyph, an emoji or a combining mark makes no guess and
//!   ends the guessing until the host's cursor says where the line got to.
//! - **Confirm** when the host's cursor has moved past the predicted cell and
//!   the row delivered in the same delta holds the character — or, with no row
//!   delivered, when the cursor alone has passed it (the host coalesced the
//!   row into a state this client already held).
//! - **Refute** when a delivered row holds something else where the cursor has
//!   passed, or when a prediction outlives three measured round trips — a
//!   row the cursor has *not* reached says nothing, because the host may not
//!   have processed that key yet, and nothing on the wire distinguishes "not
//!   yet" from "never" (the case a wire `echo` seq would close). One refutation
//!   flushes everything and goes *quiet*: predictions are still tracked (the
//!   latency is still measured) but not shown, until one confirms. This is the
//!   `Password:` prompt — a shell that is not echoing stays not-echoing until
//!   the next line, and the next line proves itself by echoing.
//! - **Show** only once the measured echo latency is worth hiding — loopback
//!   and LAN must never flicker a dim glyph a millisecond before the real
//!   one. Before any measurement the caller's hint decides (a relayed host is
//!   worth predicting on sight).

use crate::delta::{CursorState, Delta, DeltaOp, RowPayload};

/// A keystroke, as the caller knows it *before* encoding.
///
/// Classified here rather than recovered from the bytes: `key::encode` is a
/// frozen contract with two ports, and un-encoding would be a third reading
/// of the same table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    /// A character that a line editor echoes as itself. No Ctrl/Alt, no chord.
    Printable(char),
    Backspace,
    /// Anything else — Enter, an arrow, a control character, a chord.
    Other,
}

/// Whether predictions are made and shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Policy {
    /// Show once the link measures slow enough to be worth it.
    #[default]
    Auto,
    /// Show whenever the engine is confident, whatever the latency.
    Always,
    /// Track nothing.
    Off,
}

/// One predicted cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Prediction {
    pub row: u16,
    pub col: u16,
    pub ch: char,
    /// When the key was pressed, on the caller's clock, for latency.
    made_at: u64,
}

/// Latency above which a prediction is worth showing, and below which it
/// stops being. Hysteresis so a link hovering at the threshold does not
/// blink the overlay on and off.
const SHOW_ABOVE_MS: f32 = 40.0;
const HIDE_BELOW_MS: f32 = 20.0;
/// A prediction older than this many measured round trips is wrong: the host
/// had every chance to echo it.
const EXPIRE_AFTER_RTTS: f32 = 3.0;
/// Floor on that deadline: a 0.4 ms LAN link measured three times over is
/// still inside scheduler jitter, and a refutation there would silence a
/// working line.
const EXPIRE_FLOOR_MS: f32 = 100.0;
/// Before any measurement exists, how long a prediction may stay pending.
const EXPIRE_UNMEASURED_MS: u64 = 1000;
/// Weight of a new sample in the latency estimate.
const EWMA: f32 = 0.3;

#[derive(Debug, Default)]
pub struct Predictor {
    policy: Policy,
    pending: Vec<Prediction>,
    /// Where the host's cursor is, per the last keyframe or `Cursor` op.
    cursor: CursorState,
    cols: u16,
    alt_screen: bool,
    /// A refutation happened and no prediction has confirmed since.
    quiet: bool,
    /// Measured press-to-echo latency, EWMA. `None` until the first confirm.
    latency_ms: Option<f32>,
    /// The caller's belief before any measurement: is this link slow?
    remote_hint: bool,
    /// Latched by the hysteresis above.
    showing: bool,
}

impl Predictor {
    pub fn new(policy: Policy) -> Self {
        Self {
            policy,
            ..Self::default()
        }
    }

    pub fn policy(&self) -> Policy {
        self.policy
    }

    /// Before a measurement exists, whether to show on faith.
    pub fn set_remote_hint(&mut self, remote: bool) {
        self.remote_hint = remote;
    }

    /// A keyframe replaced the whole state. Every guess is void.
    pub fn on_keyframe(&mut self, cursor: CursorState, cols: u16, alt_screen: bool) {
        self.pending.clear();
        self.cursor = cursor;
        self.cols = cols;
        self.alt_screen = alt_screen;
    }

    /// A keystroke is about to be sent.
    pub fn on_input(&mut self, key: Key, now_ms: u64) {
        if self.policy == Policy::Off {
            return;
        }
        match key {
            Key::Printable(ch) if !narrow(ch) => {
                // One cell or two is the host's call, and a guess placed
                // after a wrong answer lands in the spacer and refutes a
                // correct line. Stop guessing instead.
                self.pending.clear();
            }
            Key::Printable(ch) => {
                // A full-screen program decides for itself what a key does;
                // guessing "it echoes" is wrong for every one of them.
                if self.alt_screen {
                    return;
                }
                let (row, col) = match self.pending.last() {
                    Some(p) => (p.row, p.col + 1),
                    None => (self.cursor.row, self.cursor.col),
                };
                if col >= self.cols {
                    // Where the next glyph goes is the shell's wrapping rule,
                    // not ours.
                    return;
                }
                self.pending.push(Prediction { row, col, ch, made_at: now_ms });
            }
            Key::Backspace => {
                // Only our own guesses; a real cell is the host's to erase.
                self.pending.pop();
            }
            Key::Other => self.pending.clear(),
        }
    }

    /// A delta was applied. Judge every pending prediction against it.
    pub fn reconcile(&mut self, delta: &Delta, now_ms: u64) {
        let mut cursor_moved = false;
        let mut rows: Vec<(u16, &RowPayload)> = Vec::new();
        for op in &delta.ops {
            match op {
                DeltaOp::Cursor { cursor } => {
                    self.cursor = *cursor;
                    cursor_moved = true;
                }
                DeltaOp::Row { row, payload } => rows.push((*row, payload)),
                DeltaOp::AltScreen { active } => {
                    self.alt_screen = *active;
                    self.pending.clear();
                }
                // The line a guess sat on has moved or been cleared; the
                // guess has nothing to stand on.
                DeltaOp::Scroll { .. } | DeltaOp::Erase { .. } => self.pending.clear(),
                _ => {}
            }
        }

        let cursor = self.cursor;
        let mut i = 0;
        while i < self.pending.len() {
            let p = self.pending[i];
            let delivered = rows.iter().find(|(r, _)| *r == p.row).map(|(_, pl)| *pl);
            let passed = cursor.row != p.row || cursor.col > p.col;
            let verdict = match (delivered, passed) {
                // The host has written past this cell: the row says what it is.
                (Some(payload), true) => Some(char_at(payload, p.col) == p.ch),
                // The row arrived but the cursor has not reached the cell:
                // the host may simply not have processed that key yet. Typing
                // `ab` fast lands `a`'s echo with `b` still in flight, and
                // refuting `b` here would flush a correct guess. Ambiguous
                // silence is what expiry is for.
                (Some(_), false) => None,
                // Coalesced: the cursor passed it but the row rode an earlier
                // state. Trust the cursor.
                (None, true) if cursor_moved => Some(true),
                (None, _) => None,
            };
            match verdict {
                Some(true) => {
                    self.confirm(now_ms.saturating_sub(p.made_at));
                    self.pending.remove(i);
                }
                Some(false) => {
                    self.refute();
                    return;
                }
                None => i += 1,
            }
        }
        self.expire(now_ms);
    }

    /// Time passed with nothing arriving. Drops what has waited too long.
    pub fn tick(&mut self, now_ms: u64) {
        self.expire(now_ms);
    }

    fn expire(&mut self, now_ms: u64) {
        let Some(oldest) = self.pending.first() else { return };
        let age = now_ms.saturating_sub(oldest.made_at);
        match self.latency_ms {
            // A link we have measured had its chances. Treat the silence as a
            // shell that is not echoing, which is what it is at `Password:`.
            Some(rtt) if age as f32 > (rtt * EXPIRE_AFTER_RTTS).max(EXPIRE_FLOOR_MS) => {
                self.refute();
            }
            None if age > EXPIRE_UNMEASURED_MS => self.pending.clear(),
            _ => {}
        }
    }

    fn confirm(&mut self, sample_ms: u64) {
        let s = sample_ms as f32;
        let rtt = match self.latency_ms {
            Some(prev) => prev + (s - prev) * EWMA,
            None => s,
        };
        self.latency_ms = Some(rtt);
        self.quiet = false;
        if rtt > SHOW_ABOVE_MS {
            self.showing = true;
        } else if rtt < HIDE_BELOW_MS {
            self.showing = false;
        }
    }

    fn refute(&mut self) {
        self.pending.clear();
        self.quiet = true;
    }

    /// Measured press-to-echo latency, once something has echoed.
    pub fn echo_latency_ms(&self) -> Option<f32> {
        self.latency_ms
    }

    /// Whether the overlay should be drawn at all right now.
    pub fn showing(&self) -> bool {
        match self.policy {
            Policy::Off => false,
            _ if self.quiet => false,
            Policy::Always => true,
            Policy::Auto => match self.latency_ms {
                Some(_) => self.showing,
                None => self.remote_hint,
            },
        }
    }

    /// The cells to draw: empty unless [`Self::showing`].
    pub fn overlay(&self) -> &[Prediction] {
        if self.showing() {
            &self.pending
        } else {
            &[]
        }
    }

    /// Where the caret belongs while predictions are pending: after the last
    /// one, so the line reads as the user typed it.
    pub fn caret(&self) -> Option<(u16, u16)> {
        if !self.showing() {
            return None;
        }
        self.pending.last().map(|p| (p.row, p.col + 1))
    }

    /// Everything pending, shown or not. For tests and for the caller that
    /// wants the latency measured without an overlay.
    pub fn pending(&self) -> &[Prediction] {
        &self.pending
    }
}

/// A character this engine will vouch for occupying exactly one cell.
///
/// Not a width table — a client must never carry one (ADR-004; `Run::cells`).
/// Everything below U+1100 is one cell in every East Asian Width revision,
/// except the combining marks, which are zero; the first wide range begins at
/// U+1100 (Hangul Jamo). Anything above is *unknown here*, not wide.
fn narrow(ch: char) -> bool {
    let c = ch as u32;
    (0x20..0x1100).contains(&c) && !(0x0300..=0x036F).contains(&c) && c != 0x7F
}

/// The character a row payload puts at `col`: one per cell, a space once a
/// run's text is exhausted — `Applier::fill`'s rule, restated for one cell.
fn char_at(row: &RowPayload, col: u16) -> char {
    let mut at = 0u16;
    for run in &row.runs {
        let end = at + run.cells;
        if col < end {
            return run.text.chars().nth((col - at) as usize).unwrap_or(' ');
        }
        at = end;
    }
    ' '
}

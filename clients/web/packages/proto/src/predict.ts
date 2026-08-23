/**
 * Predicted echo: the glyph a keystroke will produce, drawn before the host
 * says so.
 *
 * The TypeScript port of `crates/zest-proto/src/predict.rs`, rule for rule —
 * read that module's docs for the reasoning, and ADR-016. Both ports replay
 * `crates/zest-proto/fixtures/predict.json`, which is what keeps the browser
 * and the native app agreeing about which glyphs are guesses.
 *
 * Three things this is not: a VT emulator (printables and a Backspace over its
 * own guesses, nothing else — ADR-004), a writer into `GridView` (an overlay
 * the painter draws; the grid is shared with every attached device), and a
 * wire feature (it reconciles from the delta's own rows and cursor, plus a
 * clock the caller supplies).
 */

import type { CursorState, Delta, RowPayload } from './wire.ts';

/**
 * A keystroke as the caller knows it *before* `encodeKey` — never un-encoded.
 *
 * `ch` is one code point; anything else (a ZWJ sequence, a composed string, an
 * empty string) is treated as `other` — it flushes and predicts nothing.
 */
export type PredictKey = { key: 'printable'; ch: string } | { key: 'backspace' } | { key: 'other' };

export type PredictPolicy = 'auto' | 'always' | 'off';

export interface Prediction {
  readonly row: number;
  readonly col: number;
  /** One code point. */
  readonly ch: string;
  readonly madeAt: number;
}

const SHOW_ABOVE_MS = 40;
const HIDE_BELOW_MS = 20;
const EXPIRE_AFTER_RTTS = 3;
const EXPIRE_FLOOR_MS = 100;
const EXPIRE_UNMEASURED_MS = 1000;
const EWMA = 0.3;

export class Predictor {
  #pending: Prediction[] = [];
  #cursor: CursorState = { row: 0, col: 0, visible: true, shape: 0 };
  #cols = 0;
  #altScreen = false;
  #quiet = false;
  #latencyMs: number | null = null;
  #remoteHint = false;
  #showing = false;

  readonly policy: PredictPolicy;

  constructor(policy: PredictPolicy = 'auto') {
    this.policy = policy;
  }

  /** Before a measurement exists, whether to show on faith. */
  setRemoteHint(remote: boolean): void {
    this.#remoteHint = remote;
  }

  /** A keyframe replaced the whole state. Every guess is void. */
  onKeyframe(cursor: CursorState, cols: number, altScreen: boolean): void {
    this.#pending = [];
    this.#cursor = cursor;
    this.#cols = cols;
    this.#altScreen = altScreen;
  }

  /** A keystroke is about to be sent. */
  onInput(key: PredictKey, nowMs: number): void {
    if (this.policy === 'off') return;
    switch (key.key) {
      case 'printable': {
        // One cell or two is the host's call (a client never computes
        // widths, ADR-004), and a guess placed after a wrong answer lands in
        // the spacer and refutes a correct line. Stop guessing instead.
        if (!narrow(key.ch)) {
          this.#pending = [];
          return;
        }
        // A full-screen program decides for itself what a key does.
        if (this.#altScreen) return;
        const last = this.#pending[this.#pending.length - 1];
        const row = last ? last.row : this.#cursor.row;
        const col = last ? last.col + 1 : this.#cursor.col;
        // Where the next glyph goes past the edge is the shell's wrapping rule.
        if (col >= this.#cols) return;
        this.#pending.push({ row, col, ch: key.ch, madeAt: nowMs });
        return;
      }
      case 'backspace':
        // Only our own guesses; a real cell is the host's to erase.
        this.#pending.pop();
        return;
      case 'other':
        this.#pending = [];
        return;
    }
  }

  /** A delta was applied. Judge every pending prediction against it. */
  reconcile(delta: Delta, nowMs: number): void {
    let cursorMoved = false;
    const rows = new Map<number, RowPayload>();
    for (const op of delta.ops) {
      switch (op.op) {
        case 'cursor':
          this.#cursor = op.cursor;
          cursorMoved = true;
          break;
        case 'row':
          rows.set(op.row, op.payload);
          break;
        case 'alt_screen':
          this.#altScreen = op.active;
          this.#pending = [];
          break;
        // The line a guess sat on moved or was cleared.
        case 'scroll':
        case 'erase':
          this.#pending = [];
          break;
        default:
          break;
      }
    }

    const cursor = this.#cursor;
    let i = 0;
    while (i < this.#pending.length) {
      const p = this.#pending[i] as Prediction;
      const delivered = rows.get(p.row);
      const passed = cursor.row !== p.row || cursor.col > p.col;
      let verdict: boolean | null;
      if (delivered !== undefined && passed) {
        // The host has written past this cell: the row says what it is.
        verdict = charAt(delivered, p.col) === p.ch;
      } else if (delivered !== undefined) {
        // The cursor has not reached the cell: the host may not have
        // processed that key yet. Ambiguous silence is what expiry is for.
        verdict = null;
      } else if (passed && cursorMoved) {
        // Coalesced: the cursor passed it but the row rode an earlier state.
        verdict = true;
      } else {
        verdict = null;
      }
      if (verdict === true) {
        this.#confirm(Math.max(0, nowMs - p.madeAt));
        this.#pending.splice(i, 1);
      } else if (verdict === false) {
        this.#refute();
        return;
      } else {
        i++;
      }
    }
    this.#expire(nowMs);
  }

  /** Time passed with nothing arriving. */
  tick(nowMs: number): void {
    this.#expire(nowMs);
  }

  #expire(nowMs: number): void {
    const oldest = this.#pending[0];
    if (oldest === undefined) return;
    const age = Math.max(0, nowMs - oldest.madeAt);
    if (this.#latencyMs !== null) {
      // A measured link had its chances: this is a shell that is not echoing.
      if (age > Math.max(this.#latencyMs * EXPIRE_AFTER_RTTS, EXPIRE_FLOOR_MS)) this.#refute();
    } else if (age > EXPIRE_UNMEASURED_MS) {
      this.#pending = [];
    }
  }

  #confirm(sampleMs: number): void {
    const rtt =
      this.#latencyMs === null ? sampleMs : this.#latencyMs + (sampleMs - this.#latencyMs) * EWMA;
    this.#latencyMs = rtt;
    this.#quiet = false;
    if (rtt > SHOW_ABOVE_MS) this.#showing = true;
    else if (rtt < HIDE_BELOW_MS) this.#showing = false;
  }

  #refute(): void {
    this.#pending = [];
    this.#quiet = true;
  }

  /** Measured press-to-echo latency, once something has echoed. */
  echoLatencyMs(): number | null {
    return this.#latencyMs;
  }

  /** Whether the overlay should be drawn at all right now. */
  showing(): boolean {
    if (this.policy === 'off') return false;
    if (this.#quiet) return false;
    if (this.policy === 'always') return true;
    return this.#latencyMs === null ? this.#remoteHint : this.#showing;
  }

  /** The cells to draw: empty unless `showing()`. */
  overlay(): readonly Prediction[] {
    return this.showing() ? this.#pending : [];
  }

  /** Where the caret belongs while guesses are pending: after the last one. */
  caret(): { row: number; col: number } | null {
    if (!this.showing()) return null;
    const last = this.#pending[this.#pending.length - 1];
    return last ? { row: last.row, col: last.col + 1 } : null;
  }

  /** Everything pending, shown or not. */
  pending(): readonly Prediction[] {
    return this.#pending;
  }
}

/**
 * A character this engine will vouch for occupying exactly one cell: exactly
 * one code point, below U+1100 (where the first wide range, Hangul Jamo,
 * begins), not a combining mark, not a control. Not a width table — a client
 * must never carry one. Anything else is *unknown here*, not wide.
 */
function narrow(ch: string): boolean {
  const cps = [...ch];
  if (cps.length !== 1) return false;
  const c = (cps[0] as string).codePointAt(0) as number;
  return c >= 0x20 && c < 0x1100 && !(c >= 0x0300 && c <= 0x036f) && c !== 0x7f;
}

/**
 * The character a row payload puts at `col`: one per cell, a space once a
 * run's text is exhausted — `expandRow`'s rule, restated for one cell.
 */
function charAt(row: RowPayload, col: number): string {
  let at = 0;
  for (const run of row.runs) {
    const end = at + run.cells;
    if (col < end) {
      const chars = [...run.text];
      return chars[col - at] ?? ' ';
    }
    at = end;
  }
  return ' ';
}

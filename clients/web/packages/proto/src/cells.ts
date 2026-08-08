/**
 * Turning runs back into cells.
 *
 * This is the file where a client is most likely to be quietly wrong, so the
 * rule is stated once and obeyed mechanically:
 *
 * > Emit exactly `run.cells` cells. Take characters from `run.text` in
 * > **code-point** order, and once they run out, pad with spaces.
 *
 * **Never call a width function.** `Run.cells` is the host's decision. Three
 * renderers will not agree on `wcwidth` for every emoji and combining sequence,
 * and a client that computes its own drifts from the host's grid by one column
 * in a way that is nearly impossible to trace back to its cause.
 *
 * Nor `text.length`, which is UTF-16 code units: every non-BMP emoji counts as
 * two and the row silently shifts. `[...text]` iterates code points, which is
 * what the Rust `chars()` on the other side does.
 *
 * A double-width character arrives as two runs — one with `WIDE` carrying the
 * character, one with `WIDE_SPACER` carrying no text at all — so a run whose
 * cell count exceeds its character count is the ordinary case, not a corruption.
 */

import { type Color, DEFAULT_COLOR } from './color.ts';
import type { AttrDef, RowPayload } from './wire.ts';

export interface Cell {
  /** The base character. Combining marks are beside it, never inside it. */
  readonly ch: string;
  readonly fg: Color;
  readonly bg: Color;
  readonly flags: number;
  /** Combining marks for this cell, in the order they were applied. */
  readonly marks: string;
}

export const BLANK: Cell = {
  ch: ' ',
  fg: DEFAULT_COLOR,
  bg: DEFAULT_COLOR,
  flags: 0,
  marks: '',
};

/**
 * Expand one row to exactly `cols` cells.
 *
 * The encoder drops trailing blanks in the default attribute — a mostly-empty
 * 200-column row is otherwise 200 cells of nothing on the wire every time
 * anything on it changes — so padding to the width, which the client knows, is
 * part of the format rather than a fallback.
 *
 * A run naming an attribute this client has never been told about renders in the
 * default rather than failing. That follows `GridView`, which is the reference
 * for this decoder; the stricter `Applier` asks for a keyframe instead, because
 * it is writing into a real terminal where being wrong is worse than being plain.
 */
export function expandRow(
  row: RowPayload,
  cols: number,
  attrs: ReadonlyMap<number, AttrDef>,
): Cell[] {
  const out: Cell[] = [];

  for (const run of row.runs) {
    const def = attrs.get(run.attr);
    const fg = def?.fg ?? DEFAULT_COLOR;
    const bg = def?.bg ?? DEFAULT_COLOR;
    const flags = def?.flags ?? 0;

    // The run's own first cell, so `CellMarks.at` — an offset within the run —
    // can be turned into a row offset. Reading `at` as a row offset instead is
    // the mistake this indirection exists to make visible.
    const base = out.length;

    const chars = [...run.text];
    for (let i = 0; i < run.cells; i++) {
      out.push({ ch: chars[i] ?? ' ', fg, bg, flags, marks: '' });
    }

    for (const m of run.marks) {
      const at = base + m.at;
      const cell = out[at];
      if (cell === undefined) continue;
      out[at] = { ...cell, marks: cell.marks + m.marks };
    }
  }

  while (out.length < cols) out.push(BLANK);
  return out;
}

/** The row as text, one code point per cell, marks excluded. */
export function rowText(cells: readonly Cell[]): string {
  return cells.map((c) => c.ch).join('');
}

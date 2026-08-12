/**
 * Rows as styled text runs, for renderers that lay out text rather than cells.
 *
 * The block body (`white-space: pre`, SGR colours per run — the handoff's
 * §3) and the phone's cards draw text, not a glyph grid, so what they need is
 * the row coalesced into the fewest spans that keep the styling. Built on
 * `expandRow`'s cell semantics rather than on the wire runs directly: run
 * boundaries are the encoder's choice and carry no meaning, so two runs in
 * the same style must merge, and one run can still split where a `WIDE`
 * spacer interrupts it.
 *
 * The spacer is the trap. A double-width character occupies two cells, and
 * the second — flagged `WIDE_SPACER` — holds no glyph; a grid renderer skips
 * it, so a text renderer must too, or every CJK character and emoji grows a
 * trailing space and nothing past it lines up. That cell is *suppressed*
 * here, which is the one place span text deliberately differs from
 * `rowText`: the row's cell count is a grid concern, not a text one.
 */

import { expandRow } from './cells.ts';
import { type Color, colorsEqual } from './color.ts';
import { CellFlags, hasFlag } from './flags.ts';
import type { AttrDef, RowPayload } from './wire.ts';

export interface Span {
  readonly text: string;
  readonly fg: Color;
  readonly bg: Color;
  readonly flags: number;
}

/**
 * One row as coalesced spans, unpadded.
 *
 * No `cols` parameter on purpose: padding to the grid width appends a span of
 * invisible default-styled blanks, which a text layout does not want and a
 * grid renderer should get from `expandRow` instead.
 *
 * Combining marks ride in the span text right after their base character —
 * `rowText` drops them because it is one-code-point-per-cell by contract, but
 * a renderer that dropped them would draw `é` as `e`, which is corruption,
 * not simplification.
 */
export function rowSpans(row: RowPayload, attrs: ReadonlyMap<number, AttrDef>): readonly Span[] {
  const spans: Array<{ text: string; fg: Color; bg: Color; flags: number }> = [];

  for (const cell of expandRow(row, 0, attrs)) {
    if (hasFlag(cell.flags, CellFlags.WIDE_SPACER)) continue;

    const text = cell.ch + cell.marks;
    const last = spans[spans.length - 1];
    if (
      last !== undefined &&
      last.flags === cell.flags &&
      colorsEqual(last.fg, cell.fg) &&
      colorsEqual(last.bg, cell.bg)
    ) {
      last.text += text;
    } else {
      spans.push({ text, fg: cell.fg, bg: cell.bg, flags: cell.flags });
    }
  }
  return spans;
}

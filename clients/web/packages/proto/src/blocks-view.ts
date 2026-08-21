/**
 * Block → rows: the selector behind the blocks pane and the phone's list.
 *
 * `GridView` holds two things that must be joined to render a block: the rows
 * (viewport plus client-kept scrollback) and the host's block index. This file
 * is that join and nothing more — it never inspects cell text to decide where
 * a command starts, because a client that re-parsed the grid for its own
 * prompts would be the second VT interpretation ADR-004 exists to avoid.
 *
 * Line ids are plain `number`s, like everywhere since #14: the wire sends the
 * narrowest integer, and 2^53 lines is nine quadrillion — no session reaches
 * it. The decoder (`wire.ts`) refuses an id a double cannot hold exactly, so
 * a comparison here can never be silently imprecise.
 *
 * Line ids are absolute *between reflows*, not for the life of a session: a
 * width change renumbers every one (`zest_core` grid/mod.rs). This join is
 * only sound because `GridView.applyKeyframe` discards the state it kept under
 * the old numbering at that boundary — everything walked here shares one
 * numbering with the blocks, by construction there, not by nature.
 */

import { NO_LINE } from './grid-view.ts';
import type { BlockPayload, RowPayload } from './wire.ts';

export interface BlockSlice {
  readonly block: BlockPayload;
  /** `prompt_line` up to (not including) `output_line`; the whole block when the command never started. */
  readonly promptRows: readonly RowPayload[];
  /** `output_line` through `end_line`, or to the bottom while the command runs. */
  readonly outputRows: readonly RowPayload[];
  /**
   * No end yet: the block must render to the bottom rather than waiting —
   * that is what makes a long build readable while it happens.
   */
  readonly open: boolean;
}

export interface BlocksLayout {
  /** Rows before the first block — the shell banner, or history from before integration. */
  readonly preamble: readonly RowPayload[];
  /** One slice per held block, ascending by id, empty row lists and all. */
  readonly slices: readonly BlockSlice[];
  /** Rows after the last finished block. Empty whenever a block is open. */
  readonly tail: readonly RowPayload[];
}

/**
 * Assign every row to its block, in one pass.
 *
 * Walks scrollback then viewport — both ascend by absolute line id, and
 * scrollback is strictly older, so the concatenation is the session in order.
 * Rows at `NO_LINE` are grid blanks a scroll manufactured; they have no
 * position in the session and are skipped.
 *
 * Membership mirrors `zest_core::Block::contains`:
 * `prompt_line <= line && (end_line === null || line <= end_line)` — the
 * wire's `end_line` is the last line **inclusive** (the parser pulls OSC 133;D
 * back onto it, so a one-line output has `end_line === output_line`). Blocks
 * ascend by both id and line, which is what lets one cursor over the block
 * list serve the whole walk.
 *
 * Every held block gets a slice, rows or none: the phone's list view is the
 * block index itself, and a block whose rows this client never cached (or
 * whose output was zero lines — `end_line` can sit *before* `output_line`
 * when a command printed nothing) is still a command that ran.
 */
export function sliceBlocks(view: {
  readonly rows: readonly RowPayload[];
  readonly scrollback: readonly RowPayload[];
  readonly blocks: readonly BlockPayload[];
}): BlocksLayout {
  const blocks = view.blocks;
  const preamble: RowPayload[] = [];
  const tail: RowPayload[] = [];
  const prompts: RowPayload[][] = blocks.map(() => []);
  const outputs: RowPayload[][] = blocks.map(() => []);

  let bi = 0;
  for (const row of walk(view)) {
    // Advance past blocks that ended before this row. An open block never
    // ends, so it claims everything below — but only the *last* block may,
    // and that is a bound rather than an assumption: an earlier open block
    // (an abandoned zsh prompt, from a host predating #193) otherwise
    // swallowed the whole session, leaving every later block rendering as a
    // bare header and the live prompt drawn inside a card halfway up the pane.
    // Where the next block starts is where this one must stop.
    while (bi < blocks.length) {
      const b = blocks[bi] as BlockPayload;
      const next = blocks[bi + 1];
      const ended = b.end_line !== null && row.line > b.end_line;
      const superseded = b.end_line === null && next !== undefined && row.line >= next.prompt_line;
      if (ended || superseded) bi += 1;
      else break;
    }

    const b = blocks[bi];
    if (b === undefined) {
      // Past every block: output the host stopped attributing, e.g. after the
      // block it belonged to was destroyed by a screen clear.
      tail.push(row);
      continue;
    }
    if (row.line < b.prompt_line) {
      // Before the first block this is ordinary — a client keeping longer
      // history than integration has been on for. A gap *between* blocks
      // cannot happen in an OSC 133 stream (each prompt begins where the last
      // block ended); if a host ever leaves one, the row still renders, after
      // the blocks, rather than silently vanishing.
      (bi === 0 ? preamble : tail).push(row);
      continue;
    }

    if (b.output_line !== null && row.line >= b.output_line) {
      (outputs[bi] as RowPayload[]).push(row);
    } else {
      (prompts[bi] as RowPayload[]).push(row);
    }
  }

  const slices: BlockSlice[] = blocks.map((b, i) => ({
    block: b,
    promptRows: prompts[i] as RowPayload[],
    outputRows: outputs[i] as RowPayload[],
    open: b.end_line === null,
  }));
  return { preamble, slices, tail };
}

function* walk(view: {
  readonly rows: readonly RowPayload[];
  readonly scrollback: readonly RowPayload[];
}): Generator<RowPayload> {
  for (const row of view.scrollback) if (row.line !== NO_LINE) yield row;
  for (const row of view.rows) if (row.line !== NO_LINE) yield row;
}

/**
 * The folded header's "N lines".
 *
 * Counts grid rows, not the lines the command printed: a wrapped line counts
 * once per row, which is exactly what unfolding will reveal — a count that
 * disagreed with the unfold would read as rows going missing.
 */
export function outputLineCount(slice: BlockSlice): number {
  return slice.outputRows.length;
}

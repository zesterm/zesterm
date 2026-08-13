/**
 * A client's reconstruction of a session.
 *
 * A port of `crates/zest-proto/src/decode.rs`, whose module doc names this file
 * before it existed: *"The reference decoder. The web and phone clients
 * reimplement this in TypeScript, and this is what they are checked against."*
 * Every deviation from it is a bug, so the shape follows it op for op even where
 * something else would read better in TypeScript.
 *
 * Two rules that look like details and are not:
 *
 * - **Never recompute cell widths.** See `cells.ts`.
 * - **Apply `scroll` before `row`** within a delta. The encoder emits them in
 *   that order and asserts it (`Delta::scrolls_come_first`); a decoder that
 *   sorts or reorders writes rows into positions the scroll is about to
 *   overwrite. So: iterate `ops` as given, and never sort.
 *
 * This holds a flat list of rows rather than a real terminal. The Rust side has
 * a second, stricter reading — `Applier`, which writes into a `zest_core::Terminal`
 * and refuses a delta whose `base` it does not hold. Where the two differ, this
 * follows `GridView`, and the differences are marked below.
 */

import type { AttrDef, BlockPayload, CursorState, Delta, RowPayload } from './wire.ts';
import { Modes } from './flags.ts';

/** `i64::MIN`, the line id `decode.rs` gives a row that has no content yet. */
export const NO_LINE = -(2n ** 63n);

const BLANK_ROW: RowPayload = { line: NO_LINE, runs: [], wrapped: false };

export interface KeyframeState {
  readonly cols: number;
  readonly rows_data: readonly RowPayload[];
  readonly attrs: readonly AttrDef[];
  readonly cursor: CursorState;
  readonly modes: number;
  /** Absent from callers that predate blocks; `[]` and absent mean the same. */
  readonly blocks?: readonly BlockPayload[];
  /**
   * The id from which `blocks` is authoritative. Absent means `0`, which
   * replaces wholesale.
   */
  readonly blocks_from?: number;
}

export class GridView {
  cols = 0;
  rows: RowPayload[] = [];

  /** Cumulative for the session, exactly as the encoder's interning table is. */
  attrs = new Map<number, AttrDef>();

  cursor: CursorState = { row: 0, col: 0, visible: true, shape: 0 };
  altScreen = false;

  /**
   * The host's mode bits, raw.
   *
   * Kept as an integer rather than widened into a flag set, which is what
   * `decode.rs` says this field is for: the TypeScript clients have no bitflags
   * type, and what they need is exactly the number.
   */
  modes = 0;

  title = '';

  /**
   * Lines that left the viewport, oldest first.
   *
   * Kept client-side because the host may evict them before this client asks: a
   * phone that was asleep for an hour cannot rely on the desktop still holding
   * what scrolled past.
   */
  scrollback: RowPayload[] = [];

  /**
   * Command blocks, ascending by id — the same shape as `decode.rs`.
   *
   * The phone's list view is this and nothing else, which is why it is kept
   * here rather than derived: a client that re-parsed the grid to find its own
   * prompts would be the second VT interpretation ADR-004 exists to avoid, and
   * it would disagree with the desktop about where a command started.
   */
  blocks: BlockPayload[] = [];

  /** Replace everything with a complete state. */
  applyKeyframe(k: KeyframeState): void {
    // A width change reflowed the host's grid, and reflow *renumbers* line ids
    // (`zest_core` grid/mod.rs: rewrapping changes how many rows a logical
    // line occupies, so old ids cannot survive). The keyframe's rows and
    // blocks arrive under the new numbering; everything this client kept —
    // scrollback rows, and blocks the host evicted before the resize — still
    // carries the old one, and the wire has no mapping between the two. Stale
    // state cannot be re-anchored, only discarded: joined with reanchored
    // blocks it hands pre-resize rows to a live command and pushes live rows
    // past their block (see `sliceBlocks`), which misrenders worse than a
    // shorter history reads.
    const renumbered = this.cols !== 0 && k.cols !== this.cols;
    if (renumbered) this.scrollback = [];
    this.cols = k.cols;
    // A *height* change scrolls rows out of the viewport and into the host's
    // history, and at an unchanged width their ids still mean what they meant.
    // Replacing `rows` wholesale therefore throws away rows this client holds
    // and the host still has — and the blocks anchored there go on naming them,
    // so they render as blocks with no rows: the pane looks like every block
    // vanished when only the client's copy of the text did. Carry them over
    // instead; there is nothing to fetch and nothing to renumber. (#200)
    if (!renumbered) {
      const firstNew = k.rows_data.find((r) => r.line !== NO_LINE)?.line;
      const lastHeld = this.scrollback.at(-1)?.line;
      if (firstNew !== undefined) {
        const displaced = this.rows.filter(
          (r) => r.line !== NO_LINE && r.line < firstNew && (lastHeld === undefined || r.line > lastHeld),
        );
        if (displaced.length > 0) this.scrollback = [...this.scrollback, ...displaced];
      }
    }
    this.rows = [...k.rows_data];
    // Merged, not replaced. A keyframe re-sends every attribute it uses, but the
    // ids stay meaningful to a client that has been attached throughout, so
    // dropping the table would discard definitions later deltas still name.
    for (const a of k.attrs) this.attrs.set(a.id, a);
    this.cursor = k.cursor;
    this.modes = k.modes;
    this.altScreen = (k.modes & Modes.ALT_SCREEN) !== 0;
    // Replaced from `blocks_from` up, not merged, unlike the attrs: a keyframe
    // carries every block the host holds from there, so one this client holds
    // above it that the keyframe does not mention is one the host destroyed —
    // `cls` erasing the rows it described. Below it are blocks the host merely
    // evicted, which a client keeping longer history than the host keeps.
    const from = k.blocks_from ?? 0;
    // Kept-because-evicted blocks are anchored in the old numbering too, so a
    // renumbering keyframe drops them along with the scrollback they described.
    const kept = renumbered ? [] : this.blocks.filter((b) => b.id < from);
    this.blocks = [...kept, ...(k.blocks ?? [])];
    // `title`, and `scrollback` at an unchanged width, are deliberately not
    // cleared: neither is part of the state a keyframe describes.
  }

  /**
   * Insert or replace a block, keeping the list ascending by id — `decode.rs`'s
   * `upsert_block`, binary search and all. A list that drifted out of order
   * would still *contain* the right blocks while answering "which block is
   * this line in" differently.
   */
  #upsertBlock(b: BlockPayload): void {
    let lo = 0;
    let hi = this.blocks.length;
    while (lo < hi) {
      const mid = (lo + hi) >> 1;
      if ((this.blocks[mid] as BlockPayload).id < b.id) lo = mid + 1;
      else hi = mid;
    }
    if (this.blocks[lo]?.id === b.id) this.blocks[lo] = b;
    else this.blocks.splice(lo, 0, b);
  }

  /**
   * Apply a change.
   *
   * Attributes are merged before the ops that reference them, or a run would
   * name a style this client has never been told about.
   */
  applyDelta(d: Delta): void {
    for (const a of d.attrs) this.attrs.set(a.id, a);

    for (const op of d.ops) {
      switch (op.op) {
        case 'scroll': {
          // `usize::try_from(lines).unwrap_or(0)` on the Rust side, so a
          // negative `lines` is a silent no-op rather than an upward scroll.
          // Matching that exactly matters more than it being the nicer rule.
          const n = op.lines > 0 ? op.lines : 0;
          if (n === 0 || op.top > op.bottom || op.bottom >= this.rows.length) continue;

          const end = Math.min(op.bottom, this.rows.length - 1);
          for (let i = op.top; i <= end; i++) {
            const src = i + n;
            this.rows[i] = src <= end ? (this.rows[src] as RowPayload) : BLANK_ROW;
          }
          continue;
        }

        case 'row': {
          if (op.row < this.rows.length) {
            this.rows[op.row] = op.payload;
          } else {
            // A row past the end means a resize this client has not been told
            // about. Growing is the forgiving answer; dropping it would leave a
            // permanently stale line. (`Applier` asks for a keyframe here
            // instead — it is writing into a real grid, which cannot grow a row
            // without also being wrong about its size.)
            while (this.rows.length < op.row) this.rows.push(BLANK_ROW);
            this.rows[op.row] = op.payload;
          }
          continue;
        }

        case 'erase': {
          // Not emitted by the current encoder, and implemented anyway, because
          // the other reference implements it — two readings of one wire format
          // that disagree about an op is how a client author finds out the
          // format is ambiguous.
          if (op.top > op.bottom || op.left > op.right) continue;
          const end = Math.min(op.bottom, this.rows.length - 1);
          for (let r = op.top; r <= end; r++) {
            const row = this.rows[r];
            if (row === undefined) continue;
            this.rows[r] = eraseSpan(row, op.left, op.right, op.attr);
          }
          continue;
        }

        case 'cursor':
          this.cursor = op.cursor;
          continue;

        case 'sb_push':
          // Unconditional. `Applier` skips this when the same delta scrolled,
          // because `Terminal::scroll` has already moved rows into real history
          // and pushing again would double-count. A flat row list has no history
          // to double-count, so it takes what it is given.
          this.scrollback.push(op.payload);
          continue;

        case 'alt_screen':
          // Sets the flag only. `modes` is a separate op; the encoder derives
          // both from one value, so they cannot disagree in practice.
          this.altScreen = op.active;
          continue;

        case 'title':
          this.title = op.title;
          continue;

        case 'modes':
          this.modes = op.bits;
          continue;
      }
    }

    // After the ops, matching `Applier` and `decode.rs`: a block names
    // absolute line ids that the rows in this same batch establish.
    for (const b of d.blocks) this.#upsertBlock(b);
  }
}

/**
 * Rebuild one row with `[left, right]` replaced by blanks in `attr`.
 *
 * Runs are re-split cell by cell and then coalesced, which is not the fastest
 * possible thing and is the clearest: `erase` is rare, and a clever in-place
 * splice across run boundaries is exactly where an off-by-one lives.
 *
 * Marks are dropped on the rebuilt row, matching `decode.rs`. That is fidelity
 * to the reference rather than an oversight — the two must agree.
 */
function eraseSpan(row: RowPayload, left: number, right: number, attr: number): RowPayload {
  const flat: Array<{ attr: number; ch: string }> = [];
  for (const run of row.runs) {
    const chars = [...run.text];
    for (let i = 0; i < run.cells; i++) {
      flat.push({ attr: run.attr, ch: chars[i] ?? ' ' });
    }
  }

  while (flat.length <= right) flat.push({ attr, ch: ' ' });
  for (let i = left; i <= right; i++) flat[i] = { attr, ch: ' ' };

  const runs: Array<{ attr: number; cells: number; text: string; marks: [] }> = [];
  for (const cell of flat) {
    const last = runs[runs.length - 1];
    if (last !== undefined && last.attr === cell.attr) {
      last.cells += 1;
      last.text += cell.ch;
    } else {
      runs.push({ attr: cell.attr, cells: 1, text: cell.ch, marks: [] });
    }
  }

  return { line: row.line, runs, wrapped: row.wrapped };
}

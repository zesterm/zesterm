/**
 * The Canvas 2D grid painter — the seam is `(grid, dirtyRows) → paint`.
 *
 * Deltas name the rows they touched, so repaints are row-scoped: a row is
 * cleared and redrawn whole, and untouched rows are never touched on the
 * canvas either. Colors resolve against the theme **here and only here**
 * (ADR-002): the grid carries `Color` values unresolved end-to-end, which is
 * what lets two clients render the same live session under different themes.
 *
 * Cell expansion is `expandRow` from `@zesterm/proto`, never local width math:
 * `Run.cells` is the host's decision, and this painter draws exactly the cells
 * it is handed. A wide character arrives as its glyph cell plus a spacer cell;
 * the glyph is drawn once at its cell and overflows the spacer's box
 * naturally, and the spacer is skipped.
 */

import {
  BLANK,
  CellFlags,
  NO_LINE,
  expandRow,
  type AttrDef,
  type Cell,
  type Color,
  type CursorState,
  type RowPayload,
} from '@zesterm/proto';
import type { TerminalPalette } from '@zesterm/theme';
import type { Canvas2DLike } from './context.ts';
import { fontString, type Metrics } from './metrics.ts';

/**
 * The slice of `GridView` the painter reads. Structural so tests hand-build
 * grids without running deltas through a real `GridView` first.
 */
export interface GridViewLike {
  readonly cols: number;
  readonly rows: readonly RowPayload[];
  readonly attrs: ReadonlyMap<number, AttrDef>;
  readonly cursor: CursorState;
  readonly modes: number;
}

export interface PaintOptions {
  /** The app's blink phase / focus state; the grid's own `cursor.visible` still rules. */
  cursorVisible?: boolean;
  /**
   * Accepted but unused: the seam ships before the feature. Selection will be
   * an overlay concern of this same paint call, so the parameter exists now
   * rather than forcing every caller to change when it lands.
   */
  selection?: null;
}

export interface PainterDeps {
  ctx: Canvas2DLike;
  metrics: Metrics;
  palette: TerminalPalette;
}

/** A row a delta never filled in — expands to all-blank cells. */
const EMPTY_ROW: RowPayload = { line: NO_LINE, runs: [], wrapped: false };

/**
 * DIM is 60% alpha on the glyph, not a blend of fg toward bg: alpha needs no
 * color parsing, is one context write, and reads as "faint" over any
 * background — including a colored span, where a precomputed blend toward the
 * *default* bg would be wrong.
 */
const DIM_ALPHA = 0.6;

// DOUBLE_UNDERLINE and UNDERCURL render as a plain underline for now — an
// honest simplification, not a bug: losing the distinction beats losing the
// line entirely on a client that has not grown curl rendering yet.
const UNDERLINE_FLAGS =
  CellFlags.UNDERLINE | CellFlags.DOUBLE_UNDERLINE | CellFlags.UNDERCURL;
const DECORATION_FLAGS = UNDERLINE_FLAGS | CellFlags.STRIKEOUT;

function hexByte(v: number): string {
  return v.toString(16).padStart(2, '0');
}

function resolveColor(c: Color, palette: TerminalPalette, fallback: string): string {
  if (c === 'Default') return fallback;
  if ('Indexed' in c) return palette.indexed(c.Indexed);
  const [r, g, b] = c.Rgb;
  return `#${hexByte(r)}${hexByte(g)}${hexByte(b)}`;
}

export class GridPainter {
  readonly #ctx: Canvas2DLike;
  readonly #metrics: Metrics;
  readonly #palette: TerminalPalette;

  // Canvas state writes are not free even when idempotent, and the recorded
  // command stream in tests stays readable when redundant sets are elided.
  // Reset at the top of every paint(): the context is shared with the app,
  // which may have touched it between frames.
  #lastFill: string | null = null;
  #lastFont: string | null = null;

  constructor(deps: PainterDeps) {
    this.#ctx = deps.ctx;
    this.#metrics = deps.metrics;
    this.#palette = deps.palette;
  }

  paint(
    grid: GridViewLike,
    dirty: ReadonlySet<number> | 'all',
    opts?: PaintOptions,
  ): void {
    this.#lastFill = null;
    this.#lastFont = null;
    this.#ctx.textBaseline = 'alphabetic';

    if (dirty === 'all') {
      for (let r = 0; r < grid.rows.length; r++) this.#paintRow(grid, r);
    } else {
      for (const r of dirty) {
        if (r >= 0) this.#paintRow(grid, r);
      }
    }

    // The cursor is an overlay on its row, so it repaints exactly when its row
    // did — a cursor drawn onto a clean row would double-strike, and one
    // skipped on a dirty row would be erased by the row clear.
    const cursorRowRepainted = dirty === 'all' || dirty.has(grid.cursor.row);
    if (grid.cursor.visible && opts?.cursorVisible !== false && cursorRowRepainted) {
      this.#paintCursor(grid);
    }
  }

  /** Resolve, then swap on INVERSE: reverse video over defaults must show bg-on-fg. */
  #colors(cell: Cell): { fg: string; bg: string } {
    const fg = resolveColor(cell.fg, this.#palette, this.#palette.defaultFg);
    const bg = resolveColor(cell.bg, this.#palette, this.#palette.defaultBg);
    return (cell.flags & CellFlags.INVERSE) !== 0 ? { fg: bg, bg: fg } : { fg, bg };
  }

  #setFill(color: string): void {
    if (color !== this.#lastFill) {
      this.#ctx.fillStyle = color;
      this.#lastFill = color;
    }
  }

  #setFont(flags: number): void {
    const font = fontString(this.#metrics, {
      bold: (flags & CellFlags.BOLD) !== 0,
      italic: (flags & CellFlags.ITALIC) !== 0,
    });
    if (font !== this.#lastFont) {
      this.#ctx.font = font;
      this.#lastFont = font;
    }
  }

  #paintRow(grid: GridViewLike, r: number): void {
    const { cellW, cellH, baseline, dpr } = this.#metrics;
    const ctx = this.#ctx;
    const y = r * cellH;
    const cells = expandRow(grid.rows[r] ?? EMPTY_ROW, grid.cols, grid.attrs);

    this.#setFill(this.#palette.defaultBg);
    ctx.fillRect(0, y, grid.cols * cellW, cellH);

    // The whole background pass runs before any glyph: a wide glyph overflows
    // into its spacer's box, so a spacer background painted after the glyph
    // would erase the glyph's right half. Runs of one color coalesce into one
    // rect; spans that resolve to the default are already covered by the clear.
    let spanStart = 0;
    let spanBg: string | null = null;
    const flush = (endCol: number): void => {
      if (spanBg !== null && endCol > spanStart) {
        this.#setFill(spanBg);
        ctx.fillRect(spanStart * cellW, y, (endCol - spanStart) * cellW, cellH);
      }
    };
    for (let i = 0; i < cells.length; i++) {
      const bg = this.#colors(cells[i] as Cell).bg;
      const want = bg === this.#palette.defaultBg ? null : bg;
      if (want !== spanBg) {
        flush(i);
        spanStart = i;
        spanBg = want;
      }
    }
    flush(cells.length);

    const t = Math.max(1, Math.round(dpr));
    for (let i = 0; i < cells.length; i++) {
      const cell = cells[i] as Cell;
      const flags = cell.flags;
      // The spacer holds no glyph by definition; its background was painted
      // above and its decoration is covered by the wide cell's double-width
      // rects below.
      if ((flags & CellFlags.WIDE_SPACER) !== 0) continue;
      // SGR 8 conceals the cell — decorations too, or a hidden password
      // prompt would still show its underline shape.
      if ((flags & CellFlags.HIDDEN) !== 0) continue;

      const hasGlyph = cell.ch !== ' ' || cell.marks !== '';
      if (!hasGlyph && (flags & DECORATION_FLAGS) === 0) continue;

      const x = i * cellW;
      const w = (flags & CellFlags.WIDE) !== 0 ? 2 * cellW : cellW;
      this.#setFill(this.#colors(cell).fg);
      const dim = (flags & CellFlags.DIM) !== 0;
      if (dim) ctx.globalAlpha = DIM_ALPHA;
      if (hasGlyph) {
        this.#setFont(flags);
        // Base and marks travel in one fillText: the font engine positions a
        // combining mark relative to the glyph it follows only when they are
        // shaped together — a mark drawn alone risks the dotted-circle
        // fallback. The x is still the base cell's x either way.
        ctx.fillText(cell.ch + cell.marks, x, y + baseline);
      }
      if ((flags & UNDERLINE_FLAGS) !== 0) {
        ctx.fillRect(x, y + baseline + t, w, t);
      }
      if ((flags & CellFlags.STRIKEOUT) !== 0) {
        ctx.fillRect(x, y + Math.round((cellH - t) / 2), w, t);
      }
      if (dim) ctx.globalAlpha = 1;
    }
  }

  #paintCursor(grid: GridViewLike): void {
    const { cellW, cellH, baseline, dpr } = this.#metrics;
    const ctx = this.#ctx;
    const { row, col, shape } = grid.cursor;
    const cells = expandRow(grid.rows[row] ?? EMPTY_ROW, grid.cols, grid.attrs);
    const cell = cells[col] ?? BLANK;
    const x = col * cellW;
    const y = row * cellH;
    const w = (cell.flags & CellFlags.WIDE) !== 0 ? 2 * cellW : cellW;
    const t = Math.max(1, Math.round(dpr));

    // The palette carries no accent token, and theme fg is the conventional
    // cursor color anyway — it keeps the painter's only theme input the
    // resolved palette.
    this.#setFill(this.#palette.defaultFg);

    // `shape` is the raw DECSCUSR parameter (delta.rs: "as the program asked
    // for it"): 0|1|2 block, 3|4 underline, 5|6 bar. Blink phase is the app's
    // concern via opts.cursorVisible, so the odd/even split is ignored here.
    // Unknown values fall back to block, matching `from_decscusr`'s default.
    if (shape === 3 || shape === 4) {
      ctx.fillRect(x, y + cellH - t, w, t);
    } else if (shape === 5 || shape === 6) {
      ctx.fillRect(x, y, Math.max(1, Math.round(2 * dpr)), cellH);
    } else {
      ctx.fillRect(x, y, w, cellH);
      // The block swallows the glyph, so it is restruck in the background
      // color — full alpha even for DIM cells, or the cursor hides its char.
      if (cell.ch !== ' ' || cell.marks !== '') {
        this.#setFill(this.#palette.defaultBg);
        this.#setFont(cell.flags);
        ctx.fillText(cell.ch + cell.marks, x, y + baseline);
      }
    }
  }
}

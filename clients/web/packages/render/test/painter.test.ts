/**
 * The painter, asserted through its command stream.
 *
 * A recording fake implements `Canvas2DLike` and logs every state write and
 * draw as a tagged tuple. The tests then assert on the stream — which rects
 * were cleared, in which color a glyph was struck, what x it landed on — which
 * is the whole observable behavior of a painter with no pixels to inspect.
 */

import assert from 'node:assert/strict';
import { test } from 'node:test';

import type { AttrDef, CellMarks, CursorState, Run, RowPayload } from '@zesterm/proto';
import { CellFlags } from '@zesterm/proto';
import type { TerminalPalette } from '@zesterm/theme';
import {
  GridPainter,
  fontString,
  type Canvas2DLike,
  type GridViewLike,
  type Metrics,
} from '../src/index.ts';

// ---------------------------------------------------------------------------
// The recording fake
// ---------------------------------------------------------------------------

type Op =
  | readonly ['fillStyle', string]
  | readonly ['font', string]
  | readonly ['textBaseline', string]
  | readonly ['globalAlpha', number]
  | readonly ['fillRect', number, number, number, number]
  | readonly ['fillText', string, number, number];

class RecordingContext implements Canvas2DLike {
  readonly ops: Op[] = [];

  #fillStyle: string | object = '';
  #font = '';
  #textBaseline = '';
  #globalAlpha = 1;

  get fillStyle(): string | object {
    return this.#fillStyle;
  }
  set fillStyle(v: string | object) {
    this.#fillStyle = v;
    this.ops.push(['fillStyle', String(v)]);
  }

  get font(): string {
    return this.#font;
  }
  set font(v: string) {
    this.#font = v;
    this.ops.push(['font', v]);
  }

  get textBaseline(): string {
    return this.#textBaseline;
  }
  set textBaseline(v: string) {
    this.#textBaseline = v;
    this.ops.push(['textBaseline', v]);
  }

  get globalAlpha(): number {
    return this.#globalAlpha;
  }
  set globalAlpha(v: number) {
    this.#globalAlpha = v;
    this.ops.push(['globalAlpha', v]);
  }

  fillRect(x: number, y: number, w: number, h: number): void {
    this.ops.push(['fillRect', x, y, w, h]);
  }

  fillText(text: string, x: number, y: number): void {
    this.ops.push(['fillText', text, x, y]);
  }

  /** The fillStyle in effect when op `i` executed. */
  styleAt(i: number): string {
    for (let j = i; j >= 0; j--) {
      const op = this.ops[j];
      if (op !== undefined && op[0] === 'fillStyle') return op[1];
    }
    return '';
  }

  rects(): Array<{ i: number; x: number; y: number; w: number; h: number; style: string }> {
    return this.ops.flatMap((op, i) =>
      op[0] === 'fillRect'
        ? [{ i, x: op[1], y: op[2], w: op[3], h: op[4], style: this.styleAt(i) }]
        : [],
    );
  }

  texts(): Array<{ i: number; text: string; x: number; y: number; style: string }> {
    return this.ops.flatMap((op, i) =>
      op[0] === 'fillText' ? [{ i, text: op[1], x: op[2], y: op[3], style: this.styleAt(i) }] : [],
    );
  }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

const FG = '#f0f0f0';
const BG = '#101010';

// Hand-built rather than resolved from tokens: these tests are about the
// painter's use of the palette, not the palette derivation, which has its own
// suite in @zesterm/theme.
const ANSI = [
  '#000000', '#cc0000', '#00cc00', '#cccc00', '#0000cc', '#cc00cc', '#00cccc', '#cccccc',
  '#666666', '#ff5555', '#55ff55', '#ffff55', '#5555ff', '#ff55ff', '#55ffff', '#ffffff',
];
const palette: TerminalPalette = {
  defaultFg: FG,
  defaultBg: BG,
  ansi: ANSI,
  indexed: (n: number): string => ANSI[n] ?? '#808080',
};

// Round numbers so every expected coordinate can be written literally.
const metrics: Metrics = { cellW: 10, cellH: 20, baseline: 15, dpr: 1, family: 'monospace', size: 14 };
const { cellW, cellH, baseline } = metrics;

const HIDDEN_CURSOR: CursorState = { row: 0, col: 0, visible: false, shape: 0 };

function run(attr: number, cells: number, text: string, marks: readonly CellMarks[] = []): Run {
  return { attr, cells, text, marks };
}

function row(...runs: Run[]): RowPayload {
  return { line: 0n, runs, wrapped: false };
}

function attrMap(defs: readonly AttrDef[]): ReadonlyMap<number, AttrDef> {
  return new Map(defs.map((d) => [d.id, d]));
}

function grid(g: {
  cols?: number;
  rows: readonly RowPayload[];
  attrs?: ReadonlyMap<number, AttrDef>;
  cursor?: CursorState;
}): GridViewLike {
  return {
    cols: g.cols ?? 8,
    rows: g.rows,
    attrs: g.attrs ?? new Map(),
    cursor: g.cursor ?? HIDDEN_CURSOR,
    modes: 0,
  };
}

function paint(
  g: GridViewLike,
  dirty: ReadonlySet<number> | 'all',
  opts?: { cursorVisible?: boolean },
): RecordingContext {
  const ctx = new RecordingContext();
  new GridPainter({ ctx, metrics, palette }).paint(g, dirty, opts);
  return ctx;
}

// ---------------------------------------------------------------------------
// Row scoping
// ---------------------------------------------------------------------------

test('one dirty row clears exactly that row band and no other', () => {
  const g = grid({ rows: [row(run(0, 1, 'a')), row(run(0, 1, 'b')), row(run(0, 1, 'c'))] });
  const ctx = paint(g, new Set([1]));

  const clears = ctx.rects().filter((r) => r.style === BG && r.w === g.cols * cellW);
  assert.equal(clears.length, 1, 'a Set{1} repaint must clear one row, or dirty tracking is theater');
  assert.deepEqual(
    [clears[0]?.x, clears[0]?.y, clears[0]?.w, clears[0]?.h],
    [0, 1 * cellH, g.cols * cellW, cellH],
    'the clear must sit at y = r * cellH — anywhere else erases a clean row',
  );
  for (const r of ctx.rects()) {
    assert.ok(
      r.y >= 1 * cellH && r.y + r.h <= 2 * cellH,
      `every rect must stay inside row 1's band; got y=${r.y} h=${r.h}`,
    );
  }
  for (const t of ctx.texts()) {
    assert.equal(t.text, 'b', "only the dirty row's glyphs may be struck");
  }
});

test("dirty='all' repaints every row", () => {
  const g = grid({ rows: [row(run(0, 1, 'a')), row(), row(run(0, 1, 'c'))] });
  const ctx = paint(g, 'all');

  const clearYs = ctx
    .rects()
    .filter((r) => r.style === BG && r.w === g.cols * cellW)
    .map((r) => r.y);
  assert.deepEqual(
    clearYs,
    [0, cellH, 2 * cellH],
    "'all' must clear each row's band once — a skipped row keeps stale pixels forever",
  );
});

// ---------------------------------------------------------------------------
// Backgrounds and colors
// ---------------------------------------------------------------------------

test('a colored bg fills before its text; default-bg runs add no fill beyond the clear', () => {
  const attrs = attrMap([{ id: 1, fg: 'Default', bg: { Indexed: 1 }, flags: 0 }]);
  const g = grid({ rows: [row(run(0, 3, 'abc'), run(1, 3, 'def'))], attrs });
  const ctx = paint(g, 'all');

  const bgFill = ctx.rects().find((r) => r.style === ANSI[1]);
  assert.ok(bgFill, 'the Indexed(1) background must be painted');
  assert.deepEqual(
    [bgFill.x, bgFill.y, bgFill.w, bgFill.h],
    [3 * cellW, 0, 3 * cellW, cellH],
    "the bg rect must cover exactly the run's cells",
  );

  const dText = ctx.texts().find((t) => t.text === 'd');
  assert.ok(dText, "the run's text must be struck");
  assert.ok(bgFill.i < dText.i, 'bg must land before text or it erases the glyphs');

  const defaultFills = ctx.rects().filter((r) => r.style === BG);
  assert.equal(
    defaultFills.length,
    1,
    'a default-bg run needs no fill of its own — the row clear already painted it',
  );
});

test('INVERSE swaps the resolved fg and bg', () => {
  const attrs = attrMap([
    { id: 1, fg: { Rgb: [255, 0, 0] }, bg: 'Default', flags: CellFlags.INVERSE },
  ]);
  const g = grid({ rows: [row(run(1, 1, 'x'))], attrs });
  const ctx = paint(g, 'all');

  const bgFill = ctx.rects().find((r) => r.style === '#ff0000');
  assert.ok(bgFill, 'inverse must paint the fg color as the background');
  const glyph = ctx.texts().find((t) => t.text === 'x');
  assert.ok(glyph, 'the glyph must still be struck');
  assert.equal(
    glyph.style,
    BG,
    'inverse over a Default bg must strike the glyph in the theme background — resolve first, swap after',
  );
});

// ---------------------------------------------------------------------------
// Styles and decorations
// ---------------------------------------------------------------------------

test('BOLD changes the font string; UNDERLINE emits a thin rect', () => {
  const attrs = attrMap([
    { id: 1, fg: 'Default', bg: 'Default', flags: CellFlags.BOLD | CellFlags.UNDERLINE },
  ]);
  const g = grid({ rows: [row(run(1, 1, 'u'))], attrs });
  const ctx = paint(g, 'all');

  const fonts = ctx.ops.filter((op) => op[0] === 'font').map((op) => op[1]);
  assert.ok(
    fonts.includes(fontString(metrics, { bold: true })),
    `a BOLD cell must be struck in the bold variant; fonts set: ${JSON.stringify(fonts)}`,
  );

  const thin = ctx.rects().find((r) => r.h === 1);
  assert.ok(thin, 'UNDERLINE must draw a 1px-thick rect at dpr 1');
  assert.deepEqual(
    [thin.x, thin.y, thin.w, thin.style],
    [0, baseline + 1, cellW, FG],
    'the underline sits one thickness below the baseline, cell-wide, in the fg color',
  );
});

test('DIM strikes the glyph at reduced alpha and restores full alpha after', () => {
  const attrs = attrMap([{ id: 1, fg: 'Default', bg: 'Default', flags: CellFlags.DIM }]);
  const g = grid({ rows: [row(run(1, 1, 'd'), run(0, 1, 'n'))], attrs });
  const ctx = paint(g, 'all');

  const dGlyph = ctx.texts().find((t) => t.text === 'd');
  const nGlyph = ctx.texts().find((t) => t.text === 'n');
  assert.ok(dGlyph && nGlyph, 'both glyphs must be struck');

  const alphaAt = (i: number): number => {
    for (let j = i; j >= 0; j--) {
      const op = ctx.ops[j];
      if (op !== undefined && op[0] === 'globalAlpha') return op[1];
    }
    return 1;
  };
  assert.ok(alphaAt(dGlyph.i) < 1, 'the DIM glyph must be faint');
  assert.equal(alphaAt(nGlyph.i), 1, 'alpha must be restored, or DIM leaks onto every later cell');
});

// ---------------------------------------------------------------------------
// Widths — the host's decision, never recomputed
// ---------------------------------------------------------------------------

test('a wide char draws once and the next glyph starts 2 cells later', () => {
  // The encoder's real shape: the character in a WIDE run, then a textless
  // WIDE_SPACER run that still occupies a cell (encode.rs).
  const attrs = attrMap([
    { id: 1, fg: 'Default', bg: 'Default', flags: CellFlags.WIDE },
    { id: 2, fg: 'Default', bg: 'Default', flags: CellFlags.WIDE_SPACER },
  ]);
  const g = grid({
    rows: [row(run(0, 1, 'a'), run(1, 1, '世'), run(2, 1, ''), run(0, 1, 'b'))],
    attrs,
  });
  const ctx = paint(g, 'all');

  const wide = ctx.texts().filter((t) => t.text.includes('世'));
  assert.equal(wide.length, 1, 'a wide glyph must be struck exactly once, at its own cell');
  assert.equal(wide[0]?.x, 1 * cellW, 'the wide glyph sits at its first cell');

  const b = ctx.texts().find((t) => t.text === 'b');
  assert.equal(
    b?.x,
    3 * cellW,
    'the glyph after a wide char starts two cells later — the spacer advances x without drawing',
  );
});

test('an astral emoji advances by its cells value, never by .length', () => {
  // '😀'.length is 2 (UTF-16 code units) but it is one code point in a run of
  // two cells. A painter counting code units would leave the ' ' padding cell
  // holding half a surrogate pair and shift every later column.
  const g = grid({ rows: [row(run(0, 2, '😀'), run(0, 1, 'x'))] });
  const ctx = paint(g, 'all');

  const emoji = ctx.texts().filter((t) => t.text.includes('😀'));
  assert.equal(emoji.length, 1, 'the emoji must be struck exactly once');
  assert.equal(emoji[0]?.x, 0, 'the emoji sits at its first cell');

  const x = ctx.texts().find((t) => t.text === 'x');
  assert.equal(
    x?.x,
    2 * cellW,
    "the next glyph starts at 2 * cellW — the run's cells count, not text.length",
  );
});

test('a combining mark is drawn at the same x as its base cell', () => {
  const g = grid({ rows: [row(run(0, 3, 'abc', [{ at: 1, marks: '́' }]))] });
  const ctx = paint(g, 'all');

  const marked = ctx.texts().find((t) => t.text.includes('́'));
  assert.ok(marked, 'the mark must reach the canvas — it is invisible in the run text');
  assert.ok(marked.text.includes('b'), 'the mark travels with its base so the font can shape them');
  assert.equal(marked.x, 1 * cellW, "the mark lands at the base cell's x, never a cell of its own");
});

// ---------------------------------------------------------------------------
// Cursor
// ---------------------------------------------------------------------------

test('the block cursor is painted last, at (col * cellW, row * cellH)', () => {
  const cursor: CursorState = { row: 1, col: 2, visible: true, shape: 2 };
  const g = grid({
    rows: [row(run(0, 1, 'a')), row(run(0, 4, 'wxyz'))],
    cursor,
  });
  const ctx = paint(g, 'all');

  const rects = ctx.rects();
  const cur = rects.find((r) => r.x === 2 * cellW && r.y === 1 * cellH && r.w === cellW && r.h === cellH);
  assert.ok(cur, 'the block cursor must fill its exact cell');
  assert.equal(cur.style, FG, 'the cursor block paints in the theme fg');

  for (const t of ctx.texts()) {
    if (t.i > cur.i) {
      assert.equal(
        t.style,
        BG,
        'the only draw after the cursor block is its glyph restruck in bg — the cursor must not be painted over',
      );
    }
  }
  const restruck = ctx.texts().find((t) => t.i > cur.i);
  assert.equal(restruck?.text, 'y', "the glyph under the cursor is restruck so it stays readable");
  assert.equal(restruck?.x, 2 * cellW, 'the restruck glyph stays in its cell');
});

test('the cursor is skipped when its row is not dirty, and when the app blinks it off', () => {
  const cursor: CursorState = { row: 1, col: 0, visible: true, shape: 2 };
  const g = grid({ rows: [row(run(0, 1, 'a')), row(run(0, 1, 'b'))], cursor });

  const cleanRow = paint(g, new Set([0]));
  assert.ok(
    !cleanRow.rects().some((r) => r.y === 1 * cellH),
    'a cursor on a clean row must not repaint — its pixels are already correct',
  );

  const blinkedOff = paint(g, 'all', { cursorVisible: false });
  assert.ok(
    !blinkedOff.rects().some((r) => r.style === FG && r.h === cellH && r.w === cellW),
    'cursorVisible: false suppresses the cursor even on a dirty row',
  );
});

test('DECSCUSR 3..6 select the underline and bar shapes', () => {
  const rows = [row(run(0, 1, 'a'))];

  const under = paint(grid({ rows, cursor: { row: 0, col: 0, visible: true, shape: 4 } }), 'all');
  assert.ok(
    under.rects().some((r) => r.style === FG && r.y === cellH - 1 && r.w === cellW && r.h === 1),
    'shapes 3|4 draw a thin rect along the bottom of the cell',
  );

  const bar = paint(grid({ rows, cursor: { row: 0, col: 0, visible: true, shape: 6 } }), 'all');
  assert.ok(
    bar.rects().some((r) => r.style === FG && r.x === 0 && r.h === cellH && r.w < cellW / 2),
    'shapes 5|6 draw a narrow full-height bar at the left edge of the cell',
  );
});

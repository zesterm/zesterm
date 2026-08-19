/**
 * How big a new session is.
 *
 * `Shell` asked the daemon for `120x32` regardless of the window, and the
 * daemon lays the shell's first prompt out at the size it is told — so every
 * browser session's first screen of output was formatted for a width the
 * window never had, then reflowed (#352).
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { DEFAULT_GRID, gridFor } from '../src/grid-fit.ts';

const CELL = { cellW: 8, cellH: 17, dpr: 1 };

test('cells fit the box, floored', () => {
  // Floored, never rounded: a partial column is one the terminal would draw
  // past its own edge, and a pty told it has 101 columns wraps at 101.
  assert.deepEqual(gridFor({ width: 807, height: 359 }, CELL), { cols: 100, rows: 21 });
});

test('device pixels are what the cell size is in', () => {
  // `measureMetrics` measures in device pixels, so a retina pane is twice as
  // many cells wide as its CSS width divided by the same number would suggest.
  // Getting this backwards halves every session on a retina display.
  assert.deepEqual(
    gridFor({ width: 800, height: 340 }, { cellW: 16, cellH: 34, dpr: 2 }),
    { cols: 100, rows: 20 },
  );
});

test('an unmeasurable box falls back rather than producing an absurd size', () => {
  // A pane that has not been laid out yet measures zero, and the `Math.max`
  // floors would turn that into a 2x1 terminal: a real size the daemon would
  // accept and lay a prompt out for. "We could not measure" and "it is two
  // cells wide" are different answers and only one of them is ever true.
  for (const box of [
    { width: 0, height: 400 },
    { width: 900, height: 0 },
    { width: -1, height: -1 },
  ]) {
    assert.deepEqual(gridFor(box, CELL), DEFAULT_GRID);
  }
  // A font that failed to load measures NaN, which every comparison answers
  // false to — the guard is written so that is the fallback and not a grid of
  // NaN columns.
  assert.deepEqual(gridFor({ width: 900, height: 400 }, { ...CELL, cellW: NaN }), DEFAULT_GRID);
  assert.deepEqual(gridFor({ width: 900, height: 400 }, { ...CELL, cellH: 0 }), DEFAULT_GRID);
});

test('the fallback is the size every terminal program already assumes', () => {
  // 80x24 rather than something roomier: a session that starts here and is
  // corrected a frame later reflows from a layout nothing renders badly. A
  // too-WIDE guess is the one that hurts — output already emitted at 120
  // columns cannot be rewrapped narrower without losing its line breaks,
  // which is exactly what the hard-coded 120x32 did on a narrow window.
  assert.deepEqual(DEFAULT_GRID, { cols: 80, rows: 24 });
});

test('a box smaller than one cell still names a usable grid', () => {
  // Measured and tiny is not the same as unmeasurable: a pane mid-drag can
  // genuinely be a few pixels, and the floors are what keep the answer a
  // terminal rather than a zero-column one the daemon would reject.
  assert.deepEqual(gridFor({ width: 3, height: 2 }, CELL), { cols: 2, rows: 1 });
});

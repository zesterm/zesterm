import { test } from 'node:test';
import assert from 'node:assert/strict';

import { canvasSizeFor } from '../src/grid-canvas.ts';

const m = { cellW: 10, cellH: 20, dpr: 2 };

test('the canvas bitmap follows the grid shape, not the wrapper', () => {
  // The arbitration case (#215): another client holds the session at 60x10
  // while this wrapper could fit far more. A wrapper-sized bitmap keeps stale
  // pixels below the shrunk grid forever, because nothing ever clears them.
  const grid = { cols: 60, rows: { length: 10 } };
  const wrapper = { clientWidth: 1000, clientHeight: 500 };
  assert.deepEqual(canvasSizeFor(grid, wrapper, m), { width: 600, height: 200 });
});

test('an empty grid falls back to the wrapper', () => {
  // Before the first keyframe there is no grid to follow, and a zero-sized
  // canvas would flash. The fallback is the old wrapper formula exactly.
  const grid = { cols: 0, rows: { length: 0 } };
  const wrapper = { clientWidth: 1000, clientHeight: 500 };
  assert.deepEqual(canvasSizeFor(grid, wrapper, m), {
    width: Math.floor((1000 * 2) / 10) * 10,
    height: Math.floor((500 * 2) / 20) * 20,
  });
});

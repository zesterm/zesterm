import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  PALETTE_CLOSED,
  openPalette,
  closePalette,
  setQuery,
  moveSelection,
} from '../src/state/palette.ts';

test('opening starts fresh', () => {
  const stale = moveSelection(setQuery(openPalette(), 'cargo'), 2, 5);
  const reopened = openPalette();
  void stale;
  assert.deepEqual(
    reopened,
    { open: true, query: '', selection: 0 },
    '⌘K then typing must not land keystrokes in the middle of last week\'s query',
  );
});

test('typing resets the selection', () => {
  const s = moveSelection(openPalette(), 3, 10);
  assert.equal(
    setQuery(s, 'x').selection,
    0,
    'the old index points into results that no longer exist',
  );
});

test('close keeps nothing visible', () => {
  assert.equal(closePalette(openPalette()).open, false);
  assert.equal(PALETTE_CLOSED.open, false, 'the initial state renders no palette');
});

test('selection wraps past the last result', () => {
  const atLast = moveSelection(openPalette(), 4, 5);
  assert.equal(atLast.selection, 4);
  assert.equal(
    moveSelection(atLast, 1, 5).selection,
    0,
    '↓ from the last result comes back to the top',
  );
});

test('selection wraps backwards from the first result', () => {
  assert.equal(
    moveSelection(openPalette(), -1, 5).selection,
    4,
    '↑ from the top is one keystroke to the last result, not a dead key',
  );
});

test('no results pins the selection to zero', () => {
  // Arrowing over an empty list must not produce an index the render would
  // have to bounds-check.
  assert.equal(moveSelection(openPalette(), 1, 0).selection, 0);
  assert.equal(moveSelection(moveSelection(openPalette(), 3, 10), 1, 0).selection, 0);
});

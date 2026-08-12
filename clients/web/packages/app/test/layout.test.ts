import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  LAYOUT_KEY,
  loadLayout,
  saveLayout,
  toggleLayout,
  type StorageLike,
} from '../src/state/layout.ts';

/** A `localStorage` stand-in — the interface is structural for exactly this. */
function fakeStorage(initial: Record<string, string> = {}): StorageLike & {
  data: Record<string, string>;
} {
  const data = { ...initial };
  return {
    data,
    getItem: (k) => (k in data ? (data[k] ?? null) : null),
    setItem: (k, v) => {
      data[k] = v;
    },
  };
}

test('an empty storage means horizontal', () => {
  assert.equal(loadLayout(fakeStorage()), 'horizontal', 'the default the handoff names');
});

test('vertical round-trips through storage', () => {
  const storage = fakeStorage();
  saveLayout(storage, 'vertical');
  assert.equal(loadLayout(storage), 'vertical', 'the choice must survive a reload');
});

test('a garbage stored value falls back to horizontal', () => {
  // A value written by a future version, or corrupted, must never keep the
  // window from opening — layout is a preference, not a precondition.
  assert.equal(loadLayout(fakeStorage({ [LAYOUT_KEY]: 'diagonal' })), 'horizontal');
  assert.equal(loadLayout(fakeStorage({ [LAYOUT_KEY]: '' })), 'horizontal');
});

test('the key is the published one', () => {
  // The literal is the contract with every window that already wrote it;
  // renaming it silently resets everyone to the default.
  const storage = fakeStorage();
  saveLayout(storage, 'vertical');
  assert.equal(storage.data['zesterm.layout'], 'vertical');
});

test('toggle flips both ways', () => {
  assert.equal(toggleLayout('horizontal'), 'vertical');
  assert.equal(toggleLayout('vertical'), 'horizontal', '⌘⇧E twice must come back');
});

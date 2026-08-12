import { test } from 'node:test';
import assert from 'node:assert/strict';

import { NO_FOLDS, toggle, isFolded, foldedFor } from '../src/state/folds.ts';

test('toggle folds, toggle again unfolds', () => {
  const folded = toggle(NO_FOLDS, 'studio', '3', 'b1');
  assert.ok(isFolded(folded, 'studio', '3', 'b1'), 'the chevron the user clicked must fold');
  const unfolded = toggle(folded, 'studio', '3', 'b1');
  assert.ok(!isFolded(unfolded, 'studio', '3', 'b1'), 'the same click must undo it');
});

test('the same session id on two hosts is two different sessions', () => {
  // Session ids are allocated per daemon, so studio's 3 and forge's 3 are
  // different terminals — the whole reason the API takes both ids.
  const s = toggle(NO_FOLDS, 'studio', '3', 'b1');
  assert.ok(!isFolded(s, 'forge', '3', 'b1'), 'folding on one host must not fold on another');
  assert.deepEqual([...foldedFor(s, 'forge', '3')], []);
});

test('foldedFor returns only that session\'s folds', () => {
  let s = toggle(NO_FOLDS, 'studio', '3', 'b1');
  s = toggle(s, 'studio', '3', 'b2');
  s = toggle(s, 'studio', '4', 'b9');
  assert.deepEqual(
    [...foldedFor(s, 'studio', '3')].sort(),
    ['b1', 'b2'],
    'the pane renders exactly its own session\'s folds',
  );
  assert.deepEqual([...foldedFor(s, 'studio', '4')], ['b9']);
});

test('an untouched session has an empty fold set, not a missing one', () => {
  assert.equal(
    foldedFor(NO_FOLDS, 'studio', '1').size,
    0,
    'a pane must be able to iterate folds without a null check',
  );
  assert.ok(!isFolded(NO_FOLDS, 'studio', '1', 'b1'));
});

test('toggle returns new state and leaves the old one alone', () => {
  // The reducers are pure so a signal wrapper can compare references; a
  // mutated old state would make every such comparison lie.
  const before = toggle(NO_FOLDS, 'studio', '3', 'b1');
  const after = toggle(before, 'studio', '3', 'b2');
  assert.ok(!isFolded(before, 'studio', '3', 'b2'), 'the earlier snapshot must not grow the fold');
  assert.ok(isFolded(after, 'studio', '3', 'b1'), 'the new state keeps what was already folded');
});

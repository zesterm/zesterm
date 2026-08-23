/**
 * The key classifier, held to the same answers as `zest-app`'s `predict_key`
 * (`app.rs`, `predict_key_reads_the_key_not_the_bytes`).
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { predictKeyOf, predictKeysOfText } from '../src/predict-key.ts';

const ev = (key: string, mods: Partial<{ ctrlKey: boolean; altKey: boolean; metaKey: boolean }> = {}) => ({
  key,
  ctrlKey: false,
  altKey: false,
  metaKey: false,
  ...mods,
});

test('predictKeyOf reads the key, not the bytes', () => {
  assert.deepEqual(predictKeyOf(ev('a')), { key: 'printable', ch: 'a' });
  assert.deepEqual(predictKeyOf(ev('A')), { key: 'printable', ch: 'A' }, 'shift is part of the character');
  assert.deepEqual(predictKeyOf(ev(' ')), { key: 'printable', ch: ' ' });
  assert.deepEqual(predictKeyOf(ev('Backspace')), { key: 'backspace' });
  assert.deepEqual(predictKeyOf(ev('c', { ctrlKey: true })), { key: 'other' }, '^C echoes nothing a guess could stand for');
  assert.deepEqual(predictKeyOf(ev('a', { altKey: true })), { key: 'other' });
  assert.deepEqual(predictKeyOf(ev('a', { metaKey: true })), { key: 'other' });
  assert.deepEqual(predictKeyOf(ev('Enter')), { key: 'other' });
  assert.deepEqual(predictKeyOf(ev('ArrowLeft')), { key: 'other' });
  assert.deepEqual(predictKeyOf(ev('Dead')), { key: 'other' }, 'a dead key has typed nothing yet');
  assert.deepEqual(predictKeyOf(ev('👨‍👩')), { key: 'other' }, 'more than one code point is not one character');
});

test('composed text is one guess per code point', () => {
  assert.deepEqual(predictKeysOfText('héllo'), [
    { key: 'printable', ch: 'h' },
    { key: 'printable', ch: 'é' },
    { key: 'printable', ch: 'l' },
    { key: 'printable', ch: 'l' },
    { key: 'printable', ch: 'o' },
  ]);
  assert.deepEqual(predictKeysOfText(''), []);
});

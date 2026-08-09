/**
 * Composed text: the path per-key encoding cannot see.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { Modes } from '@zesterm/proto';

import { encodeComposedText } from '../src/text.ts';
import { encodePaste } from '../src/paste.ts';

test('an IME commit encodes as plain UTF-8, astral planes included', () => {
  // The exact case the first live browser run dropped: an emoji arrives as a
  // composition, not as keydowns, and its bytes must reach the pty verbatim.
  assert.deepEqual(
    encodeComposedText('\u{1F600}'),
    Uint8Array.of(0xf0, 0x9f, 0x98, 0x80),
  );
  assert.deepEqual(
    encodeComposedText('世界'),
    Uint8Array.of(0xe4, 0xb8, 0x96, 0xe7, 0x95, 0x8c),
  );
});

test('composed text is never bracketed like a paste', () => {
  // A commit is typing. An app in bracketed-paste mode that received
  // ESC[200~ around a single kanji would treat keystrokes as a paste.
  const composed = encodeComposedText('世');
  const pasted = encodePaste('世', Modes.BRACKETED_PASTE);
  assert.ok(composed !== null);
  assert.notDeepEqual(composed, pasted, 'the two paths must stay distinct');
  assert.deepEqual(composed, Uint8Array.of(0xe4, 0xb8, 0x96));
});

test('an empty commit encodes nothing rather than an empty write', () => {
  assert.equal(encodeComposedText(''), null);
});

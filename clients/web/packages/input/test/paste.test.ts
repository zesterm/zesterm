/** Paste framing: bracketed only when the program asked, raw UTF-8 otherwise. */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { Modes } from '@zesterm/proto';
import { encodePaste } from '../src/paste.ts';

const seq = (s: string) => new TextEncoder().encode(s);

test('paste is bracketed only when the mode is set', () => {
  // A shell that never enabled 2004 would see the markers as typed input.
  assert.deepEqual(encodePaste('hello', 0), seq('hello'));
  assert.deepEqual(
    encodePaste('hello', Modes.BRACKETED_PASTE),
    seq('\x1b[200~hello\x1b[201~'),
  );
});

test('paste bytes are not rewritten', () => {
  // The host emulator is the truth; a client that filters or normalizes here
  // disagrees with the native window over the same session.
  const text = 'line1\nline2\r\t\x1b[31m世😀';
  assert.deepEqual(encodePaste(text, 0), seq(text));
  assert.deepEqual(
    encodePaste(text, Modes.BRACKETED_PASTE),
    seq(`\x1b[200~${text}\x1b[201~`),
    'wrapped verbatim, markers outside',
  );
});

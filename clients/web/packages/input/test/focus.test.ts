/** Focus reporting is opt-in; unsolicited reports are bytes a shell would echo. */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { Modes } from '@zesterm/proto';
import { encodeFocus } from '../src/focus.ts';

const seq = (s: string) => new TextEncoder().encode(s);

test('focus reports are gated on FOCUS_REPORTING', () => {
  assert.equal(encodeFocus(true, 0), null, 'no report unless the program asked');
  assert.equal(encodeFocus(false, 0), null);
  assert.deepEqual(encodeFocus(true, Modes.FOCUS_REPORTING), seq('\x1b[I'));
  assert.deepEqual(encodeFocus(false, Modes.FOCUS_REPORTING), seq('\x1b[O'));
});

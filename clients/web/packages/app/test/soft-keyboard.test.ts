/**
 * What a tap does to the hidden textarea, with no DOM (#428).
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  applyFocusAction,
  kbdCapAction,
  keyboardUp,
  setKeyboardUp,
  tapTerminalAction,
} from '../src/soft-keyboard.ts';

test('a touch on an already-focused terminal blurs and refocuses', () => {
  assert.equal(
    tapTerminalAction({ active: true, touch: true }),
    'refocus',
    'iOS opens the keyboard only for a focus change inside the gesture; the textarea is focused from mount on, so a plain focus() opened nothing (#428)',
  );
  assert.equal(tapTerminalAction({ active: false, touch: true }), 'focus');
});

test('a mouse click on a focused terminal does nothing', () => {
  assert.equal(
    tapTerminalAction({ active: true, touch: false }),
    'none',
    'a blur would send focus-out to vim on every click, and a mouse has no keyboard to open',
  );
  assert.equal(tapTerminalAction({ active: false, touch: false }), 'focus');
});

test('⌨ dismisses when the viewport says the keyboard is up, regardless of activeElement', () => {
  assert.equal(
    kbdCapAction({ keyboardUp: true, active: true }),
    'blur',
    "iOS's own dismiss key hides the keyboard without blurring, so activeElement cannot be the toggle's state",
  );
  assert.equal(kbdCapAction({ keyboardUp: true, active: false }), 'blur');
  assert.equal(kbdCapAction({ keyboardUp: false, active: true }), 'refocus');
  assert.equal(kbdCapAction({ keyboardUp: false, active: false }), 'focus');
});

test('refocus is blur then focus in the same task', () => {
  const calls: string[] = [];
  const el = { focus: () => calls.push('focus'), blur: () => calls.push('blur') } as unknown as HTMLElement;
  applyFocusAction(el, 'refocus');
  assert.deepEqual(calls, ['blur', 'focus'], 'the order is the focus change iOS is looking for');
  applyFocusAction(null, 'refocus');
});

test('the keyboard-up fact is what the viewport watcher last wrote', () => {
  assert.equal(keyboardUp(), false);
  setKeyboardUp(true);
  assert.equal(keyboardUp(), true);
  setKeyboardUp(false);
});

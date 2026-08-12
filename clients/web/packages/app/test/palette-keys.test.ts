import { test } from 'node:test';
import assert from 'node:assert/strict';

import { handlePaletteKey, type PaletteKeyEvent } from '../src/palette/keys.ts';

type Log = { moves: number[]; runs: number; dismissals: number };

const fire = (
  key: string,
  opts: { shiftKey?: boolean; isComposing?: boolean } = {},
): { log: Log; defaultPrevented: boolean } => {
  const log: Log = { moves: [], runs: 0, dismissals: 0 };
  let defaultPrevented = false;
  const e: PaletteKeyEvent = {
    key,
    shiftKey: opts.shiftKey ?? false,
    isComposing: opts.isComposing ?? false,
    preventDefault: () => {
      defaultPrevented = true;
    },
  };
  handlePaletteKey(e, {
    move: (delta) => log.moves.push(delta),
    run: () => (log.runs += 1),
    dismiss: () => (log.dismissals += 1),
  });
  return { log, defaultPrevented };
};

test('Tab is trapped: consumed, and fires nothing', () => {
  for (const shiftKey of [false, true]) {
    const { log, defaultPrevented } = fire('Tab', { shiftKey });
    assert.ok(
      defaultPrevented,
      'the hidden input is the dialog’s only tab stop — an unconsumed Tab walks ' +
        'focus out through the row buttons to the chrome behind the scrim, after which ' +
        'printable keys type into the live pty and Esc no longer dismisses (#157 review)',
    );
    assert.deepEqual(
      [log.moves, log.runs, log.dismissals],
      [[], 0, 0],
      'the footer advertises ↑↓ ⏎ esc only — Tab acting on the selection would be an unadvertised chord',
    );
  }
});

test('arrows move the selection and are consumed', () => {
  const down = fire('ArrowDown');
  assert.deepEqual(down.log.moves, [1]);
  assert.ok(down.defaultPrevented, 'an unconsumed arrow scrolls the results list under the cursor');
  const up = fire('ArrowUp');
  assert.deepEqual(up.log.moves, [-1]);
  assert.ok(up.defaultPrevented);
});

test('Enter runs, Escape dismisses', () => {
  const enter = fire('Enter');
  assert.equal(enter.log.runs, 1);
  assert.ok(enter.defaultPrevented, 'an unconsumed Enter would also submit/click whatever has focus');
  const esc = fire('Escape');
  assert.equal(esc.log.dismissals, 1);
  assert.ok(esc.defaultPrevented);
});

test('printable keys pass through untouched', () => {
  const { log, defaultPrevented } = fire('a');
  assert.equal(
    defaultPrevented,
    false,
    'typing must still reach the query input — the trap is for focus, not for text',
  );
  assert.deepEqual([log.moves, log.runs, log.dismissals], [[], 0, 0]);
});

test('IME composition owns every key first', () => {
  // Enter during composition commits the composition; treating it as "run"
  // would fire the selection mid-word for every CJK user.
  const { log, defaultPrevented } = fire('Enter', { isComposing: true });
  assert.equal(defaultPrevented, false);
  assert.deepEqual([log.moves, log.runs, log.dismissals], [[], 0, 0]);
});

/**
 * The shell chord table. Handler ordering is the carve-out: the app calls
 * `shellChord` BEFORE `belongsToBrowser`, which is how Ctrl+K reaches the
 * palette on Windows/Linux without touching the browser split.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { shellChord, type ShellAction } from '../src/chords.ts';
import { NO_MODS, mods, type Mods } from '../src/mods.ts';

const PLATFORMS = ['mac', 'other'] as const;

/** The platform's mod — meta on mac, ctrl elsewhere — plus any extras. */
function mod(platform: 'mac' | 'other', extra: Partial<Mods> = {}): Mods {
  return mods({ ...(platform === 'mac' ? { meta: true } : { ctrl: true }), ...extra });
}

test('the full table, on both platforms', () => {
  const table: readonly [{ key: string; code: string }, boolean, ShellAction][] = [
    [{ key: 'k', code: 'KeyK' }, false, { kind: 'palette' }],
    [{ key: 'd', code: 'KeyD' }, false, { kind: 'split' }],
    [{ key: 'E', code: 'KeyE' }, true, { kind: 'layout-toggle' }],
    [{ key: ',', code: 'Comma' }, false, { kind: 'settings' }],
    // Shift turns Comma's `key` into '<' on a US layout — matching on `code`
    // is what keeps ⌘⇧, working on every keymap.
    [{ key: '<', code: 'Comma' }, true, { kind: 'profiles' }],
    [{ key: 'O', code: 'KeyO' }, true, { kind: 'copy-output' }],
    [{ key: 'R', code: 'KeyR' }, true, { kind: 're-run' }],
  ];
  for (const platform of PLATFORMS) {
    for (const [key, shift, want] of table) {
      assert.deepEqual(
        shellChord(key, mod(platform, { shift }), platform),
        want,
        `${key.code}${shift ? '+shift' : ''} on ${platform}`,
      );
    }
  }
});

test('mod+1..9 name a tab; there is no tab 0', () => {
  for (const platform of PLATFORMS) {
    for (let n = 1; n <= 9; n += 1) {
      assert.deepEqual(
        shellChord({ key: String(n), code: `Digit${n}` }, mod(platform), platform),
        { kind: 'tab-n', n },
        `Digit${n} on ${platform}`,
      );
    }
    assert.equal(shellChord({ key: '0', code: 'Digit0' }, mod(platform), platform), null);
  }
});

test('Ctrl+K on other is the palette, plain Ctrl+C is not the shell\'s', () => {
  // The handoff mandates Ctrl+K opens the palette on Windows/Linux, which
  // steals readline's kill-to-eol by design; Ctrl+C stays SIGINT untouched.
  assert.deepEqual(shellChord({ key: 'k', code: 'KeyK' }, mods({ ctrl: true }), 'other'), {
    kind: 'palette',
  });
  assert.equal(
    shellChord({ key: 'c', code: 'KeyC' }, mods({ ctrl: true }), 'other'),
    null,
    'a chord not in the table falls through to the terminal encoder',
  );
});

test('the other platform\'s mod is not this platform\'s', () => {
  // Ctrl+K on a Mac stays readline's kill-to-eol — Command is available
  // there, so nothing needs stealing; and meta on Windows is the OS's key.
  assert.equal(shellChord({ key: 'k', code: 'KeyK' }, mods({ ctrl: true }), 'mac'), null);
  assert.equal(shellChord({ key: 'k', code: 'KeyK' }, mods({ meta: true }), 'other'), null);
});

test('extra modifiers disqualify a chord', () => {
  for (const platform of PLATFORMS) {
    assert.equal(
      shellChord({ key: 'k', code: 'KeyK' }, mod(platform, { alt: true }), platform),
      null,
      'mod+alt+K is a different chord, not a sloppy palette',
    );
  }
  assert.equal(
    shellChord({ key: 'k', code: 'KeyK' }, mods({ ctrl: true, meta: true }), 'mac'),
    null,
    'both mods held is neither table entry',
  );
});

test('shifted variants of unshifted entries stay unclaimed', () => {
  for (const platform of PLATFORMS) {
    assert.equal(
      shellChord({ key: 'K', code: 'KeyK' }, mod(platform, { shift: true }), platform),
      null,
      'mod+shift+K is not in the table and must not alias the palette',
    );
  }
});

test('the chord layer never claims a bare printable or a shift-only combination', () => {
  // Without this, typing would randomly trigger shell actions — every
  // printable has to reach the terminal encoder untouched.
  for (const platform of PLATFORMS) {
    assert.equal(shellChord({ key: 'k', code: 'KeyK' }, NO_MODS, platform), null);
    assert.equal(shellChord({ key: '5', code: 'Digit5' }, NO_MODS, platform), null);
    assert.equal(shellChord({ key: 'K', code: 'KeyK' }, mods({ shift: true }), platform), null);
    assert.equal(shellChord({ key: '<', code: 'Comma' }, mods({ shift: true }), platform), null);
  }
});

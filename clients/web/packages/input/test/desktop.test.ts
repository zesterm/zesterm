/**
 * The desktop/browser boundary, against `key.rs`'s `belongs_to_desktop` and
 * `is_clipboard_chord` tests, plus the browser-only `belongsToBrowser` split.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { belongsToDesktop, belongsToBrowser, isClipboardChord } from '../src/desktop.ts';
import { NO_MODS, mods, modsOf } from '../src/mods.ts';

test('command chords belong to the desktop', () => {
  // Without this the terminal encodes the bare key, and on macOS every
  // Cmd shortcut types its own letter -- Cmd+V inserting a `v` is how
  // this was found.
  assert.ok(belongsToDesktop(mods({ meta: true })));
  assert.ok(belongsToDesktop(mods({ meta: true, shift: true })));
  assert.ok(!belongsToDesktop(NO_MODS));
  assert.ok(!belongsToDesktop(mods({ ctrl: true })), 'Ctrl is the terminal\'s');
  assert.ok(!belongsToDesktop(mods({ alt: true })), 'Alt is the terminal\'s');
});

test('both clipboard conventions are accepted', () => {
  assert.ok(isClipboardChord(mods({ meta: true })), 'macOS uses Command');
  assert.ok(isClipboardChord(mods({ ctrl: true, shift: true })), 'Windows and Linux use Ctrl+Shift');
});

test('plain control is never a clipboard chord', () => {
  // The whole reason the Ctrl+Shift convention exists: Ctrl+C is SIGINT,
  // and a terminal that copied instead would be unusable.
  assert.ok(!isClipboardChord(mods({ ctrl: true })));
  assert.ok(!isClipboardChord(mods({ shift: true })));
  assert.ok(!isClipboardChord(NO_MODS));
});

test('meta chords belong to the browser on every platform', () => {
  const key = { key: 'v' };
  assert.ok(belongsToBrowser(key, mods({ meta: true }), 'mac'));
  assert.ok(belongsToBrowser(key, mods({ meta: true }), 'other'), 'the Windows key is the OS\'s too');
});

test('ctrl+shift copy/paste is released only where the browser convention lives', () => {
  const cs = mods({ ctrl: true, shift: true });
  assert.ok(belongsToBrowser({ key: 'C' }, cs, 'other'), 'Ctrl+Shift+C is copy on Windows/Linux');
  assert.ok(belongsToBrowser({ key: 'V' }, cs, 'other'));
  // On a Mac nothing outside the terminal claims Ctrl+Shift+C, so releasing it
  // would make the chord dead; it stays with the shell and types 0x03.
  assert.ok(!belongsToBrowser({ key: 'C' }, cs, 'mac'));
  // And Ctrl+Shift with any other key is the terminal's everywhere.
  assert.ok(!belongsToBrowser({ key: 'T' }, cs, 'other'));
  assert.ok(!belongsToBrowser({ key: 'c' }, mods({ ctrl: true }), 'other'), 'plain Ctrl+C is SIGINT');
});

test('modsOf reads the four KeyboardEvent booleans', () => {
  assert.deepEqual(
    modsOf({ shiftKey: true, ctrlKey: false, altKey: true, metaKey: false }),
    mods({ shift: true, alt: true }),
  );
});

/**
 * The key bar's decisions, with no DOM: which bytes a cap sends, how the
 * sticky modifiers latch, and when the bar shows by default (#421).
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { encodeKey, mods, modsOf } from '@zesterm/input';

import {
  CAPS,
  capKey,
  consume,
  KEYBAR_KEY,
  keyBarDefault,
  loadKeyBar,
  mergeMods,
  Modes,
  NO_LATCH,
  saveKeyBar,
  tapCap,
  tapModifier,
} from '../src/keybar-model.ts';

const seq = (s: string): Uint8Array => new TextEncoder().encode(s);

test('a cap encodes exactly what the same key from a hardware keyboard would', () => {
  // Byte-for-byte the hardware path, under the session's live modes: a bar
  // with its own table would be a third encoder to keep in step with the
  // Rust one.
  assert.deepEqual(tapCap('up', NO_LATCH, 0).bytes, seq('\x1b[A'));
  assert.deepEqual(
    tapCap('up', NO_LATCH, Modes.APP_CURSOR).bytes,
    seq('\x1bOA'),
    'under DECCKM the arrow must be the SS3 form, or vim and readline ignore the cap',
  );
  assert.deepEqual(tapCap('esc', NO_LATCH, 0).bytes, seq('\x1b'));
  assert.deepEqual(tapCap('tab', NO_LATCH, 0).bytes, seq('\t'));
  assert.deepEqual(tapCap('enter', NO_LATCH, 0).bytes, Uint8Array.of(0x0d), 'a bare CR, as key.ts sends Enter');
  assert.deepEqual(tapCap('ctrl-c', NO_LATCH, 0).bytes, Uint8Array.of(0x03), 'the interrupt, one tap');
  assert.deepEqual(tapCap('pipe', NO_LATCH, 0).bytes, seq('|'));
  assert.deepEqual(tapCap('tilde', NO_LATCH, 0).bytes, seq('~'));
});

test('the caps that are not keys encode nothing', () => {
  assert.equal(capKey('ctrl'), null);
  assert.equal(capKey('alt'), null);
  assert.equal(capKey('kbd'), null);
  assert.equal(tapCap('kbd', NO_LATCH, 0).bytes, null, 'the keyboard toggle is about focus, not bytes');
  for (const cap of CAPS) {
    if (capKey(cap.id) !== null) continue;
    assert.equal(tapCap(cap.id, NO_LATCH, 0).bytes, null, `${cap.id} must not wake the pty`);
  }
});

test('every cap in the row resolves — no id in the table without a meaning', () => {
  for (const cap of CAPS) {
    const r = tapCap(cap.id, NO_LATCH, 0);
    assert.ok(r.next !== undefined, cap.id);
  }
});

test('a sticky modifier cycles off → once → locked → off', () => {
  let s = NO_LATCH;
  s = tapModifier(s, 'ctrl');
  assert.equal(s.ctrl, 'once');
  s = tapModifier(s, 'ctrl');
  assert.equal(s.ctrl, 'locked');
  s = tapModifier(s, 'ctrl');
  assert.equal(s.ctrl, 'off');
  assert.equal(s.alt, 'off', 'the other modifier is untouched');
});

test('a once-latched Ctrl applies to the next key cap and releases', () => {
  const latched = tapCap('ctrl', NO_LATCH, 0);
  assert.equal(latched.bytes, null, 'tapping ctrl alone sends nothing — a bare modifier never wakes the pty');
  const r = tapCap('left', latched.next, 0);
  assert.deepEqual(r.bytes, seq('\x1b[1;5D'), 'Ctrl+Left, the word-jump readline knows');
  assert.equal(r.next.ctrl, 'off', 'spent');
});

test('a locked Ctrl stays across keys until tapped off', () => {
  const locked = tapModifier(tapModifier(NO_LATCH, 'ctrl'), 'ctrl');
  const a = tapCap('down', locked, 0);
  const b = tapCap('down', a.next, 0);
  assert.deepEqual(a.bytes, seq('\x1b[1;5B'));
  assert.deepEqual(b.bytes, seq('\x1b[1;5B'), 'still modified on the second key');
  assert.equal(b.next.ctrl, 'locked');
});

test('consuming a latch that spends nothing hands back the same state object', () => {
  // TerminalView decides "did the latch change" by identity, so a fresh
  // copy per keystroke would re-render the bar on every key typed.
  assert.equal(consume(NO_LATCH).next, NO_LATCH);
  const locked = tapModifier(tapModifier(NO_LATCH, 'alt'), 'alt');
  assert.equal(consume(locked).next, locked, 'a lock is not spent, so nothing changed');
  const once = tapModifier(NO_LATCH, 'ctrl');
  assert.notEqual(consume(once).next, once, 'a once IS spent');
});

test('the latch also modifies the next key from the soft keyboard itself', () => {
  // The bar latches; the letter comes from a real keydown. This is how ^L,
  // ^R and ^D — none of which have a cap — are reached on a tablet. The
  // component merges exactly this way.
  const { mods: latched, next } = consume(tapModifier(NO_LATCH, 'ctrl'));
  const e = { key: 'l', code: 'KeyL', shiftKey: false, ctrlKey: false, altKey: false, metaKey: false };
  assert.deepEqual(encodeKey(e, mergeMods(modsOf(e), latched), 0), Uint8Array.of(0x0c));
  assert.equal(next.ctrl, 'off');
});

test('merging never drops a modifier the event itself carried', () => {
  assert.deepEqual(
    mergeMods(mods({ shift: true, meta: true }), mods({ ctrl: true })),
    mods({ shift: true, meta: true, ctrl: true }),
  );
});

test('the bar shows by default on touch devices, decided by capability not platform', () => {
  // iPadOS Safari reports navigator.platform 'MacIntel'; a platform test
  // would hide the bar on the device the bar exists for.
  assert.equal(keyBarDefault({ coarsePointer: true, maxTouchPoints: 5 }), true);
  assert.equal(keyBarDefault({ coarsePointer: false, maxTouchPoints: 5 }), true, 'a tablet driven by a trackpad still has no Esc on its glass keyboard');
  assert.equal(keyBarDefault({ coarsePointer: false, maxTouchPoints: 0 }), false, 'a desktop keeps its pane');
});

test('a stored choice beats the device default, and garbage means unchosen', () => {
  const data: Record<string, string> = {};
  const storage = {
    getItem: (k: string) => data[k] ?? null,
    setItem: (k: string, v: string) => void (data[k] = v),
  };
  assert.equal(loadKeyBar(storage, true), true);
  saveKeyBar(storage, false);
  assert.equal(loadKeyBar(storage, true), false, 'an iPad with a hardware keyboard turned it off; that must survive a reload');
  data[KEYBAR_KEY] = 'maybe';
  assert.equal(loadKeyBar(storage, true), true, 'an unknown value must not hide the bar on a device that needs it');
});

/**
 * The Kitty encoder against `crates/zest-input/src/key.rs`'s own kitty tests.
 *
 * Every `#[test]` in that file's kitty section is mirrored here with the same
 * intent and the same expected bytes, because the two encoders serve the same
 * fleet session. Under this protocol a byte of disagreement is worse than
 * cosmetic: a program that turned the flags on has stopped expecting the legacy
 * form, so the browser tab types into a void while the native window works.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { Modes, kittyFlags } from '@zesterm/proto';
import { encodeKey } from '../src/key.ts';
import { NO_MODS, mods } from '../src/mods.ts';

const UTF8 = new TextEncoder();
const seq = (s: string) => UTF8.encode(s);
const bytes = (...xs: number[]) => Uint8Array.of(...xs);

/** The modes a program gets after pushing `flags`. */
const kitty = (flags: number) => Modes.AUTO_WRAP | Modes.SHOW_CURSOR | (flags << 16);

/** A KeyboardEvent-shaped key, with the fields the protocol reads. */
const k = (
  key: string,
  extra: { type?: string; repeat?: boolean; location?: number } = {},
) => ({ key, code: '', ...extra });

const up = (key: string, extra = {}) => k(key, { type: 'keyup', ...extra });

test('the flag layout matches the Rust bit positions', () => {
  // `1 << (16 + k)` for the protocol's `1 << k`, with holes at flags 4 and 16.
  // If this drifts, every expectation below is measuring the wrong thing.
  assert.equal(kittyFlags(kitty(1)), 1);
  assert.equal(kittyFlags(kitty(2)), 2);
  assert.equal(kittyFlags(kitty(8)), 8);
  assert.equal(kittyFlags(kitty(1 | 2 | 8)), 11);
  assert.equal(kittyFlags(Modes.AUTO_WRAP), 0, 'a program that never asked gets nothing');
});

test('no flags means the legacy encoding exactly', () => {
  // The whole protocol is opt-in. A program that never asked must see
  // byte-for-byte what it saw before this existed.
  const plain = Modes.AUTO_WRAP | Modes.SHOW_CURSOR;
  assert.deepEqual(encodeKey(k('Tab'), NO_MODS, plain), bytes(0x09));
  assert.deepEqual(encodeKey(k('Escape'), NO_MODS, plain), bytes(0x1b));
  assert.deepEqual(encodeKey(k('i'), mods({ ctrl: true }), plain), bytes(0x09));
});

test('ctrl+i stops being Tab under flag 1', () => {
  // The headline of the whole protocol: both are 0x09 in the legacy scheme, so
  // no program could ever bind them separately.
  const m = kitty(1);
  assert.deepEqual(encodeKey(k('i'), mods({ ctrl: true }), m), seq('\x1b[105;5u'));
  assert.deepEqual(encodeKey(k('Tab'), NO_MODS, m), bytes(0x09));
});

test('escape always takes the escape form under flag 1', () => {
  // A bare 0x1b is indistinguishable from the first byte of every other
  // sequence, so applications guess with a timer. This is what ends that.
  assert.deepEqual(encodeKey(k('Escape'), NO_MODS, kitty(1)), seq('\x1b[27u'));
});

test('enter and tab keep their bytes until a modifier needs saying', () => {
  const m = kitty(1);
  assert.deepEqual(encodeKey(k('Enter'), NO_MODS, m), bytes(0x0d));
  assert.deepEqual(
    encodeKey(k('Enter'), mods({ shift: true }), m),
    seq('\x1b[13;2u'),
    'Shift+Enter has no legacy byte at all -- this is why the protocol exists',
  );
});

test('flag 8 reports the keys that have bytes as escape codes too', () => {
  const m = kitty(1 | 8);
  assert.deepEqual(encodeKey(k('Enter'), NO_MODS, m), seq('\x1b[13u'));
  assert.deepEqual(encodeKey(k('Tab'), NO_MODS, m), seq('\x1b[9u'));
  assert.deepEqual(encodeKey(k('a'), NO_MODS, m), seq('\x1b[97u'));
  // Space reaches the printable path in the DOM rather than a named arm, and
  // must still land on the same number the Rust's `NamedKey::Space` gives.
  assert.deepEqual(encodeKey(k(' '), NO_MODS, m), seq('\x1b[32u'));
});

test('the reported code point is the unshifted key', () => {
  // The DOM hands over 'A' for Shift+a. A program keyed on 97 sees nothing if
  // 65 is sent, and the failure is silent and total.
  assert.deepEqual(encodeKey(k('A'), mods({ shift: true }), kitty(1 | 8)), seq('\x1b[97;2u'));
});

test('plain typing is still plain text', () => {
  // Under flags 1 and 2 a program wants the character, not a description of the
  // key that produced it.
  const m = kitty(1 | 2);
  assert.deepEqual(encodeKey(k('a'), NO_MODS, m), seq('a'));
  assert.deepEqual(encodeKey(k('A'), mods({ shift: true }), m), seq('A'));
});

test('event types are only reported when asked for', () => {
  assert.equal(
    encodeKey(up('ArrowUp'), NO_MODS, kitty(1)),
    null,
    'a release has no form until flag 2 provides one',
  );
  assert.deepEqual(
    encodeKey(up('ArrowUp'), NO_MODS, kitty(1 | 2)),
    seq('\x1b[1;1:3A'),
    'the modifier slot is still written, because the event type follows it',
  );
  assert.deepEqual(
    encodeKey(k('ArrowUp', { repeat: true }), NO_MODS, kitty(1 | 2)),
    seq('\x1b[1;1:2A'),
  );
});

test('a key with no escape form reports no release', () => {
  // There is no way to say "the key whose press was the byte `a` came back up",
  // and sending `a` again would type it twice.
  const m = kitty(1 | 2);
  assert.equal(encodeKey(up('a'), NO_MODS, m), null);
  // A repeat is not a release: a held key has always typed.
  assert.deepEqual(encodeKey(k('a', { repeat: true }), NO_MODS, m), seq('a'));
  // Enter is not a text key. It has a protocol number, so its release has
  // somewhere to be reported and escalates out of the legacy byte.
  assert.deepEqual(encodeKey(up('Enter'), NO_MODS, m), seq('\x1b[13;1:3u'));
});

test('a release is dropped entirely when nobody asked for event types', () => {
  // Without this the browser sends a second copy of every keystroke: the DOM
  // delivers keyup for everything, where winit's release was filtered by the
  // app before the encoder ever saw it.
  const plain = Modes.AUTO_WRAP | Modes.SHOW_CURSOR;
  assert.equal(encodeKey(up('a'), NO_MODS, plain), null);
  assert.equal(encodeKey(up('ArrowUp'), NO_MODS, plain), null);
  assert.equal(encodeKey(up('Enter'), NO_MODS, plain), null);
});

test('cursor keys never use SS3 under the protocol', () => {
  // SS3 has no parameter slot, so it cannot carry a modifier or an event type
  // -- and a program that asked to disambiguate is owed a form that can.
  // DECCKM is deliberately ignored here.
  const m = kitty(1) | Modes.APP_CURSOR;
  assert.deepEqual(encodeKey(k('ArrowUp'), NO_MODS, m), seq('\x1b[A'));
  assert.deepEqual(encodeKey(k('ArrowUp'), mods({ ctrl: true }), m), seq('\x1b[1;5A'));
});

test('F3 uses the tilde form because CSI R is CPR', () => {
  // `CSI 1;m R` is the cursor position report. A terminal that sent it for F3
  // would have programs answering their own keystrokes.
  const m = kitty(1);
  assert.deepEqual(encodeKey(k('F3'), NO_MODS, m), seq('\x1b[13~'));
  assert.deepEqual(encodeKey(k('F1'), mods({ ctrl: true }), m), seq('\x1b[11;5~'));
});

test('a bare modifier stays silent until flag 8', () => {
  // Waking the pty on every Shift press is the invariant the whole encoder is
  // built around.
  assert.equal(encodeKey(k('Shift'), mods({ shift: true }), kitty(1)), null);
  assert.deepEqual(
    encodeKey(k('Shift'), mods({ shift: true }), kitty(1 | 8)),
    seq('\x1b[57441;2u'),
  );
  // The right-hand instance is a different key to the protocol.
  assert.deepEqual(
    encodeKey(k('Shift', { location: 2 }), mods({ shift: true }), kitty(1 | 8)),
    seq('\x1b[57447;2u'),
  );
});

test('command chords are still the desktop’s under every flag', () => {
  // The protocol does not get to claim Cmd+V. Without this, macOS pastes
  // nothing and types a `v`.
  assert.equal(encodeKey(k('v'), mods({ meta: true }), kitty(1 | 2 | 8)), null);
});

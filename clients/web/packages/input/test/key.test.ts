/**
 * The key encoder against `crates/zest-input/src/key.rs`'s own tests.
 *
 * Every `#[test]` there is mirrored here with the same intent, because the two
 * encoders serve the same fleet session: a byte of disagreement means the same
 * keystroke types differently in a browser tab than in the native window.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { Modes } from '@zesterm/proto';
import { encodeKey, encodeText, modifierParam } from '../src/key.ts';
import { NO_MODS, mods } from '../src/mods.ts';

const UTF8 = new TextEncoder();
const seq = (s: string) => UTF8.encode(s);
const bytes = (...xs: number[]) => Uint8Array.of(...xs);
/** A KeyboardEvent-shaped key. `code` is carried but unused, as in the Rust encoder. */
const k = (key: string, code = '') => ({ key, code });

// --- mirrors of the Rust #[test]s -------------------------------------------

test('ctrl letters map to control codes', () => {
  assert.deepEqual(encodeText('c', true, false), bytes(0x03), 'Ctrl-C is ETX');
  assert.deepEqual(encodeText('a', true, false), bytes(0x01));
  assert.deepEqual(encodeText('d', true, false), bytes(0x04));
  // Case-insensitive: Ctrl-Shift-C still produces 0x03.
  assert.deepEqual(encodeText('C', true, false), bytes(0x03));
});

test('ctrl punctuation uses the conventional codes', () => {
  assert.deepEqual(encodeText('[', true, false), bytes(0x1b), 'Ctrl-[ is ESC');
  assert.deepEqual(encodeText(' ', true, false), bytes(0x00), 'Ctrl-Space is NUL');
  assert.deepEqual(encodeText('\\', true, false), bytes(0x1c));
});

test('ctrl with an unmapped key passes the character through', () => {
  // Swallowing it would make Ctrl-1 do nothing at all, which reads as the
  // terminal ignoring the keyboard.
  assert.deepEqual(encodeText('1', true, false), seq('1'));
});

test('alt prefixes with escape', () => {
  assert.deepEqual(encodeText('b', false, true), bytes(0x1b, 0x62));
  // Combined with Ctrl: ESC then the control code.
  assert.deepEqual(encodeText('c', true, true), bytes(0x1b, 0x03));
});

test('plain text passes through as utf8', () => {
  assert.deepEqual(encodeText('a', false, false), seq('a'));
  assert.deepEqual(encodeText('é', false, false), seq('é'));
  assert.deepEqual(encodeText('世', false, false), seq('世'));
});

test('arrow keys follow DECCKM', () => {
  // This is the one that breaks vim when it is wrong.
  assert.deepEqual(encodeKey(k('ArrowUp'), NO_MODS, 0), seq('\x1b[A'), 'CSI form by default');
  assert.deepEqual(
    encodeKey(k('ArrowUp'), NO_MODS, Modes.APP_CURSOR),
    seq('\x1bOA'),
    'SS3 form under DECCKM',
  );
});

test('modified cursor keys always use CSI', () => {
  // SS3 has no parameter slot, so a modified key must fall back to CSI
  // even when DECCKM is set.
  const ctrl = mods({ ctrl: true });
  assert.deepEqual(encodeKey(k('ArrowRight'), ctrl, Modes.APP_CURSOR), seq('\x1b[1;5C'));
  assert.deepEqual(encodeKey(k('ArrowRight'), ctrl, 0), seq('\x1b[1;5C'));
});

test('modifier parameters match xterm', () => {
  assert.equal(modifierParam(NO_MODS), 1);
  assert.equal(modifierParam(mods({ shift: true })), 2);
  assert.equal(modifierParam(mods({ alt: true })), 3);
  assert.equal(modifierParam(mods({ ctrl: true })), 5);
  assert.equal(modifierParam(mods({ ctrl: true, shift: true })), 6);
  assert.equal(modifierParam(mods({ ctrl: true, alt: true, shift: true })), 8);
});

test('function keys switch form when modified', () => {
  assert.deepEqual(encodeKey(k('F1'), NO_MODS, 0), seq('\x1bOP'), 'F1 is SS3 P');
  assert.deepEqual(
    encodeKey(k('F1'), mods({ shift: true }), 0),
    seq('\x1b[11;2~'),
    'modified F1 needs a parameter, so CSI',
  );
});

test('navigation keys use the tilde form', () => {
  assert.deepEqual(encodeKey(k('PageUp'), NO_MODS, 0), seq('\x1b[5~'), 'PageUp');
  assert.deepEqual(encodeKey(k('Delete'), NO_MODS, 0), seq('\x1b[3~'), 'Delete');
  assert.deepEqual(encodeKey(k('Delete'), mods({ ctrl: true }), 0), seq('\x1b[3;5~'));
});

test('empty input produces nothing', () => {
  // Distinguishing "nothing to send" from "send empty" keeps bare modifier
  // presses from waking the pty.
  assert.equal(encodeText('', false, false), null);
  assert.equal(encodeText('', false, true), null, 'even with Alt');
});

// --- the DOM-side cases the Rust suite has no analogue for ------------------

test('the editing keys match the Rust arms byte for byte', () => {
  assert.deepEqual(encodeKey(k('Enter'), NO_MODS, 0), bytes(0x0d));
  assert.deepEqual(encodeKey(k('Tab'), NO_MODS, 0), bytes(0x09));
  assert.deepEqual(encodeKey(k('Tab'), mods({ shift: true }), 0), seq('\x1b[Z'), 'back-tab is CBT');
  assert.deepEqual(
    encodeKey(k('Backspace'), NO_MODS, 0),
    bytes(0x7f),
    'Backspace is DEL, not BS — 0x08 breaks readline',
  );
  assert.deepEqual(encodeKey(k('Backspace'), mods({ ctrl: true }), 0), bytes(0x08));
  assert.deepEqual(encodeKey(k('Escape'), NO_MODS, 0), bytes(0x1b));
});

test('Home and End follow DECCKM like the arrows', () => {
  assert.deepEqual(encodeKey(k('Home'), NO_MODS, 0), seq('\x1b[H'));
  assert.deepEqual(encodeKey(k('End'), NO_MODS, Modes.APP_CURSOR), seq('\x1bOF'));
});

test('every function key carries the number key.rs assigns it', () => {
  // The gaps (16, 22) are xterm's, inherited from the VT220 layout; inventing a
  // contiguous numbering here would be a silent cross-client divergence.
  const expected: Array<[string, string]> = [
    ['F2', '\x1bOQ'],
    ['F3', '\x1bOR'],
    ['F4', '\x1bOS'],
    ['F5', '\x1b[15~'],
    ['F6', '\x1b[17~'],
    ['F7', '\x1b[18~'],
    ['F8', '\x1b[19~'],
    ['F9', '\x1b[20~'],
    ['F10', '\x1b[21~'],
    ['F11', '\x1b[23~'],
    ['F12', '\x1b[24~'],
  ];
  for (const [key, want] of expected) {
    assert.deepEqual(encodeKey(k(key), NO_MODS, 0), seq(want), key);
  }
  assert.deepEqual(encodeKey(k('PageDown'), NO_MODS, 0), seq('\x1b[6~'));
  assert.deepEqual(encodeKey(k('Insert'), NO_MODS, 0), seq('\x1b[2~'));
});

test('shifted arrows carry the xterm modifier parameter', () => {
  assert.deepEqual(encodeKey(k('ArrowUp'), mods({ shift: true }), 0), seq('\x1b[1;2A'));
  assert.deepEqual(
    encodeKey(k('ArrowLeft'), mods({ ctrl: true, alt: true, shift: true }), 0),
    seq('\x1b[1;8D'),
  );
});

test('space is a printable in the DOM and Ctrl-Space is still NUL', () => {
  // winit has NamedKey::Space; the DOM delivers `' '`, so space rides the
  // printable path here — the bytes must come out identical either way.
  assert.deepEqual(encodeKey(k(' ', 'Space'), NO_MODS, 0), seq(' '));
  assert.deepEqual(encodeKey(k(' ', 'Space'), mods({ ctrl: true }), 0), bytes(0x00));
});

test('ctrl chords reach the control table through encodeKey too', () => {
  assert.deepEqual(encodeKey(k('c', 'KeyC'), mods({ ctrl: true }), 0), bytes(0x03));
  assert.deepEqual(encodeKey(k('[', 'BracketLeft'), mods({ ctrl: true }), 0), bytes(0x1b));
  assert.deepEqual(
    encodeKey(k('C', 'KeyC'), mods({ ctrl: true, shift: true }), 0),
    bytes(0x03),
    'Ctrl-Shift-C is still ETX when the platform has not claimed it as copy',
  );
});

test('an astral-plane character encodes as its UTF-8 bytes', () => {
  // `'😀'.length` is 2 — a code-unit count would reject exactly the characters
  // the recorded corpus was once blind to. The encoder counts code points.
  assert.deepEqual(encodeKey(k('😀'), NO_MODS, 0), seq('😀'));
  assert.equal(seq('😀').length, 4, 'sanity: one code point, four UTF-8 bytes');
});

test('named non-printing keys produce nothing', () => {
  // The DOM spelling of the Rust `_ => None` arms: bare modifiers, dead keys,
  // keys this encoder has no sequence for. Waking the pty for these is the bug.
  for (const key of ['Shift', 'Control', 'Alt', 'Meta', 'Dead', 'Unidentified', 'F13', 'CapsLock']) {
    assert.equal(encodeKey(k(key), NO_MODS, 0), null, key);
  }
});

test('meta chords encode nothing', () => {
  // Falling through would encode the unmodified key: Cmd+V would type a `v`,
  // which is how the Rust encoder's guard was found.
  assert.equal(encodeKey(k('v', 'KeyV'), mods({ meta: true }), 0), null);
  assert.equal(encodeKey(k('ArrowUp'), mods({ meta: true }), 0), null);
});

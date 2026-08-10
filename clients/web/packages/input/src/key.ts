/**
 * Keyboard events to terminal byte sequences.
 *
 * `(key, mods, modes) -> Uint8Array | null`. `null` means *nothing to send*,
 * which is not the same as sending nothing: a bare modifier press must not wake
 * the pty at all.
 *
 * This is a case-for-case port of `crates/zest-input/src/key.rs`. That file is
 * the source of truth; a divergence here is a key that types differently in a
 * browser tab than in the native window, over the same fleet session.
 *
 * # How winit's key model maps onto `KeyboardEvent`
 *
 * The Rust encoder matches on `winit::keyboard::Key`, which is either a
 * `NamedKey` or a `Character(text)`. The DOM folds both into `event.key`:
 *
 * - `NamedKey::Enter/Tab/Backspace/Escape/ArrowUp/…/F12` are the identically
 *   named `e.key` strings (`'Enter'`, `'ArrowUp'`, `'F5'`, …) — the W3C names
 *   and winit's are the same words for every key this encoder handles.
 * - `Key::Character(text)` is any `e.key` that is a single code point
 *   (`'a'`, `'É'`, `'世'`, `'😀'`). Counting *code points*, never `.length`:
 *   UTF-16 gives an astral-plane character a length of 2, and treating that as
 *   "not a character" would silently drop exactly the input the recorded
 *   corpus was once blind to.
 * - `NamedKey::Space` has no DOM analogue — `e.key` is `' '`, a single code
 *   point — so space flows through the printable path here. Same bytes: the
 *   Rust arm just calls `encode_text(" ", …)` too.
 * - Everything else winit returns `None` for (`Dead`, `Unidentified`, bare
 *   `'Shift'`/`'Control'`/… presses) is a multi-code-point named string in the
 *   DOM and falls out of the same default arm.
 */

import { Modes, kittyFlags } from '@zesterm/proto';
import type { Mods } from './mods.ts';
import { belongsToDesktop } from './desktop.ts';
import { type EventType, encodeKitty } from './kitty.ts';

const UTF8 = new TextEncoder();

/** Which of press, repeat and release a DOM event describes. */
function eventTypeOf(key: KeyLike): EventType {
  if (key.type === 'keyup') return 'release';
  return key.repeat === true ? 'repeat' : 'press';
}

/** The `key`/`code` pair off a KeyboardEvent, kept structural so tests need no DOM. */
export interface KeyLike {
  readonly key: string;
  /** Unused today: the Rust encoder works from the logical key only, and so does this port. */
  readonly code: string;
  /**
   * The rest is read only under the Kitty keyboard protocol, and is optional so
   * that a caller with nothing but a key name still type-checks — a real
   * `KeyboardEvent` satisfies all of it structurally.
   */
  /** `'keyup'` rather than `'keydown'`. Absent means a press. */
  readonly type?: string;
  /** A key held down and auto-repeating. */
  readonly repeat?: boolean;
  /** `KeyboardEvent.location`: which of a paired modifier this is. */
  readonly location?: number;
}

/**
 * Encode a key press.
 *
 * Returns `null` for keys that produce nothing — bare modifiers, unhandled
 * function keys, anything the desktop has claimed — so the caller can
 * distinguish "nothing to send" from "sent empty".
 */
export function encodeKey(key: KeyLike, mods: Mods, modes: number): Uint8Array | null {
  // Falling through with Super held would encode the *unmodified* key, so on
  // macOS every Command shortcut types its own letter: Cmd+V pastes nothing
  // and inserts a `v`. `null` leaves the chord to the caller, which is where
  // the clipboard shortcuts live.
  if (belongsToDesktop(mods)) {
    return null;
  }

  // The Kitty keyboard protocol, when the program asked for it. A separate path
  // rather than a tweak to the one below: its disambiguation model is not
  // expressible as one, and everything under it must stay byte-identical for
  // the programs that never asked.
  if (kittyFlags(modes) !== 0) {
    return encodeKitty(key.key, key.location ?? 0, mods, eventTypeOf(key), modes);
  }

  // Nothing asked for event types, so a release is not encodable and a repeat
  // is indistinguishable from a press -- which is what every program not
  // speaking that protocol expects.
  if (key.type === 'keyup') {
    return null;
  }

  // The cursor-key prefix. DECCKM (`CSI ? 1 h`) switches the whole family from
  // CSI to SS3, and applications set it expecting to be obeyed.
  const cursor = (modes & Modes.APP_CURSOR) !== 0 ? '\x1bO' : '\x1b[';

  switch (key.key) {
    case 'Enter':
      return Uint8Array.of(0x0d);
    case 'Tab':
      return mods.shift
        ? UTF8.encode('\x1b[Z') // CBT, back-tab
        : Uint8Array.of(0x09);
    // Backspace is DEL (0x7f), not BS (0x08). Sending 0x08 is a classic
    // mistake that makes readline and most shells misbehave.
    case 'Backspace':
      return Uint8Array.of(mods.ctrl ? 0x08 : 0x7f);
    case 'Escape':
      return Uint8Array.of(0x1b);

    case 'ArrowUp':
      return withMods(cursor, 'A', mods);
    case 'ArrowDown':
      return withMods(cursor, 'B', mods);
    case 'ArrowRight':
      return withMods(cursor, 'C', mods);
    case 'ArrowLeft':
      return withMods(cursor, 'D', mods);

    // Home and End also follow DECCKM.
    case 'Home':
      return withMods(cursor, 'H', mods);
    case 'End':
      return withMods(cursor, 'F', mods);

    case 'PageUp':
      return tilde(5, mods);
    case 'PageDown':
      return tilde(6, mods);
    case 'Insert':
      return tilde(2, mods);
    case 'Delete':
      return tilde(3, mods);

    case 'F1':
      return ss3OrTilde('P', 11, mods);
    case 'F2':
      return ss3OrTilde('Q', 12, mods);
    case 'F3':
      return ss3OrTilde('R', 13, mods);
    case 'F4':
      return ss3OrTilde('S', 14, mods);
    case 'F5':
      return tilde(15, mods);
    case 'F6':
      return tilde(17, mods);
    case 'F7':
      return tilde(18, mods);
    case 'F8':
      return tilde(19, mods);
    case 'F9':
      return tilde(20, mods);
    case 'F10':
      return tilde(21, mods);
    case 'F11':
      return tilde(23, mods);
    case 'F12':
      return tilde(24, mods);

    default:
      break;
  }

  // `Key::Character`: a single code point. Multi-code-point values are the DOM's
  // named keys (`'Shift'`, `'Dead'`, `'Unidentified'`, F13+, media keys) — the
  // `_ => None` arm of the Rust match. Composed grapheme clusters longer than
  // one code point arrive through the IME path, which is deferred (see index.ts).
  if (isOneCodePoint(key.key)) {
    return encodeText(key.key, mods.ctrl, mods.alt);
  }

  return null;
}

/** Code points, never `.length` — one astral-plane character is `.length === 2`. */
function isOneCodePoint(s: string): boolean {
  let n = 0;
  for (const _ of s) {
    n += 1;
    if (n > 1) return false;
  }
  return n === 1;
}

/** Printable text, with Ctrl and Alt applied. */
export function encodeText(text: string, ctrl: boolean, alt: boolean): Uint8Array | null {
  if (ctrl) {
    // Ctrl maps a letter to its control code: Ctrl-A is 0x01. The
    // punctuation cases are the conventional ones every terminal implements.
    const first = text.codePointAt(0);
    if (first === undefined) {
      return null;
    }
    // ASCII lowercase only, like Rust's `to_ascii_lowercase`: `toLowerCase()`
    // has locale-shaped surprises outside ASCII and none of them are wanted here.
    const lower = first >= 0x41 && first <= 0x5a ? first + 0x20 : first;
    let code: number | null = null;
    if (lower >= 0x61 && lower <= 0x7a) {
      code = lower - 0x61 + 1;
    } else {
      switch (String.fromCodePoint(lower)) {
        case '@':
        case ' ':
          code = 0x00;
          break;
        case '[':
          code = 0x1b;
          break;
        case '\\':
          code = 0x1c;
          break;
        case ']':
          code = 0x1d;
          break;
        case '^':
          code = 0x1e;
          break;
        case '_':
        case '?':
          code = 0x1f;
          break;
        default:
          // Not a control combination -- pass the character through rather
          // than swallowing it.
          code = null;
      }
    }
    if (code !== null) {
      return finish(Uint8Array.of(code), alt);
    }
    return finish(UTF8.encode(text), alt);
  }

  return finish(UTF8.encode(text), alt);
}

/**
 * Alt is sent as a leading ESC.
 *
 * The alternative convention (setting the high bit) is long obsolete and breaks
 * UTF-8 outright.
 */
function finish(bytes: Uint8Array, alt: boolean): Uint8Array | null {
  if (bytes.length === 0) {
    return null;
  }
  if (alt) {
    const out = new Uint8Array(bytes.length + 1);
    out[0] = 0x1b;
    out.set(bytes, 1);
    return out;
  }
  return bytes;
}

/** The xterm modifier parameter: 1 + bitmask. */
export function modifierParam(mods: Mods): number {
  let m = 0;
  if (mods.shift) m |= 1;
  if (mods.alt) m |= 2;
  if (mods.ctrl) m |= 4;
  return m + 1;
}

/**
 * A cursor-style key, with a modifier parameter when one applies.
 *
 * Modified cursor keys always use CSI, never SS3, even under DECCKM — SS3 has
 * no parameter slot to put the modifier in.
 */
function withMods(prefix: string, finalChar: string, mods: Mods): Uint8Array {
  const param = modifierParam(mods);
  if (param === 1) {
    return UTF8.encode(prefix + finalChar);
  }
  return UTF8.encode(`\x1b[1;${param}${finalChar}`);
}

/** A `CSI n ~` key. */
function tilde(n: number, mods: Mods): Uint8Array {
  const param = modifierParam(mods);
  if (param === 1) {
    return UTF8.encode(`\x1b[${n}~`);
  }
  return UTF8.encode(`\x1b[${n};${param}~`);
}

/** F1-F4 are SS3 unmodified and `CSI n ~` when modified. */
function ss3OrTilde(ss3: string, tildeN: number, mods: Mods): Uint8Array {
  if (modifierParam(mods) === 1) {
    return UTF8.encode(`\x1bO${ss3}`);
  }
  return tilde(tildeN, mods);
}

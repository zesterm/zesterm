/**
 * The Kitty keyboard protocol (CSI u), flags 1, 2 and 8.
 *
 * A case-for-case port of the kitty half of `crates/zest-input/src/key.rs`.
 * That file is the source of truth; a divergence here is a key that types
 * differently in a browser tab than in the native window, over the same fleet
 * session — and under this protocol the divergence is not cosmetic, because a
 * program that turned the flags on has stopped expecting the legacy bytes
 * entirely.
 *
 * The flags themselves are set by the program, on the host, and arrive here in
 * the mode word (`zest_core::Modes`, bits 16 upward). The client encodes; the
 * host only says what the encoding should be.
 *
 * # What is not implemented, and is therefore never advertised
 *
 * Flags 4 (alternate keys) and 16 (associated text) are masked off by the
 * terminal, so `kittyFlags` can never return them and there is nothing to
 * handle here. Keypad separation under flag 1 is likewise absent on both
 * sides — see the roadmap.
 */

import { kittyFlags } from '@zesterm/proto';
import type { Mods } from './mods.ts';
import { modifierParam } from './key.ts';

const UTF8 = new TextEncoder();

/**
 * Which of press, repeat and release this is.
 *
 * The legacy scheme cannot express the distinction at all — it is the reason
 * kitty flag 2 exists — so it is carried separately from the modifiers.
 */
export type EventType = 'press' | 'repeat' | 'release';

/** The protocol's sub-parameter. Press is 1 and is written as the default. */
function eventParam(event: EventType): number {
  switch (event) {
    case 'press':
      return 1;
    case 'repeat':
      return 2;
    case 'release':
      return 3;
  }
}

/**
 * Encode under the Kitty keyboard protocol, or `null` for nothing to send.
 *
 * `modes` carries the flags; callers reach this only when `kittyFlags(modes)`
 * is non-zero, which is the check `encodeKey` makes.
 */
export function encodeKitty(
  keyName: string,
  location: number,
  mods: Mods,
  event: EventType,
  modes: number,
): Uint8Array | null {
  const flags = kittyFlags(modes);
  const eventTypes = (flags & 2) !== 0;
  const reportAll = (flags & 8) !== 0;

  // Nothing asked to hear about releases, so there is no form to send one in.
  // A repeat still encodes, as a press -- which is what a program holding a
  // key down has always seen.
  if (!eventTypes && event === 'release') {
    return null;
  }

  // Escape is always the escape form, modified or not. It is the reason to turn
  // flag 1 on at all: a bare 0x1b is indistinguishable from the first byte of
  // every other sequence, so an application has to guess with a timer, and
  // `CSI 27 u` is what removes the guess.
  if (keyName === 'Escape') {
    return kittyU(27, mods, event, eventTypes);
  }

  // The other keys with a legacy byte keep it, because disambiguating `Ctrl+I`
  // from `Tab` does not require changing what `Tab` itself sends -- and
  // changing it would break every program that asked only to disambiguate.
  // Flag 8 is the program asking for the escape code regardless.
  const legacy = legacyByte(keyName);
  if (legacy !== null) {
    const [number, bytes] = legacy;
    const plain = modifierParam(mods) === 1 && !(eventTypes && eventParam(event) !== 1);
    // `plain` is false for anything with an event type to report, so a release
    // always escalates to the escape form rather than falling through to a
    // legacy byte that cannot carry one. Unlike a text key, these have a
    // protocol number to be reported under.
    return reportAll || !plain ? kittyU(number, mods, event, eventTypes) : bytes;
  }

  // Cursor keys keep their CSI letter, never SS3: SS3 has no parameter slot,
  // and a program that asked to disambiguate is owed a form that can carry the
  // modifiers. DECCKM is deliberately ignored here for the same reason, which
  // is why this function does not consult it.
  const letter = csiLetter(keyName);
  if (letter !== null) {
    return kittyLetter(letter, mods, event, eventTypes);
  }
  const tildeN = tildeNumber(keyName);
  if (tildeN !== null) {
    return kittyTilde(tildeN, mods, event, eventTypes);
  }

  // The modifier keys themselves. Only flag 8 asks for these, and without it a
  // bare modifier press must stay silent rather than wake the pty.
  if (reportAll) {
    const n = modifierKeyNumber(keyName, location);
    if (n !== null) {
      return kittyU(n, mods, event, eventTypes);
    }
  }

  // A printable key. Code points, never `.length` -- one astral-plane
  // character is `.length === 2`, and the whole recorded corpus was once blind
  // to exactly that.
  const cp = oneCodePoint(keyName);
  if (cp === null) {
    return null;
  }

  // The protocol keys on the *unshifted* character. Recovering it properly is
  // kitty flag 4, which this terminal does not implement, so lowercasing is the
  // closest honest answer and is exact for the letters that actually collide.
  // Locale-independent `toLowerCase`, matching Rust's `char::to_lowercase`.
  const number = String.fromCodePoint(cp).toLowerCase().codePointAt(0) ?? cp;

  // Plain typing stays plain text even under this protocol -- the program wants
  // the character, not a description of the key. It is Ctrl and Alt that
  // collapse distinct keys onto one byte, and those are what the escape form
  // exists to separate.
  if (reportAll || mods.ctrl || mods.alt) {
    return kittyU(number, mods, event, eventTypes);
  }

  // No escape form for this key, so there is nowhere to put an event type: the
  // release of a key whose press was the byte `a` is not expressible, and
  // sending `a` again would type it twice. Flag 2 without flag 8 therefore
  // reports releases for modified and functional keys only, which is the
  // protocol working as specified rather than a gap. A *repeat* still sends the
  // text, because a held key has always typed.
  if (event === 'release') {
    return null;
  }
  return UTF8.encode(keyName);
}

/**
 * The keys that have a single legacy byte, with their protocol numbers.
 *
 * Escape is handled before this and is deliberately absent: it never keeps its
 * legacy byte under this protocol. Space is absent too, but for a different
 * reason — the DOM reports it as `' '`, a single code point, so it reaches the
 * printable path and comes out as `CSI 32 u`, which is the same answer the
 * Rust's `NamedKey::Space` arm gives.
 */
function legacyByte(keyName: string): [number, Uint8Array] | null {
  switch (keyName) {
    case 'Enter':
      return [13, Uint8Array.of(0x0d)];
    case 'Tab':
      return [9, Uint8Array.of(0x09)];
    case 'Backspace':
      return [127, Uint8Array.of(0x7f)];
    default:
      return null;
  }
}

/** The keys encoded as `CSI 1 ; mods letter`. */
function csiLetter(keyName: string): string | null {
  switch (keyName) {
    case 'ArrowUp':
      return 'A';
    case 'ArrowDown':
      return 'B';
    case 'ArrowRight':
      return 'C';
    case 'ArrowLeft':
      return 'D';
    case 'Home':
      return 'H';
    case 'End':
      return 'F';
    default:
      return null;
  }
}

/**
 * The keys encoded as `CSI n ; mods ~`.
 *
 * F1-F4 are here rather than in `csiLetter`, where their `P`/`Q`/`R`/`S` finals
 * would be shorter: `CSI 1 ; m R` is CPR, the cursor position report. The
 * legacy encoder makes the same choice the moment F3 is modified.
 */
function tildeNumber(keyName: string): number | null {
  switch (keyName) {
    case 'Insert':
      return 2;
    case 'Delete':
      return 3;
    case 'PageUp':
      return 5;
    case 'PageDown':
      return 6;
    case 'F1':
      return 11;
    case 'F2':
      return 12;
    case 'F3':
      return 13;
    case 'F4':
      return 14;
    case 'F5':
      return 15;
    case 'F6':
      return 17;
    case 'F7':
      return 18;
    case 'F8':
      return 19;
    case 'F9':
      return 20;
    case 'F10':
      return 21;
    case 'F11':
      return 23;
    case 'F12':
      return 24;
    default:
      return null;
  }
}

/** `KeyboardEvent.location` for the right-hand instance of a paired key. */
const LOCATION_RIGHT = 2;

/**
 * The protocol's numbers for the modifier keys themselves.
 *
 * Meta/Super is absent on purpose: `belongsToDesktop` refuses the chord before
 * this is reached, so listing it would be a branch that can never run.
 */
function modifierKeyNumber(keyName: string, location: number): number | null {
  const right = location === LOCATION_RIGHT;
  switch (keyName) {
    case 'Shift':
      return right ? 57447 : 57441;
    case 'Control':
      return right ? 57448 : 57442;
    case 'Alt':
      return right ? 57449 : 57443;
    case 'CapsLock':
      return 57358;
    case 'NumLock':
      return 57360;
    default:
      return null;
  }
}

/** The `mods:event` parameter, or `null` when both are at their defaults. */
function kittyParams(mods: Mods, event: EventType, eventTypes: boolean): string | null {
  const m = modifierParam(mods);
  const e = eventParam(event);
  if (eventTypes && e !== 1) {
    return `${m}:${e}`;
  }
  if (m !== 1) {
    return `${m}`;
  }
  return null;
}

/**
 * `CSI number ; mods:event u`.
 *
 * The number is never omitted: `CSI u` with no parameter is SCORC.
 */
function kittyU(number: number, mods: Mods, event: EventType, eventTypes: boolean): Uint8Array {
  const p = kittyParams(mods, event, eventTypes);
  return UTF8.encode(p === null ? `\x1b[${number}u` : `\x1b[${number};${p}u`);
}

/** `CSI 1 ; mods:event letter`, collapsing to `CSI letter` when it can. */
function kittyLetter(
  finalChar: string,
  mods: Mods,
  event: EventType,
  eventTypes: boolean,
): Uint8Array {
  const p = kittyParams(mods, event, eventTypes);
  return UTF8.encode(p === null ? `\x1b[${finalChar}` : `\x1b[1;${p}${finalChar}`);
}

/** `CSI n ; mods:event ~`. */
function kittyTilde(n: number, mods: Mods, event: EventType, eventTypes: boolean): Uint8Array {
  const p = kittyParams(mods, event, eventTypes);
  return UTF8.encode(p === null ? `\x1b[${n}~` : `\x1b[${n};${p}~`);
}

/** The single code point of a printable key, or `null` if it is a named key. */
function oneCodePoint(s: string): number | null {
  let n = 0;
  for (const _ of s) {
    n += 1;
    if (n > 1) return null;
  }
  return n === 1 ? (s.codePointAt(0) ?? null) : null;
}

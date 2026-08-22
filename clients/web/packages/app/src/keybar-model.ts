/**
 * The key bar: the keys a soft keyboard does not have, as data.
 *
 * A touch keyboard has no Esc, no Tab, no Ctrl and no arrows, so a terminal
 * driven from one cannot interrupt a command, recall history or answer a
 * TUI's menu. The phone design (screen 10) answers with a row of key caps
 * above the keyboard; this is that row for the web client (#421).
 *
 * Everything decided here is pure so `node --test` can prove it: which
 * caps exist, what each one means, how a sticky modifier latches, and when
 * the bar shows by default. The bytes come from `encodeKey` — the same
 * encoder a hardware keyboard goes through — fed a synthesised `KeyLike`, so
 * `↑` under DECCKM is `ESC O A` here exactly as it is from a real key, and a
 * new wire mode never needs a second table.
 */

import { Modes } from '@zesterm/proto';
import { encodeKey, mods, NO_MODS, type KeyLike, type Mods } from '@zesterm/input';

/**
 * A sticky modifier's state. `once` applies to the next key and releases;
 * `locked` (a second tap) stays until tapped off — the phone design's
 * "latches, modifies the next key, releases", plus the lock that a `^R`
 * search or a run of `^N`s in a TUI needs.
 */
export type Latch = 'off' | 'once' | 'locked';

export interface LatchState {
  readonly ctrl: Latch;
  readonly alt: Latch;
}

export const NO_LATCH: LatchState = { ctrl: 'off', alt: 'off' };

export type StickyModifier = 'ctrl' | 'alt';

export function tapModifier(state: LatchState, which: StickyModifier): LatchState {
  const next: Latch =
    state[which] === 'off' ? 'once' : state[which] === 'once' ? 'locked' : 'off';
  return { ...state, [which]: next };
}

/**
 * The modifiers a key should carry now, and the latch state after it.
 * Called once per key that actually produced bytes — a bare modifier press
 * must not spend a `once`.
 */
export function consume(state: LatchState): { readonly mods: Mods; readonly next: LatchState } {
  return {
    mods: mods({ ctrl: state.ctrl !== 'off', alt: state.alt !== 'off' }),
    next: {
      ctrl: state.ctrl === 'once' ? 'off' : state.ctrl,
      alt: state.alt === 'once' ? 'off' : state.alt,
    },
  };
}

/** Modifiers from a real key event plus whatever the bar has latched. */
export function mergeMods(event: Mods, latched: Mods): Mods {
  return {
    shift: event.shift,
    ctrl: event.ctrl || latched.ctrl,
    alt: event.alt || latched.alt,
    meta: event.meta,
  };
}

export type CapId =
  | 'esc'
  | 'tab'
  | 'ctrl'
  | 'alt'
  | 'up'
  | 'down'
  | 'left'
  | 'right'
  | 'enter'
  | 'ctrl-c'
  | 'slash'
  | 'dash'
  | 'pipe'
  | 'tilde'
  | 'kbd';

export interface CapDef {
  readonly id: CapId;
  readonly label: string;
  /** Screen-reader name; the label alone is a glyph for half of them. */
  readonly title: string;
}

/**
 * The row, in order. Screen 10's `esc tab ctrl ↑` lead, then what a shell
 * and a TUI actually need under a finger: the other arrows, Enter (the soft
 * keyboard's own return key is not reachable while a cap has focus on some
 * browsers), `^C` as one tap because it is the key people reach for in a
 * hurry, the punctuation iOS hides behind a shift, and the keyboard toggle
 * last — it is the one cap that is not a key.
 */
export const CAPS: readonly CapDef[] = [
  { id: 'esc', label: 'esc', title: 'Escape' },
  { id: 'tab', label: 'tab', title: 'Tab' },
  { id: 'ctrl', label: 'ctrl', title: 'Control (sticky: tap once for the next key, twice to lock)' },
  { id: 'alt', label: 'alt', title: 'Alt (sticky: tap once for the next key, twice to lock)' },
  { id: 'up', label: '↑', title: 'Up' },
  { id: 'down', label: '↓', title: 'Down' },
  { id: 'left', label: '←', title: 'Left' },
  { id: 'right', label: '→', title: 'Right' },
  { id: 'enter', label: '⏎', title: 'Enter' },
  { id: 'ctrl-c', label: '^C', title: 'Control-C (interrupt)' },
  { id: 'slash', label: '/', title: 'Slash' },
  { id: 'dash', label: '-', title: 'Dash' },
  { id: 'pipe', label: '|', title: 'Pipe' },
  { id: 'tilde', label: '~', title: 'Tilde' },
  { id: 'kbd', label: '⌨', title: 'Show or hide the keyboard' },
];

/**
 * What a cap types: the `KeyLike` for `encodeKey` and the modifiers it
 * carries of its own. `null` for the caps that are not keys — the sticky
 * modifiers and the keyboard toggle — so a caller cannot encode them by
 * accident.
 */
export function capKey(id: CapId): { readonly key: KeyLike; readonly mods: Mods } | null {
  const named = (key: string): { key: KeyLike; mods: Mods } => ({
    key: { key, code: key },
    mods: NO_MODS,
  });
  switch (id) {
    case 'esc':
      return named('Escape');
    case 'tab':
      return named('Tab');
    case 'up':
      return named('ArrowUp');
    case 'down':
      return named('ArrowDown');
    case 'left':
      return named('ArrowLeft');
    case 'right':
      return named('ArrowRight');
    case 'enter':
      return named('Enter');
    case 'ctrl-c':
      return { key: { key: 'c', code: 'KeyC' }, mods: mods({ ctrl: true }) };
    case 'slash':
      return { key: { key: '/', code: 'Slash' }, mods: NO_MODS };
    case 'dash':
      return { key: { key: '-', code: 'Minus' }, mods: NO_MODS };
    case 'pipe':
      return { key: { key: '|', code: 'Backslash' }, mods: mods({ shift: true }) };
    case 'tilde':
      return { key: { key: '~', code: 'Backquote' }, mods: mods({ shift: true }) };
    case 'ctrl':
    case 'alt':
    case 'kbd':
      return null;
  }
}

/**
 * A cap tap, resolved: the bytes to send (if any) and the latch afterwards.
 *
 * A modifier cap only moves the latch. A key cap merges the latch into its
 * own modifiers, encodes under the session's live `modes`, and spends any
 * `once` — but only when something was actually encoded, so a cap the
 * encoder declines (none today, but `encodeKey` may return null) cannot eat
 * a pending Ctrl. `kbd` is the caller's: it is about focus, not bytes.
 */
export function tapCap(
  id: CapId,
  latch: LatchState,
  modes: number,
): { readonly bytes: Uint8Array | null; readonly next: LatchState } {
  if (id === 'ctrl' || id === 'alt') return { bytes: null, next: tapModifier(latch, id) };
  const cap = capKey(id);
  if (cap === null) return { bytes: null, next: latch };
  const { mods: latched, next } = consume(latch);
  const bytes = encodeKey(cap.key, mergeMods(cap.mods, latched), modes);
  return bytes === null ? { bytes: null, next: latch } : { bytes, next };
}

/** Re-exported so the one test that needs a mode bit reads naturally. */
export { Modes };

/**
 * Whether to show the bar before anyone has chosen.
 *
 * Touch capability, never the platform string: iPadOS Safari reports
 * `MacIntel`, and a Windows laptop with a touch screen reports `Win32` —
 * both are devices whose keyboard may be on the glass. `(pointer: coarse)`
 * is the primary pointer; `maxTouchPoints` catches a tablet driven by a
 * trackpad that still has no Esc key on its screen keyboard.
 */
export function keyBarDefault(env: {
  readonly coarsePointer: boolean;
  readonly maxTouchPoints: number;
}): boolean {
  return env.coarsePointer || env.maxTouchPoints > 0;
}

export interface StorageLike {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}

export const KEYBAR_KEY = 'zesterm.keybar';

/**
 * The stored choice, or the device default when nothing was chosen. Only
 * the two exact strings count — anything else is "never chosen", so a value
 * from another version cannot hide the bar on a device that needs it.
 */
export function loadKeyBar(storage: StorageLike, fallback: boolean): boolean {
  const v = storage.getItem(KEYBAR_KEY);
  return v === 'on' ? true : v === 'off' ? false : fallback;
}

export function saveKeyBar(storage: StorageLike, on: boolean): void {
  storage.setItem(KEYBAR_KEY, on ? 'on' : 'off');
}

/**
 * The modifier set, mirroring `winit::keyboard::ModifiersState` as the Rust
 * encoder consumes it: four booleans, where `meta` is winit's Super — Command
 * on macOS, the Windows key elsewhere. A plain object rather than a bitset
 * because nothing here is wire-bound; the bits that matter (the xterm modifier
 * parameter) are computed at the one place they are emitted, in `key.ts`.
 */
export interface Mods {
  readonly shift: boolean;
  readonly ctrl: boolean;
  readonly alt: boolean;
  readonly meta: boolean;
}

export const NO_MODS: Mods = { shift: false, ctrl: false, alt: false, meta: false };

/** A `Mods` with only the named modifiers set. Exists so tests read like the Rust ones. */
export function mods(m: Partial<Mods> = {}): Mods {
  return { ...NO_MODS, ...m };
}

/**
 * Build `Mods` from a KeyboardEvent. The parameter is structural — the four
 * boolean fields, not the DOM type — so this package (and its tests) need no
 * DOM lib at all.
 */
export function modsOf(e: {
  shiftKey: boolean;
  ctrlKey: boolean;
  altKey: boolean;
  metaKey: boolean;
}): Mods {
  return { shift: e.shiftKey, ctrl: e.ctrlKey, alt: e.altKey, meta: e.metaKey };
}

/**
 * Which chords are the browser's (or the app shell's), not the shell-on-the-
 * other-end's. Ports `belongs_to_desktop` and `is_clipboard_chord` from
 * `crates/zest-input/src/key.rs`, plus the one decision the browser adds on top.
 */

import type { Mods } from './mods.ts';

/**
 * Whether a chord belongs to the desktop rather than to the terminal.
 *
 * The terminal modifier set is Ctrl, Alt and Shift. There is no escape
 * sequence for Super/Command and no application expects one, so a chord that
 * includes it is the window manager's or the app's — never the shell's.
 */
export function belongsToDesktop(mods: Mods): boolean {
  return mods.meta;
}

/**
 * Whether this chord means copy/paste.
 *
 * Two conventions, both accepted everywhere. `Ctrl+Shift+C/V` is what
 * terminals use on Windows and Linux, and it exists only because plain
 * `Ctrl+C` must stay SIGINT. macOS has no such conflict and universally uses
 * Command — a terminal accepting only `Ctrl+Shift` there is one where paste
 * appears not to work at all.
 *
 * Accepting both rather than choosing by platform: the other chord is unused
 * on each platform, and accepting it costs nothing against someone's muscle
 * memory from another machine.
 */
export function isClipboardChord(mods: Mods): boolean {
  return mods.meta || (mods.ctrl && mods.shift);
}

/**
 * Whether the app should leave this event alone: no `preventDefault`, no bytes.
 *
 * Meta-chords are never the shell's (see `belongsToDesktop`), and in a browser
 * they are usually not even the page's — Cmd+T, Cmd+W, Cmd+V all mean things
 * the tab must not steal.
 *
 * `Ctrl+Shift+C/V` is released only on non-mac platforms, where it *is* the
 * terminal clipboard convention and the app shell will handle it. This is
 * narrower than the Rust `is_clipboard_chord`, deliberately: the native app
 * accepts both conventions because it implements the clipboard itself, but
 * returning true here means *nobody* acts — on macOS neither the browser nor
 * the shell claims `Ctrl+Shift+C`, so releasing it there would make the chord
 * simply dead. On a Mac it stays with the terminal and types 0x03, exactly as
 * the Rust encoder would.
 */
export function belongsToBrowser(
  key: { readonly key: string },
  mods: Mods,
  platform: 'mac' | 'other',
): boolean {
  if (belongsToDesktop(mods)) {
    return true;
  }
  if (platform === 'other' && mods.ctrl && mods.shift) {
    const k = key.key;
    return k === 'c' || k === 'C' || k === 'v' || k === 'V';
  }
  return false;
}

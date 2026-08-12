/**
 * The app shell's own chord table — which keystrokes the shell claims before
 * anything else sees them. Handler ordering is the contract: the app calls
 * `shellChord` BEFORE `belongsToBrowser`, so a chord in this table never
 * reaches the browser split or the terminal encoder.
 *
 * ⚠ On Windows/Linux this table steals readline's Ctrl+K (kill-to-end-of-line)
 * from the shell BY DESIGN — the handoff mandates that Ctrl+K opens the
 * palette, and the palette is the product's front door. Rejected alternative:
 * teaching `belongsToBrowser` about Ctrl+K, which other consumers pin — that
 * function answers "is this the browser's/OS's", and the palette chord is
 * neither; it is the app's, which is exactly what handler ordering expresses.
 *
 * Matching is on `code` (the physical key), not `key`: with Shift held the
 * `key` of Comma is `<` on a US layout and something else on every other one,
 * while `Comma` is `Comma` everywhere — ⌘⇧, must not depend on the keymap.
 */

import type { Mods } from './mods.ts';

export type ShellAction =
  | { readonly kind: 'palette' }
  | { readonly kind: 'split' }
  | { readonly kind: 'layout-toggle' }
  | { readonly kind: 'settings' }
  | { readonly kind: 'profiles' }
  | { readonly kind: 'copy-output' }
  | { readonly kind: 're-run' }
  | { readonly kind: 'tab-n'; readonly n: number };

/**
 * The shell action a chord means, or null when the chord is not the shell's.
 *
 * `mod` is Command on macOS and Ctrl elsewhere. The match is exact: the other
 * platform's mod, or Alt, disqualifies — ⌘K with Ctrl also held is not the
 * palette, and Ctrl+K on a Mac stays readline's kill-to-eol because Command
 * is available there and nothing needs to be stolen. Without `mod` the answer
 * is always null, so a bare printable or a Shift-only combination can never
 * be claimed here.
 */
export function shellChord(
  key: { readonly key: string; readonly code: string },
  mods: Mods,
  platform: 'mac' | 'other',
): ShellAction | null {
  const mod = platform === 'mac' ? mods.meta : mods.ctrl;
  const foreignMod = platform === 'mac' ? mods.ctrl : mods.meta;
  if (!mod || foreignMod || mods.alt) return null;

  if (mods.shift) {
    switch (key.code) {
      case 'KeyE':
        return { kind: 'layout-toggle' };
      case 'Comma':
        return { kind: 'profiles' };
      case 'KeyO':
        return { kind: 'copy-output' };
      case 'KeyR':
        return { kind: 're-run' };
      default:
        return null;
    }
  }

  switch (key.code) {
    case 'KeyK':
      return { kind: 'palette' };
    case 'KeyD':
      return { kind: 'split' };
    case 'Comma':
      return { kind: 'settings' };
    default:
      break;
  }

  // Digit1..Digit9 — the top row; Digit0 stays unclaimed, there is no tab 0.
  if (key.code.startsWith('Digit')) {
    const n = key.code.charCodeAt(5) - 0x30;
    if (n >= 1 && n <= 9) return { kind: 'tab-n', n };
  }
  return null;
}

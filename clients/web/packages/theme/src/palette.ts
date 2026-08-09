/**
 * Token record → the 256-color terminal palette —
 * `crates/zest-theme/src/resolve.rs`'s `resolve`, for the tokens-only case.
 *
 * The Rust `resolve` takes a full `Theme`, whose `ansi`/`terminal` blocks can
 * override everything here; this client-side function takes bare `UiTokens`,
 * so it is exactly Rust's *default* derivation path. One documented
 * divergence follows from that: builtin.rs stores explicit brights computed as
 * `normal.map(brighten_ansi)` for all eight, while the tokens-only default
 * (resolve.rs) uses the conventional exceptions — bright black is the theme's
 * faintest readable grey (`ui.faint`), bright white its plain foreground
 * (`ui.fg`). This function follows resolve.rs, so brights 0 and 7 can differ
 * from the native app's builtins until the wire carries the `ansi` block.
 */

import type { UiTokens } from './tokens.ts';
import { brightenAnsi } from './oklch.ts';

export interface TerminalPalette {
  defaultFg: string;
  defaultBg: string;
  /** ANSI 0-15: the theme's normal eight, then the derived brights. */
  ansi: string[];
  /** Any palette index 0-255: ansi, then the 6x6x6 cube, then the gray ramp. */
  indexed(n: number): string;
}

// The xterm cube's channel levels — convention, not taste, so computed rather
// than stored (240 hex strings per theme is how a format dies).
const CUBE_LEVELS = [0, 95, 135, 175, 215, 255] as const;

function hexByte(v: number): string {
  return v.toString(16).padStart(2, '0');
}

export function resolveTerminalPalette(tokens: UiTokens): TerminalPalette {
  // 0..8 come from the theme's own ANSI names: a theme authored purely in the
  // sigx vocabulary is already complete.
  const normal = [
    tokens.black,
    tokens.red,
    tokens.green,
    tokens.yellow,
    tokens.blue,
    tokens.magenta,
    tokens.cyan,
    tokens.white,
  ];

  // 8..16: an OKLCH lift, not a blend toward white (see oklch.ts). The two
  // exceptions are conventional rather than computed — see the module doc.
  const bright = [
    tokens.faint,
    ...normal.slice(1, 7).map(brightenAnsi),
    tokens.fg,
  ];

  const ansi = [...normal, ...bright];

  return {
    defaultFg: tokens.fg,
    defaultBg: tokens.bg,
    ansi,
    indexed(n: number): string {
      if (!Number.isInteger(n) || n < 0 || n > 255) {
        throw new RangeError(`palette index out of range: ${n}`);
      }
      if (n < 16) return ansi[n]!;
      if (n < 232) {
        // 16 + 36r + 6g + b, channels indexing CUBE_LEVELS.
        const i = n - 16;
        const r = CUBE_LEVELS[Math.floor(i / 36)]!;
        const g = CUBE_LEVELS[Math.floor(i / 6) % 6]!;
        const b = CUBE_LEVELS[i % 6]!;
        return `#${hexByte(r)}${hexByte(g)}${hexByte(b)}`;
      }
      // 232..255: the grayscale ramp, 8 + 10 per step.
      const v = 8 + (n - 232) * 10;
      return `#${hexByte(v)}${hexByte(v)}${hexByte(v)}`;
    },
  };
}

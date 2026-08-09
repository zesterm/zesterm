/**
 * The bridge to `@sigx/terminal-zero`.
 *
 * zest-theme's `UiTokens` is terminal-zero's `Theme` record minus
 * `name`/`mode` — a contract, not a coincidence (tokens.rs pins it with
 * `deny_unknown_fields`). So registering a zesterm theme with sigx is a
 * spread, not a mapping. This module exists so the spread happens in exactly
 * one place, where the contract test can watch it.
 */

import type { Theme, ThemeMode, UiTokens } from './tokens.ts';
import { TOKEN_KEYS } from './tokens.ts';

/** The record sigx's `registerTheme(id, theme)` accepts. */
export type SigxTheme = { name: string; mode: ThemeMode } & UiTokens;

/**
 * Built key-by-key from `TOKEN_KEYS` rather than `{...theme.ui}` so a stray
 * extra property on a `ui` object that arrived over the wire cannot leak into
 * sigx — `deny_unknown_fields` guards the Rust boundary, this guards ours.
 */
export function toSigxTheme(theme: Theme): SigxTheme {
  const out = { name: theme.name, mode: theme.mode } as SigxTheme;
  for (const key of TOKEN_KEYS) {
    out[key] = theme.ui[key];
  }
  return out;
}

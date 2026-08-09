/**
 * Token record → CSS custom properties.
 *
 * The client-UI handoff (docs/design/client-ui/README.md) drives chrome off
 * variables named straight from the token record — it pins `--zt-bg` and
 * `--zt-panel` and leaves the rest implied. The mapping here is mechanical:
 * `--zt-` plus the token key, camelCase folded to kebab-case
 * (`accentSoft` → `--zt-accent-soft`), because CSS custom properties are
 * case-sensitive and hand-written stylesheets reach for kebab by reflex — a
 * camelCase variable there would fail silently as an unset var.
 */

import type { UiTokens } from './tokens.ts';
import { TOKEN_KEYS } from './tokens.ts';

/** `accentSoft` → `--zt-accent-soft`. */
export function cssVarName(token: keyof UiTokens): string {
  return `--zt-${token.replace(/[A-Z]/g, (ch) => `-${ch.toLowerCase()}`)}`;
}

/** One entry per token — exactly 24 — ready for a stylesheet or inline style. */
export function cssVarsOf(tokens: UiTokens): Record<string, string> {
  const out: Record<string, string> = {};
  for (const key of TOKEN_KEYS) {
    out[cssVarName(key)] = tokens[key];
  }
  return out;
}

/**
 * The slice of `CSSStyleDeclaration` this package needs. Structural on
 * purpose: node tests pass a plain object and no DOM types are required.
 */
export interface StyleTarget {
  style: { setProperty(name: string, value: string): void };
}

/** Apply all 24 variables to an element (typically `document.documentElement`). */
export function applyCssVars(tokens: UiTokens, el: StyleTarget): void {
  for (const [name, value] of Object.entries(cssVarsOf(tokens))) {
    el.style.setProperty(name, value);
  }
}

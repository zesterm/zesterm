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
import { blockHeaderFill, softHairline, titlebarFill } from './derived.ts';

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

/**
 * The three derived chrome surfaces as CSS variables. Separate from
 * `cssVarsOf` because these are not tokens — they are computed from tokens
 * (`derived.ts`), and stylesheets that reach for `--zt-titlebar` must get the
 * per-theme derivation, never a literal that would paint a dark bar onto
 * `paper`.
 */
export function derivedCssVars(ui: UiTokens): Record<string, string> {
  return {
    '--zt-titlebar': titlebarFill(ui),
    '--zt-block-header': blockHeaderFill(ui),
    '--zt-hairline': softHairline(ui),
  };
}

/**
 * The full theme, onto an element: the 24 token variables plus the 3 derived
 * surfaces — 27 properties. This is the one call a theme *switch* re-runs;
 * `applyCssVars` alone would leave the chrome surfaces painted in the old
 * theme's derivation.
 */
export function applyThemeCss(ui: UiTokens, el: StyleTarget): void {
  applyCssVars(ui, el);
  for (const [name, value] of Object.entries(derivedCssVars(ui))) {
    el.style.setProperty(name, value);
  }
}

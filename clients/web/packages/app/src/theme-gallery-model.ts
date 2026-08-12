/**
 * Pure theme-gallery logic (design §8) — one card per theme, with every
 * derived value derived HERE so the component only turns cards into elements.
 *
 * The swatch strip is the theme's normal ANSI row in index order, read from
 * `resolveTerminalPalette` rather than re-typed — the same derivation the
 * grid paints with, so the strip cannot drift from what the terminal shows.
 */

import {
  DEFAULT_DARK,
  DEFAULT_LIGHT,
  resolveTerminalPalette,
  type Theme,
  type UiTokens,
} from '@zesterm/theme';

export interface ThemeCard {
  readonly id: string;
  readonly name: string;
  /** Footer qualifier: the mode, `· default` appended for the two defaults. */
  readonly qualifier: string;
  /** ANSI 0–7 in index order — brights are derived and never shown (§8). */
  readonly swatches: readonly string[];
  /**
   * The theme's own tokens, for the live preview. The preview is rendered in
   * THIS theme's bg/fg/green/blue/red as inline styles — the one place a
   * colour value legitimately bypasses the page's `--zt-*` variables, because
   * a preview painted in the page theme's colours previews nothing.
   */
  readonly ui: UiTokens;
}

export function themeCards(themes: readonly Theme[]): readonly ThemeCard[] {
  return themes.map((t) => ({
    id: t.id,
    name: t.name,
    qualifier:
      t.id === DEFAULT_DARK || t.id === DEFAULT_LIGHT ? `${t.mode} · default` : t.mode,
    swatches: resolveTerminalPalette(t.ui).ansi.slice(0, 8),
    ui: t.ui,
  }));
}

/**
 * `@zesterm/theme` — zest-theme's token vocabulary for the web client.
 *
 * The `ui` record is `@sigx/terminal-zero`'s token contract verbatim; the
 * built-ins mirror `crates/zest-theme/src/builtin.rs`; the derived surfaces
 * and the terminal palette run the same math as the Rust crate so the browser
 * tab and the native window agree pixel-for-pixel on what a theme looks like.
 */

export type { UiTokens, Theme, ThemeMode } from './tokens.ts';
export { TOKEN_KEYS } from './tokens.ts';
export {
  builtinThemes,
  themeById,
  obsidian,
  nord,
  gum,
  classic,
  paper,
  DEFAULT_DARK,
  DEFAULT_LIGHT,
} from './builtin.ts';
export {
  cssVarName,
  cssVarsOf,
  applyCssVars,
  derivedCssVars,
  applyThemeCss,
  type StyleTarget,
} from './css.ts';
export { toSigxTheme, type SigxTheme } from './sigx.ts';
export { resolveTerminalPalette, type TerminalPalette } from './palette.ts';
export { titlebarFill, blockHeaderFill, softHairline } from './derived.ts';

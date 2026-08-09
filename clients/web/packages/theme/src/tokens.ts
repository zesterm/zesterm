/**
 * The theme token schema — `crates/zest-theme/src/tokens.rs` in TypeScript.
 *
 * `UiTokens` is `@sigx/terminal-zero`'s `Theme` record field-for-field and
 * case-for-case, minus `name`/`mode` which live on the outer `Theme`. That is a
 * contract, not a coincidence: `{...theme.ui, name, mode}` must be a valid
 * argument to sigx's `registerTheme()`. The Rust side serializes with
 * `#[serde(rename_all = "camelCase")]`, so these property names are exactly
 * what crosses the wire.
 *
 * Colors are `"#rrggbb"` (or `"#rrggbbaa"`) strings — the serialized form of
 * Rust's `Rgba8` — because that is what both sigx and CSS consume directly.
 */

/** Serialized form of zest-theme's `ThemeMode` (`rename_all = "lowercase"`). */
export type ThemeMode = 'dark' | 'light';

/**
 * terminal-zero's token contract, verbatim. The trailing eight color names are
 * terminal-zero's ANSI set; they double as zesterm's *normal* ANSI palette, so
 * a theme authored purely in the sigx vocabulary is already a complete 8-color
 * terminal theme.
 */
export interface UiTokens {
  // surfaces
  /** Canvas / terminal background. */
  bg: string;
  /** Raised surface: title-bar cards, palette body, sidebar rows. */
  panel: string;
  /** Window background behind panes; title bars / footers. */
  chrome: string;
  /** Idle borders and rules — every 1px border. */
  line: string;
  /** Drop-shadow tone; strength is alpha, a rendering concern. */
  shadow: string;

  // text
  /** Default text. */
  fg: string;
  /** Muted text: secondary labels, inactive tab titles. */
  dim: string;
  /** Faintest text: metadata, timestamps, progress track. */
  faint: string;

  // accent (focus + selection)
  /** Focus ring, cursor, links, active-tab rule. */
  accent: string;
  /** Subtle focus tint behind content (selected rows, chips). */
  accentSoft: string;
  /** Text on a solid accent fill. */
  accentText: string;
  /** Selected-row / hover tint. */
  selSoft: string;

  // status
  success: string;
  warn: string;
  danger: string;
  info: string;

  // ansi (terminal-zero's eight; zesterm's normal ANSI 0-7 in index order)
  black: string;
  red: string;
  green: string;
  yellow: string;
  blue: string;
  magenta: string;
  cyan: string;
  white: string;
}

/**
 * A theme, as the clients consume it.
 *
 * zest-theme's full `Theme` also carries `schema`, `ansi`, `terminal` and
 * `effects`; those are authoring/serialization concerns and everything in them
 * has a derivation from `ui` (see `palette.ts`), so the client-side record
 * stays at the four fields every screen actually needs.
 */
export interface Theme {
  /** Stable key used by settings (`theme = "obsidian"`). */
  id: string;
  /** Display name. */
  name: string;
  mode: ThemeMode;
  ui: UiTokens;
}

/**
 * The 24 token names, in the declaration order of `tokens.rs`. A single
 * authoritative list so `cssVarsOf` and the contract tests iterate the record
 * rather than each hand-maintaining a copy that can drift.
 */
export const TOKEN_KEYS = [
  'bg',
  'panel',
  'chrome',
  'line',
  'shadow',
  'fg',
  'dim',
  'faint',
  'accent',
  'accentSoft',
  'accentText',
  'selSoft',
  'success',
  'warn',
  'danger',
  'info',
  'black',
  'red',
  'green',
  'yellow',
  'blue',
  'magenta',
  'cyan',
  'white',
] as const satisfies readonly (keyof UiTokens)[];

// If TOKEN_KEYS misses a UiTokens key the two types stop being mutually
// assignable and this line fails typecheck — the array cannot silently lag the
// interface.
type _TokenKeysCoverUiTokens = keyof UiTokens extends (typeof TOKEN_KEYS)[number]
  ? true
  : never;
const _tokenKeysCoverUiTokens: _TokenKeysCoverUiTokens = true;
void _tokenKeysCoverUiTokens;

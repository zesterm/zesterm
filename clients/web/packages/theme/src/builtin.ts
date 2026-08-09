/**
 * The built-in themes — `crates/zest-theme/src/builtin.rs`, values verbatim.
 *
 * Every hex below was hand-copied from the Rust crate's output (obsidian's
 * `ui` record is authored literally in builtin.rs; the other four are derived
 * there by `resolve::derive_ui_tokens`, so their values were captured by
 * serializing the crate's `builtin::all()`). Drift here means zesterm's native
 * window and the web client disagree about what "obsidian" looks like — the
 * future anti-drift fix is an xtask that exports these records from the Rust
 * crate instead of a human copying them.
 */

import type { Theme } from './tokens.ts';

/**
 * sigx's default dark theme. The `ui` record is `@sigx/terminal-zero`'s
 * registered obsidian *verbatim* — the client-UI design
 * (docs/design/client-ui/) is specified against these exact values.
 */
export const obsidian: Theme = {
  id: 'obsidian',
  name: 'Obsidian',
  mode: 'dark',
  ui: {
    bg: '#0b0f1a',
    panel: '#121829',
    chrome: '#0b0f1a',
    line: '#2a3350',
    shadow: '#000000',
    fg: '#d7dcea',
    dim: '#8b93a7',
    faint: '#4a516a',
    accent: '#6ea8ff',
    accentSoft: '#16203a',
    accentText: '#0b0f1a',
    selSoft: '#161d33',
    success: '#5fd17f',
    warn: '#e0b341',
    danger: '#e0606a',
    info: '#5fc4e0',
    black: '#0b0f1a',
    red: '#e0606a',
    green: '#5fd17f',
    yellow: '#e0b341',
    blue: '#6ea8ff',
    magenta: '#b07cff',
    cyan: '#5fc4e0',
    white: '#d7dcea',
  },
};

export const nord: Theme = {
  id: 'nord',
  name: 'Nord',
  mode: 'dark',
  ui: {
    bg: '#2e3440',
    panel: '#363c48',
    chrome: '#323844',
    line: '#434955',
    shadow: '#000000',
    fg: '#d8dee9',
    dim: '#989eaa',
    faint: '#69707c',
    accent: '#81a1c1',
    accentSoft: '#3a4352',
    accentText: '#2e3440',
    selSoft: '#404a5a',
    success: '#a3be8c',
    warn: '#ebcb8b',
    danger: '#bf616a',
    info: '#88c0d0',
    black: '#3b4252',
    red: '#bf616a',
    green: '#a3be8c',
    yellow: '#ebcb8b',
    blue: '#81a1c1',
    magenta: '#b48ead',
    cyan: '#88c0d0',
    white: '#e5e9f0',
  },
};

export const gum: Theme = {
  id: 'gum',
  name: 'Gum',
  mode: 'dark',
  ui: {
    bg: '#1f1b2a',
    panel: '#262232',
    chrome: '#231f2e',
    line: '#373242',
    shadow: '#000000',
    fg: '#e6e1f0',
    dim: '#9a95a5',
    faint: '#635f6f',
    accent: '#7aa2ff',
    accentSoft: '#302b45',
    accentText: '#1f1b2a',
    selSoft: '#383353',
    success: '#5ce6a8',
    warn: '#ffc857',
    danger: '#ff5c8a',
    info: '#5fd7e0',
    black: '#2b2536',
    red: '#ff5c8a',
    green: '#5ce6a8',
    yellow: '#ffc857',
    blue: '#7aa2ff',
    magenta: '#c792ea',
    cyan: '#5fd7e0',
    white: '#e6e1f0',
  },
};

/** The classic dark terminal palette — close to xterm's own. */
export const classic: Theme = {
  id: 'classic',
  name: 'Classic',
  mode: 'dark',
  ui: {
    bg: '#000000',
    panel: '#090909',
    chrome: '#050505',
    line: '#171717',
    shadow: '#000000',
    fg: '#e5e5e5',
    dim: '#808080',
    faint: '#3a3a3a',
    accent: '#0000ee',
    accentSoft: '#050002',
    accentText: '#e5e5e5',
    selSoft: '#0d0008',
    success: '#00cd00',
    warn: '#cdcd00',
    danger: '#cd0000',
    info: '#00cdcd',
    black: '#000000',
    red: '#cd0000',
    green: '#00cd00',
    yellow: '#cdcd00',
    blue: '#0000ee',
    magenta: '#cd00cd',
    cyan: '#00cdcd',
    white: '#e5e5e5',
  },
};

/** The light theme. */
export const paper: Theme = {
  id: 'paper',
  name: 'Paper',
  mode: 'light',
  ui: {
    bg: '#faf9f5',
    panel: '#f0efeb',
    chrome: '#f5f4f0',
    line: '#d8d9d4',
    shadow: '#000000',
    fg: '#24292f',
    dim: '#646c6c',
    faint: '#9fa4a1',
    accent: '#1f6feb',
    accentSoft: '#e1e8cf',
    accentText: '#faf9f5',
    selSoft: '#d0e2c1',
    success: '#27803a',
    warn: '#9a6b00',
    danger: '#c0392b',
    info: '#1b7c83',
    black: '#2b2b2b',
    red: '#c0392b',
    green: '#27803a',
    yellow: '#9a6b00',
    blue: '#1f6feb',
    magenta: '#8250df',
    cyan: '#1b7c83',
    white: '#6e7781',
  },
};

/** All built-ins, in builtin.rs's `IDS` order — the theme picker's order. */
export const builtinThemes: readonly Theme[] = [obsidian, nord, gum, classic, paper];

/** The defaults when nothing is configured (builtin.rs's `DEFAULT_DARK`/`_LIGHT`). */
export const DEFAULT_DARK = 'obsidian';
export const DEFAULT_LIGHT = 'paper';

/** Look up a built-in by its stable id, or `undefined` — builtin.rs's `get`. */
export function themeById(id: string): Theme | undefined {
  return builtinThemes.find((t) => t.id === id);
}

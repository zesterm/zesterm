/**
 * The theme store: the client's theme choice, kept and made live.
 *
 * The mockup's client-side state list says the theme id is the client's to
 * keep — it never crosses the wire. This module owns the whole lifecycle:
 * the localStorage read, the fallback, applying the CSS variables, the
 * boot-bg first-paint cache, and telling live components a switch happened.
 *
 * No DOM access at module scope: the DOM-shaped things (`document`,
 * `localStorage`) arrive as constructor arguments, so `node --test` can
 * import this file and drive it with fakes.
 */

import {
  applyThemeCss,
  themeById,
  DEFAULT_DARK,
  type StyleTarget,
  type Theme,
} from '@zesterm/theme';

/**
 * The slice of `Storage` the store needs. Structural for the same reason as
 * `StyleTarget`: node tests pass a Map-backed fake and no DOM lib is required.
 */
export interface StorageLike {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}

const THEME_KEY = 'zesterm.theme';
const BOOT_BG_KEY = 'zesterm.boot-bg';

export interface ThemeStore {
  /** The theme currently applied. */
  readonly theme: Theme;
  /** Switch themes; an unknown id falls back to the default dark theme. */
  setTheme(id: string): void;
  /** Hear about switches; returns the unsubscribe. */
  onThemeChange(cb: (theme: Theme) => void): () => void;
}

export function createThemeStore(el: StyleTarget, storage: StorageLike): ThemeStore {
  const subscribers = new Set<(theme: Theme) => void>();
  // DEFAULT_DARK is an id, and it names a built-in — builtin.generated.ts
  // emits both from the same Rust source, so the lookup cannot miss.
  const fallback = themeById(DEFAULT_DARK)!;

  const apply = (theme: Theme): void => {
    applyThemeCss(theme.ui, el);
    // Cache the resolved background for the next load's first paint —
    // index.html replays it before the bundle exists, so a light theme does
    // not flash dark. Every switch must rewrite it, or the *next* visit
    // boots painted in the previous theme's bg.
    try {
      storage.setItem(BOOT_BG_KEY, theme.ui.bg);
    } catch {
      // Private modes throw on write; the inline fallback stands.
    }
  };

  let current = themeById(storage.getItem(THEME_KEY) ?? '') ?? fallback;
  apply(current);

  return {
    get theme() {
      return current;
    },
    setTheme(id: string): void {
      current = themeById(id) ?? fallback;
      // Persist the *resolved* id: writing an unknown id through would make
      // every later boot silently fall back while the setting claims otherwise.
      try {
        storage.setItem(THEME_KEY, current.id);
      } catch {
        // Same private-mode posture as boot-bg: the switch still applies.
      }
      apply(current);
      for (const cb of [...subscribers]) cb(current);
    },
    onThemeChange(cb: (theme: Theme) => void): () => void {
      subscribers.add(cb);
      return () => void subscribers.delete(cb);
    },
  };
}

// The app-wide store, behind an init rather than constructed here: module
// scope must stay DOM-free (see the header), and components that cannot be
// handed the instance through props (TerminalView subscribes directly) need
// one agreed place to find it.
let active: ThemeStore | null = null;

export function initThemeStore(el: StyleTarget, storage: StorageLike): ThemeStore {
  active = createThemeStore(el, storage);
  return active;
}

/** `null` before boot — callers treat "no store" as "the theme never changes". */
export function themeStore(): ThemeStore | null {
  return active;
}

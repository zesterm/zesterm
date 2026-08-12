/**
 * Horizontal vs vertical tabs, persisted per window. The design's State
 * section makes layout client-side per-window state; when a settings
 * transport exists, `tabs.position` becomes the authority and this store
 * its cache — which is why the whole module is three small functions over
 * a storage interface rather than anything a migration would have to unpick.
 */

export type Layout = 'horizontal' | 'vertical';

/**
 * Structurally `window.localStorage`, but tests pass a plain object — the
 * point is that nothing here needs a DOM lib.
 */
export interface StorageLike {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}

export const LAYOUT_KEY = 'zesterm.layout';

/**
 * Anything that is not exactly 'vertical' — absent, garbage, a value from a
 * future version — falls back to the default rather than throwing: a broken
 * stored preference must never keep the window from opening.
 */
export function loadLayout(storage: StorageLike): Layout {
  return storage.getItem(LAYOUT_KEY) === 'vertical' ? 'vertical' : 'horizontal';
}

export function saveLayout(storage: StorageLike, layout: Layout): void {
  storage.setItem(LAYOUT_KEY, layout);
}

export function toggleLayout(layout: Layout): Layout {
  return layout === 'horizontal' ? 'vertical' : 'horizontal';
}

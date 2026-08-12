/**
 * Pure chrome logic — everything the tab strip, sidebar and launcher decide
 * that is not pixels: chip labels, launcher rows, the scroll-into-view
 * predicate, the icon-rail breakpoint. Plain data in, plain data out, so all
 * of it runs under `node --test` with no renderer in the room; the components
 * stay thin over these functions.
 */

import type { Tab } from './state/tabs.ts';

/**
 * The one mono family for the grid AND the chrome's mono text (cwd lines, key
 * caps, sub-labels). The handoff's typography table is explicit that chrome
 * mono reuses the grid font rather than shipping a second one; sharing the
 * constant is what keeps a drift from being possible. `entry-client` mirrors
 * it into `--zt-mono` for the stylesheet, and `TerminalView` measures glyphs
 * with it directly.
 */
export const MONO_FAMILY = 'ui-monospace, "SF Mono", Menlo, Consolas, monospace';

/**
 * The grid's font size in CSS pixels. Shared for the same reason as the
 * family: `TerminalView` sizes the pty off these metrics and `GridPane` sizes
 * its canvas off them — two literals would let the pty and the painted grid
 * disagree by a row.
 */
export const GRID_FONT_SIZE = 13;

/**
 * A tab chip shows its title ONLY — host and cwd live in the tooltip and the
 * vertical sidebar/header, which have room for them (design §1). An untitled
 * session reads as `shell`, never as a blank chip.
 */
export function chipTitle(tab: Pick<Tab, 'title'>): string {
  return tab.title === '' ? 'shell' : tab.title;
}

/** A host id is 64 hex chars; twelve are enough to tell machines apart on screen. */
export function shortHostId(id: string): string {
  return id.length <= 12 ? id : id.slice(0, 12);
}

/**
 * The chip's tooltip — where host and cwd live, since the 34px chip has no
 * room for a second line. The label falls back to the shortened host id so a
 * host the directory has not named yet is still identifiable.
 */
export function chipTooltip(
  tab: Pick<Tab, 'title' | 'hostId' | 'cwd'>,
  hostLabel?: string,
): string {
  const host = hostLabel ?? shortHostId(tab.hostId);
  return tab.cwd === ''
    ? `${chipTitle(tab)} — ${host}`
    : `${chipTitle(tab)} — ${host} · ${tab.cwd}`;
}

/**
 * One tab per (host, session) pair — the FULL pair, like folds: session ids
 * are per daemon, so `studio`'s session 3 and `forge`'s session 3 are
 * different terminals. ':' cannot appear in a hex host id or a decimal
 * session id, so no pair collides with another by shuffling characters
 * across the join.
 */
export function tabIdOf(hostId: string, sessionId: string): string {
  return `${hostId}:${sessionId}`;
}

/** A machine a session can be started on, as the launcher needs it. */
export interface HostChoice {
  readonly id: string;
  readonly label: string;
}

/**
 * One launcher menu row. The shape deliberately carries everything the
 * profile-row markup needs (name over a mono sub-line, host chip, ⌘N) so
 * that profile rows drop into the same slots when profiles land — the menu
 * does not get rebuilt for them.
 */
export interface LauncherRow {
  readonly hostId: string;
  readonly hostLabel: string;
  readonly name: string;
  /** Mono sub-line. The shortened host id — real data, never a fabricated command. */
  readonly sub: string;
  readonly isDefault: boolean;
  /** `⌘1`…`⌘9` by menu position; null past the ninth — there is no ⌘10. */
  readonly chord: string | null;
}

/**
 * Hosts → launcher rows. The current host comes first tagged `default` —
 * it is the row `⏎` runs — and the rest keep their given order, so the menu
 * is stable across opens. An unknown or null default tags nothing; `⏎` then
 * falls to the first row, which the menu resolves, not this builder.
 */
export function launcherRows(
  hosts: readonly HostChoice[],
  defaultHostId: string | null,
): readonly LauncherRow[] {
  const def = defaultHostId === null ? undefined : hosts.find((h) => h.id === defaultHostId);
  const ordered = def === undefined ? hosts : [def, ...hosts.filter((h) => h.id !== def.id)];
  return ordered.map((h, i) => ({
    hostId: h.id,
    hostLabel: h.label,
    name: `New session on ${h.label}`,
    sub: shortHostId(h.id),
    isDefault: def !== undefined && h.id === def.id,
    chord: i < 9 ? `⌘${i + 1}` : null,
  }));
}

/** What a document-level keydown means to the open launcher menu. */
export type LauncherKey = 'dismiss' | 'run-default' | 'focus-rows' | 'none';

/**
 * The open menu's key policy, pure so every chord the menu ADVERTISES is
 * pinned to an action: `esc` dismisses, `⏎` runs the default, `⇧⏎` moves
 * focus into the host rows — the "Run on another host…" row's own action
 * (design §1: both act; a dead chord feeds a newline to the shell behind the
 * menu instead). `⏎` yields when focus already sits inside the menu, because
 * the focused row activates itself and claiming the key would run the
 * default over the row the user chose.
 */
export function launcherKeyOf(key: string, shift: boolean, focusInMenu: boolean): LauncherKey {
  if (key === 'Escape') return 'dismiss';
  if (key !== 'Enter') return 'none';
  if (shift) return 'focus-rows';
  return focusInMenu ? 'none' : 'run-default';
}

/** A horizontal extent — a chip's or its scroll viewport's, in any shared coordinates. */
export interface Extent {
  readonly left: number;
  readonly right: number;
}

/**
 * Whether the active chip needs scrolling into view: any part of it outside
 * the viewport qualifies. Pure so the edge cases are testable; the component
 * only measures rects and calls `scrollIntoView` when this says so — calling
 * it unconditionally would fight the user's own scroll on every re-render.
 */
export function shouldScrollIntoView(chip: Extent, viewport: Extent): boolean {
  return chip.left < viewport.left || chip.right > viewport.right;
}

/**
 * The sidebar's icon-rail breakpoint. The stylesheet's
 * `@media (max-width: 900px)` rule is what actually collapses the sidebar;
 * this is the same number for logic-side callers, and the test pins the two
 * against drifting apart. `<=` because `max-width` is inclusive.
 */
export const ICON_RAIL_MAX_WIDTH = 900;

export function isIconRail(width: number): boolean {
  return width <= ICON_RAIL_MAX_WIDTH;
}

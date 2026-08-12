/**
 * The tab list and which tab is active — pure data and pure reducers, no sigx.
 * Components wrap this in a signal; keeping the transitions here means the
 * close/activate/singleton rules are testable under `node --test` with no
 * renderer in the room.
 *
 * A tab carries `panes` as an array from day one, even though it always has
 * exactly one today: the split-pane screen in the handoff makes "a tab holds
 * panes" the shape, and retrofitting an array under a scalar is the kind of
 * migration that touches every consumer at once.
 */

export interface Pane {
  readonly id: string;
  readonly hostId: string;
  readonly sessionId: string;
  readonly focused: boolean;
}

export type TabKind = 'session' | 'settings' | 'profiles';

/** Link health surfaces on the tab itself — the handoff has no status bar. */
export type LinkState = 'live' | 'stalled' | 'reconnecting';

export interface Tab {
  readonly id: string;
  readonly kind: TabKind;
  readonly title: string;
  readonly hostId: string;
  readonly cwd: string;
  readonly color: string | null;
  readonly panes: readonly Pane[];
  readonly link: LinkState;
}

export interface TabsState {
  readonly tabs: readonly Tab[];
  readonly activeId: string | null;
}

export const NO_TABS: TabsState = { tabs: [], activeId: null };

/** Append and focus — a tab you just opened is the one you meant to look at. */
export function openTab(state: TabsState, tab: Tab): TabsState {
  return { tabs: [...state.tabs, tab], activeId: tab.id };
}

/**
 * Close a tab. When the closed tab was the active one, activate the neighbour
 * at the *previous* index, clamped — closing the first tab lands on the new
 * first, and closing the last tab of all leaves nothing active. Closing an
 * inactive tab never moves focus: yanking the user's attention to wherever a
 * background tab died is the misfeature this rule exists to prevent.
 */
export function closeTab(state: TabsState, id: string): TabsState {
  const i = state.tabs.findIndex((t) => t.id === id);
  if (i < 0) return state;
  const tabs = state.tabs.filter((t) => t.id !== id);
  if (state.activeId !== id) return { tabs, activeId: state.activeId };
  if (tabs.length === 0) return { tabs, activeId: null };
  const neighbour = tabs[Math.min(Math.max(i - 1, 0), tabs.length - 1)];
  return { tabs, activeId: neighbour === undefined ? null : neighbour.id };
}

/** Activate a tab that exists; an unknown id changes nothing. */
export function activate(state: TabsState, id: string): TabsState {
  if (!state.tabs.some((t) => t.id === id)) return state;
  return { tabs: state.tabs, activeId: id };
}

/**
 * Settings and Profiles exist at most once — `⌘,` on an already-open Settings
 * tab activates it rather than opening a second. `mk` runs only when the tab
 * is genuinely new, so callers can allocate ids in it without wasting them.
 */
export function openSingleton(
  state: TabsState,
  kind: 'settings' | 'profiles',
  mk: () => Tab,
): TabsState {
  const existing = state.tabs.find((t) => t.kind === kind);
  if (existing !== undefined) return activate(state, existing.id);
  return openTab(state, mk());
}

/**
 * The session's OSC title arriving after the tab opened — a tab created from
 * a fresh session has an empty title until the shell names itself. A no-op
 * (unknown id, unchanged title) returns the same reference so no signal fires.
 */
export function setTitle(state: TabsState, id: string, title: string): TabsState {
  const tab = state.tabs.find((t) => t.id === id);
  if (tab === undefined || tab.title === title) return state;
  return {
    tabs: state.tabs.map((t) => (t.id === id ? { ...t, title } : t)),
    activeId: state.activeId,
  };
}

/**
 * Link health surfacing on the tab — the handoff has no status bar, so the
 * tab's dot is where "reconnecting" is allowed to show. Same no-op contract
 * as `setTitle`.
 */
export function setLink(state: TabsState, id: string, link: LinkState): TabsState {
  const tab = state.tabs.find((t) => t.id === id);
  if (tab === undefined || tab.link === link) return state;
  return {
    tabs: state.tabs.map((t) => (t.id === id ? { ...t, link } : t)),
    activeId: state.activeId,
  };
}

export interface HostGroup {
  readonly hostId: string;
  readonly tabs: readonly Tab[];
}

/**
 * Group tabs by host, in stable order of first appearance. This function IS
 * the "sidebar renders from the same tab list" design rule: the vertical
 * sidebar's host groups are derived from the very array the horizontal strip
 * renders — never a second list, because a hardcoded row cannot be selected
 * and a separate array cannot show a session the user just started. Callers
 * pass the session tabs; Settings and Profiles are pinned rows, not groups.
 */
export function groupByHost(tabs: readonly Tab[]): readonly HostGroup[] {
  const order: string[] = [];
  const byHost = new Map<string, Tab[]>();
  for (const tab of tabs) {
    const group = byHost.get(tab.hostId);
    if (group === undefined) {
      order.push(tab.hostId);
      byHost.set(tab.hostId, [tab]);
    } else {
      group.push(tab);
    }
  }
  return order.map((hostId) => ({ hostId, tabs: byHost.get(hostId) ?? [] }));
}

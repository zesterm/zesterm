/**
 * What the palette can offer — built purely from what the browser actually
 * holds, never from what it wishes it knew (design §6).
 *
 * The honesty rules are the file's whole shape:
 *
 * - **Blocks come from every connected daemon's answer, seeded by the
 *   attached grids** (#530). Each machine the shell holds a connection to
 *   answers `search_blocks` for the sessions it owns and the history it
 *   stores; the attached tabs' `GridView.blocks` are the rows on screen
 *   before an answer lands, and the same block seen both ways is one row —
 *   the live one, because it has a tab to run in. The "N hosts searched"
 *   count states the hosts that *answered*, never a host the directory
 *   merely lists.
 * - **Provenance says only what arrived over the wire.** An age renders only
 *   when the host stamped a timestamp; a block without one gets no fabricated
 *   "just now". `exit ?` stays `exit ?` — the same never-a-green-tick rule
 *   the blocks pane keeps.
 * - **Actions are the ones that work.** Settings/profiles tabs do not exist
 *   in the browser yet, so no row advertises them; a dead row reads as a
 *   broken feature (the launcher menu's own rule).
 */

import type { BlockPayload, BlockState } from '@zesterm/proto';
import type { SessionEntry } from '@zesterm/control';
import { builtinThemes } from '@zesterm/theme';

import type { BlockSearchView } from '../block-search.ts';
import { chipTitle, shortHostId, tabIdOf, type HostChoice } from '../chrome-model.ts';
import type { Tab } from '../state/tabs.ts';

/** Glyph tint for a block row — the rail palette, so the two can never disagree. */
export type ItemTone = 'success' | 'danger' | 'warn' | 'faint';

export type PaletteAction =
  | { readonly kind: 'layout-toggle' }
  | { readonly kind: 'keybar-toggle' }
  | { readonly kind: 'set-theme'; readonly themeId: string };

export type PaletteItem =
  | {
      readonly kind: 'block';
      /** The open tab holding the block's session, when there is one. */
      readonly tabId: string | null;
      readonly hostId: string;
      /** `null` for a block only a host's store remembers (ADR-020). */
      readonly sessionId: string | null;
      readonly blockId: number;
      /** The command — what matching runs over and what ⏎ types. */
      readonly text: string;
      readonly provenance: string;
      /** Epoch ms of the block's freshest stamp; null when the host sent none. */
      readonly recency: number | null;
      readonly tone: ItemTone;
      /** False for a command the host cut: history to read, not to re-run. */
      readonly runnable: boolean;
    }
  | {
      readonly kind: 'session';
      /** The open tab, when there is one; null for a directory-only session. */
      readonly tabId: string | null;
      readonly hostId: string;
      readonly sessionId: string;
      readonly text: string;
      readonly provenance: string;
    }
  | { readonly kind: 'host'; readonly hostId: string; readonly text: string; readonly provenance: string }
  | {
      readonly kind: 'action';
      readonly action: PaletteAction;
      readonly text: string;
      readonly provenance: string;
    };

/** One attached tab's grid, as the shell hands it over. */
export interface AttachedTabBlocks {
  readonly tabId: string;
  readonly hostId: string;
  readonly sessionId: string;
  readonly hostLabel: string;
  readonly blocks: readonly BlockPayload[];
}

/** `129s` would read as a typo; each unit hands over where the next is exact enough. */
function formatAge(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  if (s < 60) return `${s}s ago`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  return `${Math.floor(h / 24)}d ago`;
}

/** `host · age · outcome`, with the age only when the host stamped one. */
function provenanceOf(
  hostLabel: string,
  stamp: number | null,
  state: BlockState,
  nowMs: number,
): { provenance: string; tone: ItemTone } {
  const parts: string[] = [hostLabel];
  // Ages only when the host stamped one — never fabricated.
  if (stamp !== null) parts.push(formatAge(nowMs - stamp));
  const finished = state.state === 'finished';
  parts.push(finished ? `exit ${state.exit_code ?? '?'}` : 'running');
  return {
    provenance: parts.join(' · '),
    tone: !finished
      ? 'warn'
      : state.exit_code === null
        ? 'faint'
        : state.exit_code === 0
          ? 'success'
          : 'danger',
  };
}

/**
 * Command history rows: the attached grids first, then every host's answer
 * to the search, one row per `(host, session, block)` with the attached copy
 * winning — it has a tab to run in, and its state is fresher (a grid flips
 * running → finished before the next answer lands). Prompt-state and
 * command-less blocks are skipped — an empty prompt is not history and there
 * is nothing to re-run. Outcomes come from the wire verbatim: `exit ?` for a
 * null code, the blocks pane's never-a-green-tick rule. A command the host
 * cut is shown with its cut and is not runnable.
 */
export function blockItems(
  tabs: readonly AttachedTabBlocks[],
  search: BlockSearchView,
  hostLabels: Readonly<Record<string, string>>,
  nowMs: number,
): readonly PaletteItem[] {
  const label = (hostId: string): string => hostLabels[hostId] ?? shortHostId(hostId);
  const items: PaletteItem[] = [];
  const seen = new Set<string>();
  for (const tab of tabs) {
    for (const b of tab.blocks) {
      if (b.command === '' || b.state.state === 'prompt') continue;
      seen.add(`${tab.hostId}:${tab.sessionId}:${b.id}`);
      const stamp = b.ended_ms ?? b.started_ms ?? null;
      items.push({
        kind: 'block',
        tabId: tab.tabId,
        hostId: tab.hostId,
        sessionId: tab.sessionId,
        blockId: b.id,
        text: b.command,
        recency: stamp,
        runnable: true,
        ...provenanceOf(tab.hostLabel, stamp, b.state, nowMs),
      });
    }
  }
  for (const h of search.hits) {
    if (h.command === '' || h.state.state === 'prompt') continue;
    if (h.session !== null && seen.has(`${h.hostId}:${h.session}:${h.block}`)) continue;
    const stamp = h.endedMs ?? h.startedMs;
    items.push({
      kind: 'block',
      tabId: null,
      hostId: h.hostId,
      sessionId: h.session,
      blockId: h.block,
      text: h.commandTruncated ? `${h.command}…` : h.command,
      recency: stamp,
      runnable: !h.commandTruncated,
      ...provenanceOf(label(h.hostId), stamp, h.state, nowMs),
    });
  }
  return items;
}

/**
 * Open tabs first, then the directory's sessions that are not already open —
 * deduped on the full (host, session) pair, because a session reachable two
 * ways is still one terminal and two rows for it would race each other under
 * ⏎.
 */
export function sessionItems(
  tabs: readonly Tab[],
  entries: readonly SessionEntry[],
  hostLabels: Readonly<Record<string, string>>,
): readonly PaletteItem[] {
  const label = (hostId: string): string => hostLabels[hostId] ?? shortHostId(hostId);
  const items: PaletteItem[] = [];
  const openIds = new Set<string>();
  for (const t of tabs) {
    if (t.kind !== 'session') continue;
    const pane = t.panes[0];
    if (pane === undefined) continue;
    openIds.add(t.id);
    items.push({
      kind: 'session',
      tabId: t.id,
      hostId: t.hostId,
      sessionId: pane.sessionId,
      text: chipTitle(t),
      provenance: [label(t.hostId), ...(t.cwd === '' ? [] : [t.cwd]), 'open'].join(' · '),
    });
  }
  for (const e of entries) {
    if (openIds.has(tabIdOf(e.host, e.session))) continue;
    items.push({
      kind: 'session',
      tabId: null,
      hostId: e.host,
      sessionId: e.session,
      text: chipTitle(e),
      provenance: [label(e.host), ...(e.cwd === '' ? [] : [e.cwd])].join(' · '),
    });
  }
  return items;
}

/** Same copy as the launcher's rows — the palette and the `+` are two doors to one verb. */
export function hostItems(hosts: readonly HostChoice[]): readonly PaletteItem[] {
  return hosts.map((h) => ({
    kind: 'host',
    hostId: h.id,
    text: `New session on ${h.label}`,
    provenance: shortHostId(h.id),
  }));
}

/**
 * The static actions: the layout flip, and one row per built-in theme. Theme
 * switching goes through the theme store at run time; here a theme is only a
 * name and a mode.
 */
export function actionItems(): readonly PaletteItem[] {
  return [
    {
      kind: 'action',
      action: { kind: 'layout-toggle' },
      text: 'Toggle tab layout',
      provenance: '',
    },
    {
      kind: 'action',
      action: { kind: 'keybar-toggle' },
      text: 'Toggle key bar',
      provenance: 'esc · tab · ctrl · arrows',
    },
    ...builtinThemes.map(
      (t): PaletteItem => ({
        kind: 'action',
        action: { kind: 'set-theme', themeId: t.id },
        text: `Theme: ${t.name}`,
        provenance: t.mode,
      }),
    ),
  ];
}

/** What ⏎ means, resolved purely so every row kind's verb is testable. */
export type RunTarget =
  | { readonly kind: 'nothing' }
  | { readonly kind: 'run-block'; readonly tabId: string; readonly command: string }
  | { readonly kind: 'activate-tab'; readonly tabId: string }
  | { readonly kind: 'open-session'; readonly hostId: string; readonly sessionId: string }
  | { readonly kind: 'create-session'; readonly hostId: string }
  | { readonly kind: 'layout-toggle' }
  | { readonly kind: 'keybar-toggle' }
  | { readonly kind: 'set-theme'; readonly themeId: string };

/**
 * ⏎ on a block re-runs only in the ACTIVE tab — the terminal's own prompt
 * gate (`runCommand`, the ⌘⇧R rule) then decides whether typing is safe.
 * For a block on a background tab the target is activation alone, nothing
 * destructive: the user has not seen that tab's prompt state, and typing
 * into a shell they are not looking at is how a command lands in a running
 * program's stdin. A block with no tab here — another machine's, or one only
 * a store remembers — is the footer's literal `⏎ run here`: typed into the
 * active tab, through the same gate. A command the host cut runs nowhere.
 */
export function runTargetOf(item: PaletteItem, activeTabId: string | null): RunTarget {
  switch (item.kind) {
    case 'block':
      if (!item.runnable) return { kind: 'nothing' };
      if (item.tabId !== null) {
        return item.tabId === activeTabId
          ? { kind: 'run-block', tabId: item.tabId, command: item.text }
          : { kind: 'activate-tab', tabId: item.tabId };
      }
      return activeTabId === null
        ? { kind: 'nothing' }
        : { kind: 'run-block', tabId: activeTabId, command: item.text };
    case 'session':
      return item.tabId !== null
        ? { kind: 'activate-tab', tabId: item.tabId }
        : { kind: 'open-session', hostId: item.hostId, sessionId: item.sessionId };
    case 'host':
      return { kind: 'create-session', hostId: item.hostId };
    case 'action':
      return item.action.kind === 'layout-toggle'
        ? { kind: 'layout-toggle' }
        : item.action.kind === 'keybar-toggle'
          ? { kind: 'keybar-toggle' }
          : { kind: 'set-theme', themeId: item.action.themeId };
  }
}

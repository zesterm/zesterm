/**
 * Ranking the palette's results (design §6) — pure, so the order the modal
 * shows is provable under `node --test`.
 *
 * The group order is PINNED: Blocks → Sessions → Hosts → Actions, whatever
 * the match quality says. Blocks first is the point of the palette — it is
 * primarily a history of what ran anywhere in the fleet — so a session that
 * matches the query better than any block still lists after every block that
 * matches at all. Ranking happens *within* a group only.
 */

import type { PaletteItem } from './sources.ts';

export interface PaletteSources {
  readonly blocks: readonly PaletteItem[];
  readonly sessions: readonly PaletteItem[];
  readonly hosts: readonly PaletteItem[];
  readonly actions: readonly PaletteItem[];
}

export type GroupLabel = 'Blocks' | 'Sessions' | 'Hosts' | 'Actions';

export interface PaletteGroup {
  readonly label: GroupLabel;
  readonly items: readonly PaletteItem[];
}

/**
 * Subsequence match, scored — `null` when the query is not a subsequence of
 * the text. Lower scores rank first: the span the greedy leftmost match
 * covers dominates (an exact substring's span is the query's own length, the
 * best possible), the start position breaks ties so a match at the head of
 * the command beats the same match buried in its arguments.
 */
export function matchScore(query: string, text: string): number | null {
  if (query === '') return 0;
  const q = query.toLowerCase();
  const t = text.toLowerCase();
  let first = -1;
  let at = 0;
  for (const ch of q) {
    const i = t.indexOf(ch, at);
    if (i < 0) return null;
    if (first < 0) first = i;
    at = i + 1;
  }
  const span = at - first;
  return span * 1000 + first;
}

function recencyOf(item: PaletteItem): number | null {
  return item.kind === 'block' ? item.recency : null;
}

/**
 * Order one group. With a query: score ascending, recency (newest first)
 * breaking ties, source order after that (sort is stable). With an empty
 * query the group shows recents: items with a timestamp newest-first, the
 * stampless ones after them in source order — for blocks that is exactly
 * "what ran last, anywhere the browser can see".
 */
function rankGroup(query: string, items: readonly PaletteItem[]): readonly PaletteItem[] {
  const scored: { item: PaletteItem; score: number }[] = [];
  for (const item of items) {
    const score = matchScore(query, item.text);
    if (score !== null) scored.push({ item, score });
  }
  scored.sort((a, b) => {
    if (a.score !== b.score) return a.score - b.score;
    const ra = recencyOf(a.item);
    const rb = recencyOf(b.item);
    if (ra === rb) return 0;
    if (ra === null) return 1;
    if (rb === null) return -1;
    return rb - ra;
  });
  return scored.map((s) => s.item);
}

/** Ordered groups, pinned Blocks → Sessions → Hosts → Actions; empty groups dropped. */
export function rankResults(query: string, sources: PaletteSources): readonly PaletteGroup[] {
  const groups: { label: GroupLabel; items: readonly PaletteItem[] }[] = [
    { label: 'Blocks', items: rankGroup(query, sources.blocks) },
    { label: 'Sessions', items: rankGroup(query, sources.sessions) },
    { label: 'Hosts', items: rankGroup(query, sources.hosts) },
    { label: 'Actions', items: rankGroup(query, sources.actions) },
  ];
  return groups.filter((g) => g.items.length > 0);
}

/**
 * The selection is one flat index over every visible row (the palette store's
 * `moveSelection` wraps over this length), so the flattening must be the one
 * the modal renders — this function is that single definition.
 */
export function flattenResults(groups: readonly PaletteGroup[]): readonly PaletteItem[] {
  return groups.flatMap((g) => g.items);
}

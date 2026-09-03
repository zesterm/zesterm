import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  flattenResults,
  matchScore,
  rankResults,
  type PaletteSources,
} from '../src/palette/rank.ts';
import type { PaletteItem } from '../src/palette/sources.ts';
import { moveSelection, openPalette } from '../src/state/palette.ts';

function blockItem(text: string, recency: number | null = null, tabId: string | null = 'h1:1'): PaletteItem {
  return {
    kind: 'block',
    tabId,
    hostId: 'h1',
    sessionId: '1',
    blockId: 0,
    text,
    provenance: '',
    recency,
    tone: 'success',
    runnable: true,
  };
}

function sessionItem(text: string): PaletteItem {
  return { kind: 'session', tabId: 'h1:1', hostId: 'h1', sessionId: '1', text, provenance: '' };
}

function hostItem(text: string): PaletteItem {
  return { kind: 'host', hostId: 'h2', text, provenance: '' };
}

function actionItem(text: string): PaletteItem {
  return { kind: 'action', action: { kind: 'layout-toggle' }, text, provenance: '' };
}

const EMPTY: PaletteSources = { blocks: [], sessions: [], hosts: [], actions: [] };

test('groups are pinned Blocks → Sessions → Hosts → Actions', () => {
  const groups = rankResults('', {
    blocks: [blockItem('cargo build')],
    sessions: [sessionItem('zsh')],
    hosts: [hostItem('New session on studio')],
    actions: [actionItem('Toggle tab layout')],
  });
  assert.deepEqual(
    groups.map((g) => g.label),
    ['Blocks', 'Sessions', 'Hosts', 'Actions'],
    'blocks first IS the point — the palette is primarily a history of what ran',
  );
});

test('blocks list first even when a session matches the query better', () => {
  const groups = rankResults('build', {
    ...EMPTY,
    blocks: [blockItem('cargo xtask check-deps && build')],
    sessions: [sessionItem('build')],
  });
  assert.deepEqual(
    groups.map((g) => g.label),
    ['Blocks', 'Sessions'],
    'ranking happens WITHIN a group; an exact session match never lifts Sessions above Blocks',
  );
});

test('subsequence matching filters, and tighter matches rank first', () => {
  const groups = rankResults('test', {
    ...EMPTY,
    blocks: [blockItem('git status'), blockItem('the last step'), blockItem('cargo test')],
  });
  const texts = flattenResults(groups).map((i) => i.text);
  assert.ok(!texts.includes('git status'), "'test' is not a subsequence of 'git status' — no e");
  assert.deepEqual(
    texts,
    ['cargo test', 'the last step'],
    "the contiguous 'test' spans 4 characters; the scattered t…e…s…t spans 8 — tighter wins",
  );
});

test('equal match quality falls back to recency, newest first', () => {
  const groups = rankResults('cargo', {
    ...EMPTY,
    blocks: [blockItem('cargo test', 1000), blockItem('cargo test', 2000)],
  });
  assert.deepEqual(
    flattenResults(groups).map((i) => (i.kind === 'block' ? i.recency : null)),
    [2000, 1000],
    'between two identical commands the one that ran last is the one to re-run',
  );
});

test('matchScore is null for a non-match and 0 for the empty query', () => {
  assert.equal(matchScore('xyz', 'cargo build'), null);
  assert.equal(matchScore('', 'anything'), 0, 'an empty query keeps every item — recents');
});

test('the empty query shows recents: newest stamped blocks first, stampless last', () => {
  const groups = rankResults('', {
    ...EMPTY,
    blocks: [blockItem('old', 1000), blockItem('stampless', null), blockItem('new', 2000)],
  });
  assert.deepEqual(
    flattenResults(groups).map((i) => i.text),
    ['new', 'old', 'stampless'],
    'recency orders the blank-query list; a block with no timestamp cannot claim to be recent',
  );
});

test('groups with no matching items are dropped, label and all', () => {
  const groups = rankResults('cargo', {
    ...EMPTY,
    blocks: [blockItem('cargo test')],
    sessions: [sessionItem('zsh')],
    actions: [actionItem('Toggle tab layout')],
  });
  assert.deepEqual(
    groups.map((g) => g.label),
    ['Blocks'],
    'a group label over nothing advertises results that do not exist',
  );
});

test('selection wraps against the real flattened result count', () => {
  const groups = rankResults('', {
    ...EMPTY,
    blocks: [blockItem('a'), blockItem('b')],
    sessions: [sessionItem('c')],
  });
  const count = flattenResults(groups).length;
  assert.equal(count, 3, 'the flat list crosses group boundaries — 2 blocks + 1 session');

  let s = openPalette();
  s = moveSelection(s, -1, count);
  assert.equal(s.selection, 2, '↑ from the first row lands on the LAST row of the last group');
  s = moveSelection(s, 1, count);
  assert.equal(s.selection, 0, '↓ past the last row wraps to the first block');
});

test('fleet rows interleave with live rows by recency, whichever machine ran them', () => {
  // A fresher block stored on another machine ranks above an older one in
  // an attached tab: the list is one history, not this tab's then theirs.
  const groups = rankResults('', {
    ...EMPTY,
    blocks: [blockItem('old here', 100), blockItem('newer there', 200, null), blockItem('newest here', 300)],
  });
  assert.deepEqual(
    groups[0]?.items.map((i) => i.text),
    ['newest here', 'newer there', 'old here'],
  );
});

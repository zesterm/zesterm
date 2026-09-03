import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  NO_TABS,
  openTab,
  closeTab,
  activate,
  openSingleton,
  groupByHost,
  setCommand,
  setLink,
  setTitle,
  type Tab,
  type TabsState,
} from '../src/state/tabs.ts';

function tab(id: string, over: Partial<Tab> = {}): Tab {
  return {
    id,
    kind: 'session',
    title: id,
    command: '',
    hostId: 'studio',
    cwd: '~',
    color: null,
    panes: [{ id: `${id}-p0`, hostId: 'studio', sessionId: '1', focused: true }],
    link: 'live',
    ...over,
  };
}

function withTabs(...tabs: Tab[]): TabsState {
  return tabs.reduce(openTab, NO_TABS);
}

test('openTab appends and activates', () => {
  const s = withTabs(tab('a'), tab('b'));
  assert.deepEqual(
    s.tabs.map((t) => t.id),
    ['a', 'b'],
    'tabs keep opening order — the strip renders this array as-is',
  );
  assert.equal(s.activeId, 'b', 'a tab you just opened is the one you meant to look at');
});

test('closing the active tab activates the previous-index neighbour', () => {
  const s = closeTab(withTabs(tab('a'), tab('b'), tab('c')), 'c');
  assert.equal(s.activeId, 'b', 'focus falls back to the tab you were on before this one');
  assert.deepEqual(s.tabs.map((t) => t.id), ['a', 'b']);
});

test('closing the active middle tab lands on its left neighbour', () => {
  const s = activate(withTabs(tab('a'), tab('b'), tab('c')), 'b');
  assert.equal(closeTab(s, 'b').activeId, 'a', 'previous index, not next');
});

test('closing the active first tab clamps to the new first', () => {
  const s = activate(withTabs(tab('a'), tab('b')), 'a');
  assert.equal(closeTab(s, 'a').activeId, 'b', 'there is no index -1 to fall back to');
});

test('closing the last remaining tab leaves nothing active', () => {
  const s = closeTab(withTabs(tab('a')), 'a');
  assert.equal(s.tabs.length, 0);
  assert.equal(s.activeId, null, 'an activeId pointing at a closed tab would render a ghost pane');
});

test('closing an inactive tab never moves focus', () => {
  const s = closeTab(withTabs(tab('a'), tab('b'), tab('c')), 'a');
  assert.equal(
    s.activeId,
    'c',
    'a background tab dying must not yank the user off what they are reading',
  );
});

test('closing an unknown id changes nothing', () => {
  const s = withTabs(tab('a'));
  assert.equal(closeTab(s, 'nope'), s, 'same reference, so no signal fires for a no-op');
});

test('activate ignores an id that is not in the list', () => {
  const s = withTabs(tab('a'));
  assert.equal(activate(s, 'ghost').activeId, 'a', 'activating a ghost would blank the pane');
});

test('openSingleton never duplicates settings, and activates the existing tab', () => {
  let minted = 0;
  const mk = (): Tab => {
    minted += 1;
    return tab('settings-tab', { kind: 'settings', hostId: '' });
  };
  const once = openSingleton(withTabs(tab('a')), 'settings', mk);
  const again = openSingleton(activate(once, 'a'), 'settings', mk);
  assert.equal(
    again.tabs.filter((t) => t.kind === 'settings').length,
    1,
    '⌘, on an open Settings tab must activate it, not open a second',
  );
  assert.equal(again.activeId, 'settings-tab');
  assert.equal(minted, 1, 'mk runs only when the tab is genuinely new');
});

test('settings and profiles are independent singletons', () => {
  let s = openSingleton(NO_TABS, 'settings', () => tab('st', { kind: 'settings', hostId: '' }));
  s = openSingleton(s, 'profiles', () => tab('pr', { kind: 'profiles', hostId: '' }));
  assert.equal(s.tabs.length, 2, 'one of each may exist; they are different tabs');
  assert.equal(s.activeId, 'pr');
});

test('groupByHost keeps order of first appearance and loses no tab', () => {
  // Interleaved on purpose: the sidebar's host order is "who appeared first
  // in the strip", not alphabetical, so the groups follow the user's history.
  const tabs = [
    tab('a', { hostId: 'studio' }),
    tab('b', { hostId: 'forge' }),
    tab('c', { hostId: 'studio' }),
    tab('d', { hostId: 'mac' }),
    tab('e', { hostId: 'forge' }),
  ];
  const groups = groupByHost(tabs);
  assert.deepEqual(
    groups.map((g) => g.hostId),
    ['studio', 'forge', 'mac'],
    'stable first-appearance order — a re-sort would shuffle the sidebar on every open',
  );
  const flattened = groups.flatMap((g) => g.tabs.map((t) => t.id));
  assert.deepEqual(
    flattened.sort(),
    ['a', 'b', 'c', 'd', 'e'],
    'complete: a session the user just started must appear in the sidebar',
  );
  assert.deepEqual(
    groups[0]?.tabs.map((t) => t.id),
    ['a', 'c'],
    'tabs stay in strip order within their group',
  );
});

test('groupByHost of nothing is nothing', () => {
  assert.deepEqual(groupByHost([]), [], 'an empty window renders no host groups');
});

test('setTitle renames only the named tab and keeps focus where it was', () => {
  const s = activate(withTabs(tab('a'), tab('b')), 'a');
  const renamed = setTitle(s, 'b', 'vim');
  assert.equal(
    renamed.tabs.find((t) => t.id === 'b')?.title,
    'vim',
    'the OSC title arriving after open must reach the chip',
  );
  assert.equal(
    renamed.tabs.find((t) => t.id === 'a')?.title,
    'a',
    'a background session naming itself must not touch other tabs',
  );
  assert.equal(renamed.activeId, 'a', 'a title change never moves focus');
});

test('setTitle is a no-op by reference for unknown ids and unchanged titles', () => {
  const s = withTabs(tab('a'));
  assert.equal(setTitle(s, 'ghost', 'x'), s, 'same reference, so no signal fires for a no-op');
  assert.equal(setTitle(s, 'a', 'a'), s, 'an unchanged title must not cause a re-render');
});

test('setLink flips only the named tab and no-ops by reference otherwise', () => {
  const s = withTabs(tab('a'), tab('b'));
  const cut = setLink(s, 'a', 'reconnecting');
  assert.equal(
    cut.tabs.find((t) => t.id === 'a')?.link,
    'reconnecting',
    'link health surfaces on the affected tab — the design has no status bar',
  );
  assert.equal(cut.tabs.find((t) => t.id === 'b')?.link, 'live', 'only the cut tab degrades');
  assert.equal(setLink(s, 'a', 'live'), s, 'an unchanged link must not cause a re-render');
});

test('a command arriving off the blocks renames its tab', () => {
  const s = withTabs(tab('a'), tab('b'));
  const named = setCommand(s, 'a', 'cargo build');
  assert.equal(named.tabs[0]?.command, 'cargo build');
  assert.equal(named.tabs[1]?.command, '', 'only the tab named is touched');
});

test('setCommand is a no-op by reference, so no signal fires', () => {
  // Same contract as setTitle: a keyframe restates the blocks on every
  // reconnect, and a new object each time would repaint the whole strip.
  const s = setCommand(withTabs(tab('a')), 'a', 'cargo build');
  assert.equal(setCommand(s, 'a', 'cargo build'), s, 'an unchanged command');
  assert.equal(setCommand(s, 'nope', 'x'), s, 'an unknown id');
});

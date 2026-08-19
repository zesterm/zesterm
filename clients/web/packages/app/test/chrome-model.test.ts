import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  MONO_FAMILY,
  ICON_RAIL_MAX_WIDTH,
  LAUNCHER_WIDTH,
  chipTitle,
  chipTooltip,
  isIconRail,
  launcherAlign,
  launcherKeyOf,
  launcherRows,
  paneFor,
  shouldScrollIntoView,
  shortHostId,
  tabIdOf,
  type HostChoice,
} from '../src/chrome-model.ts';

test('the mono stack ends in a generic family', () => {
  assert.ok(
    MONO_FAMILY.trim().endsWith('monospace'),
    'a box with none of the named faces must still render mono, not the default serif',
  );
});

test('tabIdOf keeps hosts apart', () => {
  assert.notEqual(
    tabIdOf('aa', '3'),
    tabIdOf('bb', '3'),
    "studio's session 3 and forge's session 3 are different terminals",
  );
  assert.notEqual(
    tabIdOf('aa1', '23'),
    tabIdOf('aa12', '3'),
    "':' cannot appear in a hex host id or decimal session id, so no shuffle collides",
  );
});

test('an untitled session reads as shell, never a blank chip', () => {
  assert.equal(chipTitle({ title: '' }), 'shell');
  assert.equal(chipTitle({ title: 'vim — main.rs' }), 'vim — main.rs');
});

test('host and cwd live in the tooltip — the chip shows title only', () => {
  const tip = chipTooltip({ title: 'zsh', hostId: 'a'.repeat(64), cwd: '~/dev' }, 'studio');
  assert.ok(tip.includes('studio'), 'the host is findable from the chip via its tooltip');
  assert.ok(tip.includes('~/dev'), 'so is the cwd — the 34px chip has no room for either');
});

test('an unnamed host falls back to its shortened id in the tooltip', () => {
  const tip = chipTooltip({ title: 'zsh', hostId: 'abcdef0123456789', cwd: '' });
  assert.ok(
    tip.includes(shortHostId('abcdef0123456789')),
    'a host the directory has not labelled yet must still be identifiable',
  );
});

const HOSTS: readonly HostChoice[] = [
  { id: 'h-forge', label: 'forge' },
  { id: 'h-studio', label: 'studio' },
  { id: 'h-mac', label: 'mac' },
];

test('the current host comes first, tagged default', () => {
  const rows = launcherRows(HOSTS, 'h-studio');
  assert.equal(rows[0]?.hostId, 'h-studio', '⏎ runs the first row, which must be the default');
  assert.equal(rows[0]?.isDefault, true);
  assert.deepEqual(
    rows.slice(1).map((r) => r.hostId),
    ['h-forge', 'h-mac'],
    'the rest keep their given order so the menu is stable across opens',
  );
  assert.equal(
    rows.filter((r) => r.isDefault).length,
    1,
    'exactly one row is the default — two ⏎ targets would be ambiguous',
  );
});

test('chords are assigned by menu position, ⌘1 through ⌘9 only', () => {
  const many = Array.from({ length: 11 }, (_, i) => ({ id: `h${i}`, label: `host${i}` }));
  const rows = launcherRows(many, null);
  assert.equal(rows[0]?.chord, '⌘1');
  assert.equal(rows[8]?.chord, '⌘9');
  assert.equal(rows[9]?.chord, null, 'there is no ⌘10 — a chord that cannot be typed is a lie');
});

test('an unknown or absent default tags nothing and preserves order', () => {
  for (const def of [null, 'h-ghost']) {
    const rows = launcherRows(HOSTS, def);
    assert.deepEqual(
      rows.map((r) => r.hostId),
      ['h-forge', 'h-studio', 'h-mac'],
      'no reorder without a real default — a ghost id must not shuffle the menu',
    );
    assert.ok(
      rows.every((r) => !r.isDefault),
      'tagging a row default that ⏎ does not specially run would mislead',
    );
  }
});

test('row copy is the designed sentence over honest data', () => {
  const rows = launcherRows([{ id: 'a'.repeat(64), label: 'studio' }], null);
  assert.equal(rows[0]?.name, 'New session on studio');
  assert.equal(
    rows[0]?.sub,
    'a'.repeat(12),
    'the sub-line is the shortened host id — never a fabricated command or latency',
  );
});

test('scroll-into-view fires only when the chip leaves the viewport', () => {
  const vp = { left: 100, right: 400 };
  assert.equal(
    shouldScrollIntoView({ left: 150, right: 300 }, vp),
    false,
    'a visible chip must not be scrolled — that would fight the user’s own scroll',
  );
  assert.equal(
    shouldScrollIntoView({ left: 100, right: 400 }, vp),
    false,
    'exactly flush edges are visible, not overflowing',
  );
  assert.equal(shouldScrollIntoView({ left: 350, right: 480 }, vp), true, 'overflowing right');
  assert.equal(shouldScrollIntoView({ left: 40, right: 160 }, vp), true, 'overflowing left');
});

test('the launcher menu acts on every chord it advertises', () => {
  assert.equal(launcherKeyOf('Escape', false, false), 'dismiss');
  assert.equal(launcherKeyOf('Escape', true, true), 'dismiss', 'esc dismisses from anywhere');
  assert.equal(
    launcherKeyOf('Enter', true, false),
    'focus-rows',
    "the 'Run on another host…' row advertises ⇧⏎ — a chord that instead feeds a newline to the shell behind the menu reads as a broken feature",
  );
  assert.equal(
    launcherKeyOf('Enter', false, false),
    'run-default',
    '⏎ runs the default while focus still sits in the terminal textarea',
  );
});

test('⏎ yields to a row that already holds focus', () => {
  assert.equal(
    launcherKeyOf('Enter', false, true),
    'none',
    'a focused row activates itself — claiming ⏎ would run the default over the row the user chose',
  );
  assert.equal(
    launcherKeyOf('a', false, false),
    'none',
    'ordinary typing is not the menu’s to claim',
  );
});

test('the strip launcher opens rightwards when the + sits near the left edge', () => {
  assert.equal(
    launcherAlign(42),
    'left',
    'zero tabs put the + ~42px from the left edge — right-anchored, 276px of the 318px menu would be off-viewport and clipped',
  );
  assert.equal(
    launcherAlign(213),
    'left',
    'one tab still leaves a right-anchored menu 105px past the left viewport edge',
  );
});

test('the strip launcher keeps the designed right anchor once it fits', () => {
  assert.equal(
    launcherAlign(LAUNCHER_WIDTH),
    'right',
    'a flush fit right-anchors — the menu left edge lands exactly on the viewport edge',
  );
  assert.equal(
    launcherAlign(900),
    'right',
    'a full strip hangs the menu right-under the + as the design draws it',
  );
});

test('the icon-rail predicate agrees with the @media (max-width: 900px) rule', () => {
  assert.equal(ICON_RAIL_MAX_WIDTH, 900, 'style.css hardcodes 900px; this constant mirrors it');
  assert.equal(isIconRail(900), true, 'max-width is inclusive, so 900 collapses');
  assert.equal(isIconRail(901), false, 'one pixel wider keeps the full sidebar');
});

const TAB_HOST = 'ab'.repeat(32);
const OTHER_HOST = 'cd'.repeat(32);

/** Everything false-ish, so each test names only what it is about. */
const pane = (over: Partial<Parameters<typeof paneFor>[0]> = {}) =>
  paneFor({
    activeTabId: null,
    activeHasTarget: false,
    routeHost: undefined,
    routeSession: undefined,
    hasLanding: false,
    defaultHostId: null,
    ...over,
  });

test('the terminal shows only when the URL names the ACTIVE tab’s session', () => {
  // The URL and the active tab move in separate updates, so a render can land
  // between them. Matching on the params alone would show whichever tab
  // happened to be active while the URL described another — the wrong
  // machine's shell, wearing the right URL.
  const id = tabIdOf(TAB_HOST, '7');
  assert.deepEqual(
    pane({ activeTabId: id, activeHasTarget: true, routeHost: TAB_HOST, routeSession: '7' }),
    { kind: 'terminal', tabId: id },
  );
  assert.deepEqual(
    pane({ activeTabId: id, activeHasTarget: true, routeHost: OTHER_HOST, routeSession: '7' }),
    { kind: 'list', hostId: OTHER_HOST },
    'the URL names another machine: show that machine, not this terminal',
  );
});

test('a tab whose dial target has gone is not a terminal', () => {
  // `targets` is dropped when a tab closes; rendering a TerminalView without
  // one would be a pane with nothing to dial.
  const id = tabIdOf(TAB_HOST, '7');
  assert.deepEqual(
    pane({ activeTabId: id, activeHasTarget: false, routeHost: TAB_HOST, routeSession: '7' }),
    { kind: 'list', hostId: TAB_HOST },
  );
});

test('a machine named in the URL beats the landing', () => {
  // `/h/:hostId` is how the fleet grid's own "open" button gets you to a
  // machine. Answering it with the grid would make that button a no-op — you
  // would press open and stay exactly where you were.
  assert.deepEqual(pane({ routeHost: OTHER_HOST, hasLanding: true }), {
    kind: 'list',
    hostId: OTHER_HOST,
  });
});

test('no machine named, and a landing to show: the landing', () => {
  // The hosted path at bare `/hosts` — the fleet grid.
  assert.deepEqual(pane({ hasLanding: true, defaultHostId: TAB_HOST }), { kind: 'landing' });
});

test('no machine named and no landing lists the default one', () => {
  // Loopback, which has exactly one machine and has always shown its list at
  // `/hosts`.
  assert.deepEqual(pane({ defaultHostId: TAB_HOST }), { kind: 'list', hostId: TAB_HOST });
});

test('loopback before the directory says who it is still lists', () => {
  // The list is the thing that shows "reaching its sidecar…". A blank pane in
  // its place would make a slow start look like a broken one.
  assert.deepEqual(pane({ defaultHostId: null }), { kind: 'list', hostId: '' });
});

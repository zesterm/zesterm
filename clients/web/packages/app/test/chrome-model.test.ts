import { test } from 'node:test';
import assert from 'node:assert/strict';

import type { Dial } from '@zesterm/client';
import type { HostFacts, SessionEntry } from '@zesterm/control';

import {
  MONO_FAMILY,
  ICON_RAIL_MAX_WIDTH,
  LAUNCHER_WIDTH,
  chipTitle,
  chipTooltip,
  factsLine,
  isIconRail,
  launcherAlign,
  launchableRows,
  launcherKeyOf,
  launcherRows,
  paneFor,
  sessionRows,
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

/** What a machine published, as `launcherRows` reads it. */
const published = (
  targets: readonly { name: string; command?: string; cwd?: string }[],
  extra: Partial<HostFacts> = {},
): HostFacts => ({
  os: '',
  arch: '',
  osVersion: '',
  defaultShell: '',
  launchTargets: targets.map((t) => ({
    name: t.name,
    command: t.command ?? '',
    startingDirectory: t.cwd ?? '',
    icon: '',
    colorScheme: '',
    tabColor: null,
  })),
  ...extra,
});

/** `factsOf` over a plain table; anything absent has said nothing. */
const saying = (table: Readonly<Record<string, HostFacts>>) => (id: string) => table[id] ?? null;

test('the current host comes first, tagged default', () => {
  const rows = launchableRows(launcherRows(HOSTS, 'h-studio'));
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
  const rows = launchableRows(launcherRows(many, null));
  assert.equal(rows[0]?.chord, '⌘1');
  assert.equal(rows[8]?.chord, '⌘9');
  assert.equal(rows[9]?.chord, null, 'there is no ⌘10 — a chord that cannot be typed is a lie');
});

test('an unknown or absent default tags nothing and preserves order', () => {
  for (const def of [null, 'h-ghost']) {
    const rows = launchableRows(launcherRows(HOSTS, def));
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
  const rows = launchableRows(launcherRows([{ id: 'a'.repeat(64), label: 'studio' }], null));
  assert.equal(rows[0]?.name, 'New session on studio');
  assert.equal(
    rows[0]?.sub,
    'a'.repeat(12),
    'the sub-line is the shortened host id — never a fabricated command or latency',
  );
});

// --- what each machine publishes (#352) -------------------------------------

test('one machine grows no headers, however much it publishes', () => {
  // Every loopback shell and most accounts are one machine, and a header over
  // a single group is chrome for a fleet that does not exist. The native
  // launcher draws none there for the same reason.
  const rows = launcherRows(
    [{ id: 'h-mac', label: 'mac' }],
    'h-mac',
    saying({ 'h-mac': published([{ name: 'zsh' }, { name: 'nu' }]) }),
  );
  assert.equal(rows.filter((r) => r.kind === 'group').length, 0);
  assert.deepEqual(
    rows.map((r) => (r.kind === 'target' ? r.profile : 'GROUP')),
    [null, 'zsh', 'nu'],
    'the shell row first, then what the machine published, in its order',
  );
});

test('two machines each get a header, and their targets stay under it', () => {
  const rows = launcherRows(
    HOSTS.slice(0, 2),
    'h-forge',
    saying({
      'h-forge': published([{ name: 'Ubuntu', command: 'wsl.exe -d Ubuntu' }], {
        os: 'windows',
        arch: 'x86_64',
      }),
      'h-studio': published([{ name: 'zsh' }]),
    }),
  );
  assert.deepEqual(
    rows.map((r) => (r.kind === 'group' ? `# ${r.label}` : (r.profile ?? 'shell'))),
    ['# forge', 'shell', 'Ubuntu', '# studio', 'shell', 'zsh'],
  );
  const header = rows[0];
  assert.equal(header?.kind === 'group' ? header.sub : null, 'windows · x86_64');
  const bare = rows[3];
  assert.equal(
    bare?.kind === 'group' ? bare.sub : null,
    '',
    'a machine that said no os gets no os — a dash pretending to be a fact is worse than a blank',
  );
});

test('digits number the whole menu, not each machine', () => {
  // ⌘2 must be the second row you can SEE. Counting per group, or counting
  // headers, shifts every digit past the first machine — and the row that
  // then runs is somebody else's.
  const rows = launcherRows(
    HOSTS.slice(0, 2),
    'h-forge',
    saying({ 'h-forge': published([{ name: 'Ubuntu' }]) }),
  );
  assert.deepEqual(
    rows.map((r) => (r.kind === 'group' ? 'HEADER' : r.chord)),
    ['HEADER', '⌘1', '⌘2', 'HEADER', '⌘3'],
  );
  assert.deepEqual(
    launchableRows(rows).map((r) => r.chord),
    ['⌘1', '⌘2', '⌘3'],
    'and `launchableRows` is the same walk, so ⌘N indexes what it draws',
  );
});

test('a machine that has said nothing still offers a shell', () => {
  // Null is not an empty list, but both leave the same rows: an older daemon,
  // a machine nothing can reach, and one with no profiles all get their "New
  // session on…" row, because that row does not depend on the offer and it is
  // the only way to get a shell there at all.
  for (const facts of [null, published([])]) {
    const rows = launchableRows(
      launcherRows([{ id: 'h-mac', label: 'mac' }], null, () => facts),
    );
    assert.equal(rows.length, 1);
    assert.equal(rows[0]?.profile, null);
    assert.equal(rows[0]?.command, '', 'empty is "that machine\'s default shell"');
  }
});

test('a row carries what to run, because the wire has no profile field', () => {
  // `create_session` takes a command and a cwd. The machine resolved the
  // profile through its own defaults before publishing it (ADR-014), so the
  // row sends what it was told rather than re-resolving against a config this
  // client does not have.
  const rows = launchableRows(
    launcherRows(
      [{ id: 'h-forge', label: 'forge' }],
      null,
      saying({
        'h-forge': published([{ name: 'Ubuntu', command: 'wsl.exe -d Ubuntu', cwd: '/home/andy' }]),
      }),
    ),
  );
  assert.equal(rows[1]?.command, 'wsl.exe -d Ubuntu');
  assert.equal(rows[1]?.cwd, '/home/andy');
  assert.equal(rows[1]?.sub, 'wsl.exe -d Ubuntu', 'the sub-line shows what will run');
});

test('the shell row names the machine\'s own shell once it has said', () => {
  const withShell = launchableRows(
    launcherRows(
      [{ id: 'a'.repeat(64), label: 'forge' }],
      null,
      saying({ ['a'.repeat(64)]: published([], { defaultShell: 'pwsh.exe' }) }),
    ),
  );
  assert.equal(withShell[0]?.sub, 'pwsh.exe');
  // And falls back to the shortened id rather than guessing one — the rule
  // the sub-line has always followed.
  const silent = launchableRows(launcherRows([{ id: 'a'.repeat(64), label: 'forge' }], null));
  assert.equal(silent[0]?.sub, 'a'.repeat(12));
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

test('an empty route param is not a machine', () => {
  // The router can yield `''` for an unmatched or partial match (`/h//s/7`),
  // and `Shell.syncRoute` already treats that as "no machine named". If this
  // did not, the two would disagree: the route watcher opens nothing while the
  // pane renders a session list for a host id that is the empty string.
  assert.deepEqual(pane({ routeHost: '', hasLanding: true }), { kind: 'landing' });
  assert.deepEqual(pane({ routeHost: '', defaultHostId: TAB_HOST }), {
    kind: 'list',
    hostId: TAB_HOST,
  });
  // And an empty *session* leaves it a machine, not a terminal.
  const id = tabIdOf(TAB_HOST, '7');
  assert.deepEqual(
    pane({ activeTabId: id, activeHasTarget: true, routeHost: TAB_HOST, routeSession: '' }),
    { kind: 'list', hostId: TAB_HOST },
  );
});

test('the profiles screen beats every other arm, because it names no machine', () => {
  // `/profiles` carries no hostId and no sessionId, so each arm below it would
  // fall through — to the landing on the hosted path, to the default list on
  // loopback. ⌘⇧, would then change the URL and leave the screen exactly as it
  // was, which is the state that chord was already in (claimed, then
  // discarded) with a misleading address bar added.
  assert.deepEqual(pane({ atProfiles: true, hasLanding: true }), { kind: 'profiles' });
  assert.deepEqual(pane({ atProfiles: true, defaultHostId: TAB_HOST }), { kind: 'profiles' });
  const id = tabIdOf(TAB_HOST, '7');
  assert.deepEqual(
    pane({
      atProfiles: true,
      activeTabId: id,
      activeHasTarget: true,
      routeHost: TAB_HOST,
      routeSession: '7',
    }),
    { kind: 'profiles' },
    'even over a terminal whose session the params still name',
  );
  // And absent means absent: the flag is opt-in, so every existing caller and
  // every other URL is unaffected.
  assert.deepEqual(pane({ hasLanding: true }), { kind: 'landing' });
  assert.deepEqual(pane({ atProfiles: false, hasLanding: true }), { kind: 'landing' });
});

test('a facts line shows every part the machine answered, and nothing else', () => {
  // Every string in the offer may be empty — a daemon that cannot answer one
  // sends `''` rather than omitting it — so gating the whole line on `os`
  // would hide an arch we do have. The two screens that draw this would then
  // disagree about the same machine, which is worse than either being terse.
  assert.equal(factsLine(['windows', 'x86_64', '10.0.26220']), 'windows · x86_64 · 10.0.26220');
  assert.equal(factsLine(['', 'aarch64', '']), 'aarch64');
  assert.equal(factsLine([undefined, 'aarch64']), 'aarch64');
  // Nothing said is an empty string, so the caller draws no element at all —
  // an empty one still takes its gap and reads as a fact that failed to load.
  assert.equal(factsLine(['', '', '']), '');
  assert.equal(factsLine([undefined, undefined]), '');
});

// --- the fleet pane's rows (#376) -------------------------------------------

const sess = (host: string, session: string): SessionEntry => ({
  host,
  session,
  title: 'zsh',
  cwd: '/src',
  cols: 80,
  rows: 24,
  altScreen: false,
  attached: false,
});

const DIAL = (() => {}) as unknown as Dial;

test('a row is clickable exactly when the seam can reach the machine it names', () => {
  // The bug this exists for: the pane derived each row's dial itself, from a
  // `DataPlane` plus an OPTIONAL relay. The hosted shell had no relay to give
  // it, so every row in a full list came out disabled — and because forgetting
  // the relay and correctly having none (loopback) are the same call, neither
  // the compiler nor the suite had anything to say.
  const reachable = new Set([TAB_HOST]);
  const rows = sessionRows(
    [sess(TAB_HOST, '1'), sess(OTHER_HOST, '2'), sess(TAB_HOST, '3')],
    (hostId) => (reachable.has(hostId) ? DIAL : null),
  );
  assert.deepEqual(
    rows.map((r) => r.dial !== null),
    [true, false, true],
  );
  // A row that cannot be reached is still a row: the session exists, and
  // dropping it would make the list disagree with what the machine reported.
  assert.equal(rows.length, 3);
});

test('a row dials the machine it names, not the one the pane was opened for', () => {
  // On loopback the two are always the same value, which is exactly why the
  // distinction has to be written down rather than left to whichever caller
  // remembers it. A pane opened on one machine, listing a session on another,
  // must ask about the second.
  const asked: string[] = [];
  sessionRows([sess(OTHER_HOST, '9')], (hostId) => {
    asked.push(hostId);
    return DIAL;
  });
  assert.deepEqual(asked, [OTHER_HOST], 'the entry names the machine, so the entry decides');
});

test('no sessions is no rows, and asks the seam nothing', () => {
  let calls = 0;
  const rows = sessionRows([], () => {
    calls += 1;
    return DIAL;
  });
  assert.deepEqual(rows, []);
  assert.equal(calls, 0, 'a mint or a ticket per empty render would be a real cost');
});

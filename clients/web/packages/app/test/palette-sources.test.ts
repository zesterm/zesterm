import { test } from 'node:test';
import assert from 'node:assert/strict';

import type { BlockPayload } from '@zesterm/proto';
import type { SessionEntry } from '@zesterm/control';

import type { BlockHit, BlockSearchView } from '../src/block-search.ts';
import {
  blockItems,
  hostItems,
  runTargetOf,
  sessionItems,
  type AttachedTabBlocks,
  type PaletteItem,
} from '../src/palette/sources.ts';
import { tabIdOf } from '../src/chrome-model.ts';
import type { Tab } from '../src/state/tabs.ts';

const NOW = 1_000_000_000_000;

function block(over: Partial<BlockPayload> & { id: number }): BlockPayload {
  return {
    prompt_line: 0,
    output_line: 1,
    end_line: 2,
    state: { state: 'finished', exit_code: 0 },
    command: 'echo hi',
    cwd: '~',
    ...over,
  };
}

function attached(over: Partial<AttachedTabBlocks>): AttachedTabBlocks {
  return { tabId: 'h1:1', hostId: 'h1', sessionId: '1', hostLabel: 'studio', blocks: [], ...over };
}

const NO_SEARCH: BlockSearchView = { query: '', hits: [], hostsAsked: 0, hostsAnswered: 0 };

function hit(over: Partial<BlockHit> & { block: number }): BlockHit {
  return {
    hostId: 'h2',
    session: '4',
    command: 'make',
    commandTruncated: false,
    cwd: '/src',
    state: { state: 'finished', exit_code: 0 },
    startedMs: NOW - 10_000,
    endedMs: NOW - 5_000,
    branch: '',
    author: null,
    ...over,
  };
}

function searched(...hits: BlockHit[]): BlockSearchView {
  return { query: '', hits, hostsAsked: 1, hostsAnswered: 1 };
}

function tab(over: Partial<Tab> & { id: string }): Tab {
  return {
    kind: 'session',
    title: 'zsh',
    command: '',
    hostId: 'h1',
    cwd: '~/dev',
    color: null,
    panes: [{ id: `${over.id}-p0`, hostId: 'h1', sessionId: '1', focused: true }],
    link: 'live',
    ...over,
  };
}

function entry(over: Partial<SessionEntry>): SessionEntry {
  return {
    host: 'h1',
    session: '1',
    title: 'zsh',
    cwd: '~',
    cols: 120,
    rows: 32,
    altScreen: false,
    attached: false,
    busy: false,
    context: null,
    ...over,
  };
}

test('provenance carries an age only when the host stamped a timestamp', () => {
  const items = blockItems(
    [
      attached({
        blocks: [
          block({ id: 1, started_ms: NOW - 125_000, ended_ms: NOW - 120_000 }),
          block({ id: 2 }),
        ],
      }),
    ],
    NO_SEARCH,
    {},
    NOW,
  );
  assert.deepEqual(
    items.map((i) => i.provenance),
    ['studio · 2m ago · exit 0', 'studio · exit 0'],
    'the design string is host · age · outcome, and a host that predates timestamps gets no fabricated age — never invent "just now"',
  );
  assert.deepEqual(
    items.map((i) => (i.kind === 'block' ? i.recency : 'not-a-block')),
    [NOW - 120_000, null],
    'recency rides the same stamp the age renders from; absent means null, not now',
  );
});

test('outcomes cross verbatim: exit ?, running, and the tone matches the rail', () => {
  const items = blockItems(
    [
      attached({
        blocks: [
          block({ id: 1, state: { state: 'finished', exit_code: null } }),
          block({ id: 2, state: { state: 'finished', exit_code: 2 } }),
          block({ id: 3, end_line: null, state: { state: 'running' }, started_ms: NOW - 4200 }),
        ],
      }),
    ],
    NO_SEARCH,
    {},
    NOW,
  );
  assert.deepEqual(
    items.map((i) => i.provenance.split(' · ').pop()),
    ['exit ?', 'exit 2', 'running'],
    'a null exit code is unknown (never a green tick), and a running block has no exit to report',
  );
  assert.deepEqual(
    items.map((i) => (i.kind === 'block' ? i.tone : 'not-a-block')),
    ['faint', 'danger', 'warn'],
    "the glyph tint is the blocks pane's rail palette — the two must never disagree",
  );
});

test('prompt-state and command-less blocks are not history', () => {
  const items = blockItems(
    [
      attached({
        blocks: [
          block({ id: 1, state: { state: 'prompt' }, command: '' }),
          block({ id: 2, command: '' }),
          block({ id: 3, command: 'cargo test' }),
        ],
      }),
    ],
    NO_SEARCH,
    {},
    NOW,
  );
  assert.deepEqual(
    items.map((i) => i.text),
    ['cargo test'],
    'an empty prompt is nothing to re-run; listing it would put a blank row under ⏎',
  );
});

test('sessions dedupe on the full (host, session) pair — open tab wins', () => {
  const open = tab({ id: tabIdOf('h1', '1') });
  const items = sessionItems(
    [open],
    [entry({ host: 'h1', session: '1' }), entry({ host: 'h2', session: '1', title: 'logs' })],
    { h1: 'studio' },
  );
  assert.deepEqual(
    items.map((i) => (i.kind === 'session' ? i.tabId : 'not-a-session')),
    [tabIdOf('h1', '1'), null],
    "a session reachable two ways is one terminal (the open tab's row), while h2's session 1 is a different terminal — full pair, not bare session id — and stays",
  );
});

test('an untitled session reads as shell, never a blank row', () => {
  const items = sessionItems([], [entry({ title: '' })], {});
  assert.equal(items[0]?.text, 'shell');
});

test('host rows carry the launcher copy and an honest sub-label', () => {
  const items = hostItems([{ id: 'a'.repeat(64), label: 'studio' }]);
  assert.equal(items[0]?.text, 'New session on studio');
  assert.equal(items[0]?.provenance, 'a'.repeat(12), 'the shortened id — real data, not a slogan');
});

test('⏎ resolves per row kind, and a block only types into the ACTIVE tab', () => {
  const blockItem: PaletteItem = {
    kind: 'block',
    tabId: 'h1:1',
    hostId: 'h1',
    sessionId: '1',
    blockId: 3,
    text: 'cargo test',
    provenance: '',
    recency: null,
    tone: 'success',
    runnable: true,
  };
  assert.deepEqual(
    runTargetOf(blockItem, 'h1:1'),
    { kind: 'run-block', tabId: 'h1:1', command: 'cargo test' },
    "the active tab's own prompt gate then decides whether typing is safe",
  );
  assert.deepEqual(
    runTargetOf(blockItem, 'h2:9'),
    { kind: 'activate-tab', tabId: 'h1:1' },
    'a background tab activates only — typing into a shell the user cannot see risks a running stdin',
  );

  const openSession: PaletteItem = {
    kind: 'session',
    tabId: 'h1:1',
    hostId: 'h1',
    sessionId: '1',
    text: 'zsh',
    provenance: '',
  };
  assert.deepEqual(runTargetOf(openSession, null), { kind: 'activate-tab', tabId: 'h1:1' });
  assert.deepEqual(
    runTargetOf({ ...openSession, tabId: null }, null),
    { kind: 'open-session', hostId: 'h1', sessionId: '1' },
    'a directory-only session opens rather than activating a tab that does not exist',
  );

  assert.deepEqual(
    runTargetOf({ kind: 'host', hostId: 'h2', text: '', provenance: '' }, null),
    { kind: 'create-session', hostId: 'h2' },
  );
  assert.deepEqual(
    runTargetOf(
      { kind: 'action', action: { kind: 'set-theme', themeId: 'paper' }, text: '', provenance: '' },
      null,
    ),
    { kind: 'set-theme', themeId: 'paper' },
  );
  assert.deepEqual(
    runTargetOf(
      { kind: 'action', action: { kind: 'layout-toggle' }, text: '', provenance: '' },
      null,
    ),
    { kind: 'layout-toggle' },
  );
  assert.deepEqual(
    runTargetOf(
      { kind: 'action', action: { kind: 'keybar-toggle' }, text: '', provenance: '' },
      null,
    ),
    { kind: 'keybar-toggle' },
    'the key bar is reachable from the palette, so an iPad with a hardware keyboard can turn it off',
  );
});

test('the same block seen from an attached grid and from its daemon is one row, the live one', () => {
  // The attached copy has a tab to run in and a fresher state; a second row
  // for the same (host, session, block) would race it under ⏎.
  const items = blockItems(
    [attached({ tabId: 'h1:1', hostId: 'h1', sessionId: '1', blocks: [block({ id: 7, command: 'make' })] })],
    searched(
      hit({ hostId: 'h1', session: '1', block: 7, command: 'make' }),
      hit({ hostId: 'h1', session: '1', block: 8, command: 'make -j' }),
    ),
    { h1: 'studio' },
    NOW,
  );
  assert.deepEqual(
    items.map((i) => [i.text, i.kind === 'block' ? i.tabId : '?']),
    [
      ['make', 'h1:1'],
      ['make -j', null],
    ],
    'block 7 once, as the tab’s own; block 8 from the daemon alone, with no tab',
  );
});

test('a stored block of a dead session renders its provenance from the host’s stamps', () => {
  const items = blockItems(
    [],
    searched(hit({ hostId: 'h2', session: null, block: 2, command: 'ffmpeg -i in.mov', state: { state: 'finished', exit_code: 1 } })),
    { h2: 'mac' },
    NOW,
  );
  assert.equal(items[0]?.provenance, 'mac · 5s ago · exit 1');
  assert.equal(items[0]?.kind === 'block' ? items[0].sessionId : '?', null);
  assert.equal(items[0]?.kind === 'block' ? items[0].tone : '?', 'danger');
  const unlabeled = blockItems([], searched(hit({ hostId: 'h2', block: 2 })), {}, NOW);
  assert.equal(unlabeled[0]?.provenance.split(' · ')[0], 'h2', 'the shortened id when no label is known');
});

test('⏎ on a block with no tab here runs it in the active tab, and nowhere with no tab at all', () => {
  const [stored] = blockItems([], searched(hit({ session: null, block: 2, command: 'make' })), {}, NOW);
  assert.ok(stored);
  assert.deepEqual(
    runTargetOf(stored, 'h1:1'),
    { kind: 'run-block', tabId: 'h1:1', command: 'make' },
    'the footer’s literal "run here"; the terminal’s own prompt gate still decides',
  );
  assert.deepEqual(runTargetOf(stored, null), { kind: 'nothing' }, 'no tab, nowhere to type');
});

test('a command the host cut is shown with its cut and runs nowhere', () => {
  const [cut] = blockItems(
    [],
    searched(hit({ session: null, block: 2, command: 'a'.repeat(40), commandTruncated: true })),
    {},
    NOW,
  );
  assert.ok(cut);
  assert.equal(cut.text, `${'a'.repeat(40)}…`);
  assert.deepEqual(runTargetOf(cut, 'h1:1'), { kind: 'nothing' }, 'the first four kilobytes of a script are not the script');
});

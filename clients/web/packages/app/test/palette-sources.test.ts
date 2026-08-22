import { test } from 'node:test';
import assert from 'node:assert/strict';

import type { BlockPayload } from '@zesterm/proto';
import type { SessionEntry } from '@zesterm/control';

import {
  blockItems,
  hostItems,
  hostsSearchedCount,
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
  return { tabId: 'h1:1', hostId: 'h1', hostLabel: 'studio', blocks: [], ...over };
}

function tab(over: Partial<Tab> & { id: string }): Tab {
  return {
    kind: 'session',
    title: 'zsh',
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

test('the hosts-searched count states the attached-host set, nothing broader', () => {
  const tabs = [
    attached({ tabId: 'h1:1', hostId: 'h1' }),
    attached({ tabId: 'h1:2', hostId: 'h1' }),
  ];
  assert.equal(
    hostsSearchedCount(tabs),
    1,
    'two tabs on one machine are one host searched — the count is machines, not tabs',
  );
  assert.equal(
    hostsSearchedCount([]),
    0,
    'a directory full of hosts with nothing attached searched zero of them — claiming more fabricates reach',
  );
});

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
    blockId: 3,
    text: 'cargo test',
    provenance: '',
    recency: null,
    tone: 'success',
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
});

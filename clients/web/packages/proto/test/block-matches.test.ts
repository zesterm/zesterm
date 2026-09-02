/**
 * The search answer (#527, #530): the first reply-only tag this client
 * models, and the one property every reader of it has to keep — it defaults,
 * it never throws. `parseHostMessage` runs with nothing to catch it in
 * `ConnectionClient`, so a throw here ends the fleet connection carrying every
 * other machine's listing, over one field of one history row.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { isBlockMatches, parseBlockMatch, parseHostMessage } from '../src/wire.ts';

const HOST = '2e'.repeat(32);

const LIVE = {
  host: HOST,
  session: 7,
  block: 3,
  title: 'zsh',
  command: 'cargo build --workspace',
  command_truncated: false,
  cwd: '/home/a/p',
  state: { state: 'finished', exit_code: 101 },
  started_ms: 1_756_800_000_000,
  ended_ms: 1_756_800_004_000,
  context: { branch: 'main', venv: '', kube: '' },
  author: 'ab'.repeat(32),
};

test('a live hit decodes every field the palette renders', () => {
  const m = parseBlockMatch(LIVE);
  assert.equal(m.host, HOST);
  assert.equal(m.session, 7);
  assert.equal(m.block, 3);
  assert.equal(m.title, 'zsh');
  assert.equal(m.command, 'cargo build --workspace');
  assert.equal(m.command_truncated, false);
  assert.equal(m.cwd, '/home/a/p');
  assert.deepEqual(m.state, { state: 'finished', exit_code: 101 });
  assert.equal(m.started_ms, 1_756_800_000_000, 'past u32, and still a number');
  assert.equal(m.ended_ms, 1_756_800_004_000);
  assert.equal(m.context?.branch, 'main');
  assert.equal(m.author, 'ab'.repeat(32));
});

test('a stored hit of a dead session decodes session as null, not zero', () => {
  // Zero is a real-looking id. Opening it would attach to whatever session
  // happens to hold that number now — which after a daemon restart is a
  // stranger's shell.
  const m = parseBlockMatch({ ...LIVE, session: null, author: null, context: null, started_ms: null, ended_ms: null });
  assert.equal(m.session, null);
  assert.equal(m.author, null);
  assert.equal(m.context, null);
  assert.equal(m.started_ms, null);
  assert.equal(m.ended_ms, null);
});

test('absent optionals and explicit nulls read the same', () => {
  // The Rust side spells every option as a plain `null`, but a reader must
  // not depend on that spelling: `Sessions.created` taught this file that
  // absent and null are one fact.
  const m = parseBlockMatch({ host: HOST, block: 1, state: { state: 'running' } });
  assert.equal(m.session, null);
  assert.equal(m.title, '');
  assert.equal(m.command, '');
  assert.equal(m.command_truncated, false);
  assert.equal(m.cwd, '');
  assert.equal(m.started_ms, null);
  assert.equal(m.ended_ms, null);
  assert.equal(m.context, null);
  assert.equal(m.author, null);
});

test('an unknown state reads as finished with no code, and never throws', () => {
  // `parseBlockPayload` throws on a state it cannot read, and rightly: a
  // keyframe block it would render wrongly. Here a throw ends the connection
  // — so the degradation is the never-a-green-tick answer instead.
  const m = parseBlockMatch({ ...LIVE, state: { state: 'paused' } });
  assert.deepEqual(m.state, { state: 'finished', exit_code: null });
  const missing = parseBlockMatch({ ...LIVE, state: undefined });
  assert.deepEqual(missing.state, { state: 'finished', exit_code: null });
});

test('a refusal is still a block_matches, with its reason and no rows', () => {
  const msg = parseHostMessage({
    t: 'block_matches',
    query: 'cargo',
    matches: [],
    error: 'history not searched: too many questions in flight; ask again',
  });
  assert.ok(isBlockMatches(msg), 'the consumer counts a refusal as an answer');
  if (!isBlockMatches(msg)) return;
  assert.equal(msg.query, 'cargo');
  assert.deepEqual(msg.matches, []);
  assert.equal(msg.truncated, false, 'absent defaults');
  assert.equal(msg.sessions, 0);
  assert.match(msg.error, /history not searched/);
});

test('the minimal successful answer decodes through its defaults', () => {
  // What a host with nothing matching sends: the tag, the echo, an empty
  // list, and nothing else — the successful case nobody tests by hand.
  const msg = parseHostMessage({ t: 'block_matches', query: 'x', matches: [] });
  assert.ok(isBlockMatches(msg));
  if (!isBlockMatches(msg)) return;
  assert.equal(msg.error, '');
  assert.equal(msg.truncated, false);
});

test('isBlockMatches rejects a merely carried message of the same tag', () => {
  // The `modeled()` contract: an `UnknownMessage` whose tag happens to read
  // `block_matches` is not one this client decoded.
  const carried = { t: 'block_matches', raw: {} } as const;
  assert.equal(isBlockMatches(carried), false);
});

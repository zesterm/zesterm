/**
 * The palette's fleet-wide block search store (#530): one question in
 * flight, the hosts it reached, every answer parked per host — and the echo
 * as the only correlation.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import type { BlockMatchesMessage } from '@zesterm/proto';

import { BLOCK_SEARCH_LIMIT, blockSearchStore } from '../src/block-search.ts';

const A = 'ab'.repeat(32);
const B = 'cd'.repeat(32);

function reply(query: string, ...commands: string[]): BlockMatchesMessage {
  return {
    t: 'block_matches',
    query,
    matches: commands.map((command, i) => ({
      host: A,
      session: 1,
      block: i + 1,
      title: '',
      command,
      command_truncated: false,
      cwd: '/',
      state: { state: 'finished', exit_code: 0 },
      started_ms: 1,
      ended_ms: 2,
      context: null,
      author: null,
    })),
    truncated: false,
    sessions: 1,
    error: '',
  };
}

test('a reply for a stale query is dropped; the echo is the only correlation', () => {
  // A slow host answering `ca` after the person typed `cargo` must not put
  // the broader answer where the narrower one belongs.
  const store = blockSearchStore();
  store.ask('ca', () => 1);
  store.ask('cargo', () => 1);
  store.answer(A, reply('ca', 'cat x'));
  assert.equal(store.view().hostsAnswered, 0, 'an answer to the earlier question goes nowhere');
  store.answer(A, reply('cargo', 'cargo b'));
  assert.deepEqual(
    store.view().hits.map((h) => h.command),
    ['cargo b'],
  );
});

test('hosts asked and answered are counts of frames and echoes, never of the fleet', () => {
  const store = blockSearchStore();
  store.ask('make', (q, limit) => {
    assert.equal(q, 'make');
    assert.equal(limit, BLOCK_SEARCH_LIMIT, 'the fan-out is handed the palette’s own limit');
    return 3;
  });
  assert.equal(store.view().hostsAsked, 3);
  assert.equal(store.view().hostsAnswered, 0);
  store.answer(A, reply('make', 'make'));
  store.answer(B, reply('make'));
  assert.equal(store.view().hostsAnswered, 2, 'an empty answer is still an answer');
  assert.equal(store.view().hostsAsked, 3, 'one host is still pending, and the row will say so');
});

test('a new question drops every earlier answer, so a fast host’s old rows never show under it', () => {
  const store = blockSearchStore();
  store.ask('a', () => 1);
  store.answer(A, reply('a', 'apt'));
  assert.equal(store.view().hits.length, 1);
  store.ask('b', () => 1);
  assert.equal(store.view().hits.length, 0);
  assert.equal(store.view().hostsAnswered, 0);
});

test('a refusal counts as an answer, with no rows', () => {
  // The host *did* answer — "history not searched" is its word — and a row
  // that kept waiting on it would say `searching 1 host…` for ever.
  const store = blockSearchStore();
  store.ask('x', () => 1);
  store.answer(A, { ...reply('x'), error: 'history not searched: ask again' });
  assert.equal(store.view().hostsAnswered, 1);
  assert.deepEqual(store.view().hits, []);
});

test('hits are reference-stable between answers', () => {
  // The palette reads it on every keystroke, and a watch that compares by
  // reference must not see a list that is never equal to itself.
  const store = blockSearchStore();
  store.ask('m', () => 2);
  store.answer(A, reply('m', 'make'));
  const first = store.view().hits;
  assert.equal(store.view().hits, first, 'the same list until something changes');
  store.answer(B, reply('m'));
  assert.notEqual(store.view().hits, first, 'and a new one when an answer lands');
});

test('a hit is projected the way the palette keys things', () => {
  const store = blockSearchStore();
  store.ask('c', () => 1);
  const stored = { ...reply('c', 'cargo test').matches[0]!, session: null, command_truncated: true, context: { branch: 'main', venv: '', kube: '' } };
  store.answer(B, { ...reply('c'), matches: [stored] });
  const [hit] = store.view().hits;
  assert.ok(hit);
  assert.equal(hit.hostId, B, 'keyed by the host that answered, not a field in the row');
  assert.equal(hit.session, null, 'a stored block of a dead session stays null');
  assert.equal(hit.commandTruncated, true);
  assert.equal(hit.branch, 'main');
});

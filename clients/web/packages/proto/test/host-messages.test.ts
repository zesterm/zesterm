/**
 * Reply-only host messages decoded from the bytes `rmp_serde` actually wrote
 * (#530).
 *
 * The recordings pin every message a session produces, and the client-message
 * golden pins what this side *encodes*. A reply to a question the browser
 * asked rode neither, so the null-versus-absent spelling of its options was
 * asserted by a test that typed the JSON it then decoded — which agrees with
 * itself and not with the Rust encoder. `host-messages.json` is the same
 * framed encoding a daemon puts on the wire, and this reads it the way
 * `ConnectionClient` does: frame, msgpack, `parseHostMessage`.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { FrameReader } from '../src/frame.ts';
import { decode } from '../src/msgpack.ts';
import { isBlockMatches, parseHostMessage } from '../src/wire.ts';
import { hexToBytes, loadHostMessages } from './fixtures.ts';

const HOST = '2e'.repeat(32);

function decodeGolden(name: string) {
  const golden = loadHostMessages();
  assert.equal(golden.protocol, 3, 'the goldens were written for a different protocol');
  const entry = golden.messages.find((m) => m.name === name);
  assert.ok(entry, `no golden named ${name}`);
  const reader = new FrameReader();
  reader.feed(hexToBytes(entry.wire));
  const body = reader.next();
  assert.ok(body, 'one whole frame');
  assert.equal(reader.next(), undefined, 'and nothing after it');
  return parseHostMessage(decode(body));
}

test('a block_matches reply decodes from the real encoding, live row and stored row alike', () => {
  const msg = decodeGolden('block_matches');
  assert.ok(isBlockMatches(msg));
  if (!isBlockMatches(msg)) return;
  assert.equal(msg.query, 'cargo');
  assert.equal(msg.truncated, true);
  assert.equal(msg.sessions, 2);
  assert.equal(msg.error, '', 'skipped when empty on the wire, and that reads as empty');

  const [live, stored] = msg.matches;
  assert.ok(live && stored);
  assert.equal(live.host, HOST);
  assert.equal(live.session, 7, 'a live block names its session');
  assert.equal(live.block, 3);
  assert.equal(live.title, 'zsh');
  assert.equal(live.command, 'cargo build --workspace');
  assert.equal(live.command_truncated, false);
  assert.deepEqual(live.state, { state: 'finished', exit_code: 101 });
  assert.equal(live.started_ms, 1_756_800_000_000, 'past u32: the eight-byte form, read as a number');
  assert.equal(live.ended_ms, 1_756_800_004_000);
  assert.equal(live.context?.branch, 'main');
  assert.equal(live.author, 'ab'.repeat(32));

  assert.equal(stored.session, null, 'a block only the store remembers names no session');
  assert.equal(stored.block, 9);
  assert.equal(stored.title, '');
  assert.equal(stored.command_truncated, true, 'the host cut it, and says so');
  assert.deepEqual(stored.state, { state: 'finished', exit_code: null });
  assert.equal(stored.started_ms, null);
  assert.equal(stored.ended_ms, null);
  assert.equal(stored.context, null);
  assert.equal(stored.author, null);
});

test('a refusal decodes as a block_matches carrying its reason', () => {
  const msg = decodeGolden('block_matches_refused');
  assert.ok(isBlockMatches(msg), 'a refusal is this message with error set, never an error message');
  if (!isBlockMatches(msg)) return;
  assert.deepEqual(msg.matches, []);
  assert.match(msg.error, /history not searched/);
  assert.equal(msg.sessions, 0);
});

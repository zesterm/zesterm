/**
 * The encoder held byte-equal to the host's own `rmp_serde` output.
 *
 * `client-messages.json` carries one canonical framed encoding of every
 * `ClientMessage` variant, written by `cargo xtask fixtures` from the real
 * Rust types. The corpus re-encode test proves parity on host *messages*;
 * this one proves it on client messages, whose bytes no recording carries —
 * and it is the test that fails first if `rmp_serde` ever changes field
 * order, integer narrowing, or the `Vec<u8>` sequence form.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { type ClientMessage, encodeClientMessage } from '../src/wire-client.ts';
import { hexToBytes, loadClientMessages } from './fixtures.ts';

const HOST = '2e'.repeat(32);
const SESSION = { host: HOST, session: 7n };

/** The same canonical values `fixture_dump.rs`'s `write_client_messages` uses. */
const CANONICAL: Record<string, ClientMessage> = {
  hello: {
    t: 'hello',
    version: 2,
    client: 'ab'.repeat(32),
    label: 'golden',
    nonce: '5c'.repeat(32),
    watch_sessions: true,
  },
  auth: { t: 'auth', signature: 'ef'.repeat(64) },
  pairing_decision: { t: 'pairing_decision', client: 'ab'.repeat(32), approve: false },
  request_keyframe: { t: 'request_keyframe', session: SESSION },
  list_sessions: { t: 'list_sessions' },
  create_session: { t: 'create_session', command: 'htop', cwd: '/tmp', cols: 300, rows: 80 },
  attach: { t: 'attach', session: SESSION, cols: 120, rows: 40 },
  detach: { t: 'detach', session: SESSION },
  input: { t: 'input', session: SESSION, bytes: Uint8Array.of(0x1b, 0x5b, 0x41, 0x80, 0xff) },
  resize: { t: 'resize', session: SESSION, cols: 80, rows: 24 },
  ack: { t: 'ack', session: SESSION, seq: 4294967296n },
  request_scrollback: { t: 'request_scrollback', session: SESSION, from_line: -1200, count: 500 },
  close_session: { t: 'close_session', session: SESSION },
};

test('every client message variant has a golden, and every golden a construction', () => {
  const golden = loadClientMessages();
  const names = golden.messages.map((m) => m.name).sort();
  assert.deepEqual(
    names,
    Object.keys(CANONICAL).sort(),
    'the golden set and this test must cover the same variants -- a variant in one ' +
      'and not the other is an encoding nobody is checking',
  );
});

test('the TypeScript encoder produces the exact bytes rmp_serde wrote', () => {
  const golden = loadClientMessages();
  assert.equal(golden.protocol, 2, 'the goldens were written for a different protocol');

  for (const entry of golden.messages) {
    const msg = CANONICAL[entry.name];
    assert.ok(msg, `no construction for golden ${entry.name}`);
    assert.deepEqual(
      encodeClientMessage(msg),
      hexToBytes(entry.wire),
      `${entry.name}: the encoding diverged from the host's -- field order, ` +
        `integer width, or the Vec<u8> form changed on one side only`,
    );
  }
});

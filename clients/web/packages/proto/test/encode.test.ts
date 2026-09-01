/**
 * The encoder half, proven two ways.
 *
 * Round-trips through our own decoder catch self-consistency; the stronger
 * check is **re-encoding the recorded corpus**: every fixture frame was
 * written by `rmp_serde::to_vec_named`, so `encode(decode(bytes))` must
 * reproduce the identical bytes — narrowest integers, insertion-order maps,
 * fixstr headers and all. 44 recorded frames make that a parity proof against
 * the real Rust encoder, not against this package's opinion of itself.
 *
 * (A type-level check against the generated `ClientMessage` binding is *not*
 * attempted in the sending direction: ours is deliberately wider — `bigint |
 * number` ids, `Uint8Array` input bytes — and the byte-golden fixture planned
 * beside `expect.blocks` asserts the thing that actually matters, which is
 * the bytes.)
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { decode } from '../src/msgpack.ts';
import { encode } from '../src/msgpack-encode.ts';
import { FrameReader, encodeFrame } from '../src/frame.ts';
import {
  type ClientMessage,
  bytesToHex,
  encodeClientMessage,
  hexToBytes as clientHex,
} from '../src/wire-client.ts';
import { fixtureNames, hexToBytes, loadFixture } from './fixtures.ts';

/** Strip the length prefix and decode, asserting the frame was whole. */
function unframe(framed: Uint8Array): unknown {
  const r = new FrameReader();
  r.feed(framed);
  const body = r.next();
  assert.ok(body, 'the encoder must emit a complete frame');
  assert.equal(r.pending, 0, 'nothing may trail the frame');
  return decode(body);
}

test('every fixture frame re-encodes to the exact bytes rmp_serde wrote', () => {
  // The parity proof. If this holds across the whole corpus, the encoder
  // makes the same choices as the host's: narrowest integer that fits,
  // fields in declaration order, fixstr/fixmap headers where they fit.
  let frames = 0;
  for (const name of fixtureNames()) {
    const fixture = loadFixture(name);
    for (const frame of fixture.frames) {
      const framed = hexToBytes(frame.wire);
      const body = framed.slice(4);
      const reencoded = encode(decode(body));
      assert.deepEqual(
        reencoded,
        body,
        `${name} seq ${frame.seq}: re-encoding diverged from the host's bytes`,
      );
      frames++;
    }
  }
  assert.ok(frames >= 44, `the corpus shrank to ${frames} frames -- was it regenerated?`);
});

test('integers take the narrowest encoding, exactly as rmp_serde picks it', () => {
  // One wrong width and the parity test above fails on some future recording
  // instead of here, where the rule is stated.
  const cases: Array<[number | bigint, number[]]> = [
    [0, [0x00]],
    [5, [0x05]],
    [127, [0x7f]],
    [128, [0xcc, 0x80]],
    [255, [0xcc, 0xff]],
    [256, [0xcd, 0x01, 0x00]],
    [65535, [0xcd, 0xff, 0xff]],
    [65536, [0xce, 0x00, 0x01, 0x00, 0x00]],
    [4294967295, [0xce, 0xff, 0xff, 0xff, 0xff]],
    [4294967296, [0xcf, 0, 0, 0, 1, 0, 0, 0, 0]],
    [2n ** 64n - 1n, [0xcf, 255, 255, 255, 255, 255, 255, 255, 255]],
    [-1, [0xff]],
    [-32, [0xe0]],
    [-33, [0xd0, 0xdf]],
    [-128, [0xd0, 0x80]],
    [-129, [0xd1, 0xff, 0x7f]],
    [-32768, [0xd1, 0x80, 0x00]],
    [-32769, [0xd2, 0xff, 0xff, 0x7f, 0xff]],
    [-2147483648, [0xd2, 0x80, 0x00, 0x00, 0x00]],
    [-2147483649, [0xd3, 255, 255, 255, 255, 127, 255, 255, 255]],
  ];
  for (const [value, bytes] of cases) {
    assert.deepEqual(encode(value), Uint8Array.from(bytes), `encoding ${value}`);
  }
});

test('a Uint8Array encodes as a sequence of integers, never bin', () => {
  // Serde's `Vec<u8>` without `serde_bytes` is a seq — and the decoder's
  // subset would reject `bin` anyway, which makes this mistake self-catching
  // only if someone round-trips. Assert it directly instead.
  assert.deepEqual(encode(Uint8Array.of(1, 2, 200)), encode([1, 2, 200]));
});

test('floats are refused rather than smuggled to the host', () => {
  assert.throws(() => encode(1.5), /float/);
});

test('an out-of-range integer is refused rather than wrapped', () => {
  assert.throws(() => encode(2n ** 64n), /u64/);
  assert.throws(() => encode(-(2n ** 63n) - 1n), /i64/);
});

test('encodeFrame writes the little-endian prefix the daemon strips', () => {
  const framed = encodeFrame(Uint8Array.of(0xc3));
  assert.deepEqual(framed, Uint8Array.of(1, 0, 0, 0, 0xc3));
});

test('every client message round-trips through the real decoder', () => {
  const session = { host: 'ab'.repeat(32), session: 7n };
  const messages: ClientMessage[] = [
    {
      t: 'hello',
      version: 3,
      client: 'cd'.repeat(32),
      label: 'web',
      nonce: '12'.repeat(32),
      dh: '34'.repeat(32),
      watch_sessions: true,
      watch_pairings: false,
      watch_hosts: false,
      watch_signals: false,
      agent: false,
    },
    { t: 'auth', signature: 'ef'.repeat(64) },
    { t: 'pairing_decision', client: 'cd'.repeat(32), approve: false },
    { t: 'request_keyframe', session },
    { t: 'list_sessions' },
    { t: 'create_session', command: '', cwd: '/home', cols: 120, rows: 40 },
    { t: 'attach', session, cols: 120, rows: 40 },
    { t: 'detach', session },
    { t: 'input', session, bytes: Uint8Array.of(0x1b, 0x5b, 0x41) },
    { t: 'resize', session, cols: 80, rows: 24 },
    { t: 'ack', session, seq: 41n },
    { t: 'request_scrollback', session, from_line: -120, count: 200 },
    { t: 'close_session', session },
  ];

  for (const msg of messages) {
    const decoded = unframe(encodeClientMessage(msg)) as Record<string, unknown>;
    assert.equal(decoded['t'], msg.t, 'the tag must survive');
    // The tag leads: `#[serde(tag = "t")]` writes it first, and a golden
    // fixture can only ever match if we do too.
    assert.equal(Object.keys(decoded)[0], 't', `${msg.t}: the tag must be the first key`);
  }
});

test('the wire map is built in declaration order, whatever order the caller used', () => {
  // Key order is a wire property here, not a style choice. A caller building
  // the object "backwards" must still produce canonical bytes.
  const backwards = {
    watch_hosts: false,
    watch_signals: false,
    agent: false,
    watch_pairings: false,
    watch_sessions: false,
    nonce: '00'.repeat(32),
    label: 'x',
    client: 'ab'.repeat(32),
    version: 3,
    dh: '77'.repeat(32),
    t: 'hello',
  } as ClientMessage;
  const forwards: ClientMessage = {
    t: 'hello',
    version: 3,
    client: 'ab'.repeat(32),
    label: 'x',
    nonce: '00'.repeat(32),
    dh: '77'.repeat(32),
    watch_sessions: false,
    watch_pairings: false,
    watch_hosts: false,
    watch_signals: false,
    agent: false,
  };
  assert.deepEqual(encodeClientMessage(backwards), encodeClientMessage(forwards));
});

test('input bytes reach the wire as the integer sequence serde expects', () => {
  const msg: ClientMessage = {
    t: 'input',
    session: { host: '00'.repeat(32), session: 1 },
    bytes: Uint8Array.of(104, 105),
  };
  const decoded = unframe(encodeClientMessage(msg)) as Record<string, unknown>;
  assert.deepEqual(decoded['bytes'], [104, 105]);
});

test('hex helpers are strict inverses and lowercase', () => {
  // `zest-proto/src/hex.rs` refuses padding and wrong lengths; the client
  // side must be no laxer or two different ids could compare equal.
  const bytes = Uint8Array.of(0x00, 0x0f, 0xab, 0xff);
  assert.equal(bytesToHex(bytes), '000fabff');
  assert.deepEqual(clientHex('000fabff'), bytes);
  assert.throws(() => clientHex('abc'), /odd/);
  assert.throws(() => clientHex('zz'), /not hex/);
});

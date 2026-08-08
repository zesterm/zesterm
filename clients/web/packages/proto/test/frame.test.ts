/**
 * Framing and MessagePack, on inputs a fixture cannot produce.
 *
 * The corpus proves both work on well-formed traffic. These cover the cases that
 * only ever arrive from a broken or hostile peer, where "returns something
 * plausible" is much worse than "throws".
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { FrameError, FrameReader, MAX_FRAME } from '../src/frame.ts';
import { MsgpackError, decode } from '../src/msgpack.ts';

function framed(body: number[]): Uint8Array {
  const out = new Uint8Array(4 + body.length);
  new DataView(out.buffer).setUint32(0, body.length, true);
  out.set(body, 4);
  return out;
}

test('a frame split across reads is reassembled', () => {
  // The case that works on loopback and fails on a LAN: a stream is a stream,
  // and a header can arrive in pieces.
  const whole = framed([0xc3]); // `true`
  const r = new FrameReader();

  for (const byte of whole.slice(0, whole.length - 1)) {
    r.feed(Uint8Array.of(byte));
    assert.equal(r.next(), undefined, 'a partial frame was returned as complete');
  }

  r.feed(whole.slice(whole.length - 1));
  assert.deepEqual(r.next(), Uint8Array.of(0xc3));
  assert.equal(r.pending, 0);
});

test('two frames in one read come out separately', () => {
  const r = new FrameReader();
  const a = framed([0xc2]);
  const b = framed([0xc3]);
  const both = new Uint8Array(a.length + b.length);
  both.set(a, 0);
  both.set(b, a.length);

  r.feed(both);
  assert.deepEqual(r.next(), Uint8Array.of(0xc2));
  assert.deepEqual(r.next(), Uint8Array.of(0xc3));
  assert.equal(r.next(), undefined);
});

test('an oversized length is refused before anything is allocated', () => {
  const r = new FrameReader();
  const header = new Uint8Array(4);
  new DataView(header.buffer).setUint32(0, MAX_FRAME + 1, true);
  r.feed(header);
  assert.throws(() => r.next(), FrameError);
});

test('the length prefix is little-endian', () => {
  // Getting this backwards makes a 1-byte frame look like a 16MB one, which the
  // limit above then rejects -- so the failure is loud but points at the wrong
  // thing. Pinned here so it points at the right one.
  const r = new FrameReader();
  r.feed(Uint8Array.of(0x01, 0x00, 0x00, 0x00, 0xc3));
  assert.deepEqual(r.next(), Uint8Array.of(0xc3));
});

test('msgpack reads the shapes the wire actually uses', () => {
  assert.equal(decode(Uint8Array.of(0x00)), 0);
  assert.equal(decode(Uint8Array.of(0x7f)), 127);
  assert.equal(decode(Uint8Array.of(0xff)), -1);
  assert.equal(decode(Uint8Array.of(0xc0)), null);
  assert.equal(decode(Uint8Array.of(0xc2)), false);
  assert.equal(decode(Uint8Array.of(0xa3, 0x61, 0x62, 0x63)), 'abc');
  assert.deepEqual(decode(Uint8Array.of(0x92, 0x01, 0x02)), [1, 2]);
  assert.deepEqual(decode(Uint8Array.of(0x81, 0xa1, 0x61, 0x01)), { a: 1 });
});

test('a 64-bit integer stays exact past the safe range', () => {
  // `Seq` is a u64. Below 2^53 it is a number and above it a bigint, because
  // there is no third option that is both exact and a number.
  const big = Uint8Array.of(0xcf, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff);
  assert.equal(decode(big), 18446744073709551615n);

  const small = Uint8Array.of(0xcf, 0, 0, 0, 0, 0, 0, 0, 7);
  assert.equal(decode(small), 7, 'a small u64 should not force callers into bigint arithmetic');
});

test('a truncated value throws instead of returning a partial one', () => {
  assert.throws(() => decode(Uint8Array.of(0xa5, 0x61)), MsgpackError);
});

test('trailing bytes are an error, not something to ignore', () => {
  // The frame and the message disagreeing about length is a framing bug, and
  // swallowing it here would surface much later as a missing update.
  assert.throws(() => decode(Uint8Array.of(0xc3, 0xc3)), MsgpackError);
});

test('an unsupported type byte names itself', () => {
  // 0xc7 is ext8, which this decoder does not implement. The message has to say
  // which byte and where, or the next person guesses.
  assert.throws(
    () => decode(Uint8Array.of(0xc7)),
    (e: Error) => e instanceof MsgpackError && /0xc7/.test(e.message),
  );
});

test('invalid UTF-8 is rejected rather than silently replaced', () => {
  // Replacement characters would land in the grid as content and compare equal
  // to nothing the host has.
  assert.throws(() => decode(Uint8Array.of(0xa2, 0xff, 0xfe)), TypeError);
});

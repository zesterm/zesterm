/**
 * The two implementations of one format, pinned to each other.
 *
 * `crates/zest-mesh/src/enroll.rs` signs; this verifies. They share no code and
 * no test data unless it is copied by hand, so both fixtures below came out of
 * the Rust — the preimage hex is `the_preimage_is_stable`'s own golden verbatim,
 * and the signature was produced by `HostIdentity::from_secret_bytes(&[7; 32])`
 * signing `Purpose::Enrollment` over it.
 *
 * Without these, a drift in either direction surfaces at bring-up as a
 * signature mismatch that names nothing — the daemon says it was refused, the
 * Worker says the signature is bad, and neither can say which side moved.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { fromHex, hex, utf8 } from '@zesterm/cloud-shared';

import {
  enrollmentPreimage,
  enrollmentRequest,
  KEY_LEN,
  SIGNATURE_LEN,
  verifyEnrollment,
} from '../src/enroll/preimage.ts';

/** `zest-mesh`'s `the_preimage_is_stable`, copied byte for byte. */
const RUST_GOLDEN =
  '7a65737465726d2d656e726f6c6c2d763100084142434431323334' +
  '1111111111111111111111111111111111111111111111111111111111111111' +
  '0008616e64792d6d6163';

// `HostIdentity::from_secret_bytes(&[7u8; 32])` in `zest-mesh`.
const RUST_HOST = 'ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c';
const RUST_SIG =
  '6644de2c469301328c3baa74073f4b1afe318fe577d46a6c07d9ccd1e038dda1' +
  'd38ecdd0b40b9c85862e0ef03fb4dcc436537365f946ffa4b350b9b5d9b91103';

function key(hexText: string): Uint8Array {
  const bytes = fromHex(hexText, KEY_LEN);
  assert.ok(bytes, 'the fixture must be 32 bytes of hex');
  return bytes;
}

function sig(hexText: string): Uint8Array {
  const bytes = fromHex(hexText, SIGNATURE_LEN);
  assert.ok(bytes, 'the fixture must be 64 bytes of hex');
  return bytes;
}

test('the enrolment request is the exact bytes the Rust produces', () => {
  const bytes = enrollmentRequest('ABCD1234', new Uint8Array(32).fill(0x11), 'andy-mac');
  assert.equal(
    hex(bytes),
    RUST_GOLDEN,
    'the daemon signs what the Rust builds; a byte of difference here refuses every real enrolment',
  );
});

test('the signing prefix is the one the Rust signs under', () => {
  const request = enrollmentRequest('ABCD1234', new Uint8Array(32).fill(0x11), 'andy-mac');
  const preimage = enrollmentPreimage('host', 'ABCD1234', new Uint8Array(32).fill(0x11), 'andy-mac');
  const prefix = utf8('zesterm-sig-v1\0host\0enrollment\0');

  assert.deepEqual(
    preimage,
    new Uint8Array([...prefix, ...request]),
    'the prefix is what keeps an enrolment signature from also being a login',
  );
});

test('a signature the Rust made verifies here', async () => {
  assert.ok(
    await verifyEnrollment({
      role: 'host',
      code: 'ABCD1234',
      key: key(RUST_HOST),
      label: 'andy-mac',
      signature: sig(RUST_SIG),
    }),
    'this is the whole cross-implementation contract: the daemon signed it in Rust and the Worker must accept it',
  );
});

test('changing any signed field invalidates the Rust signature', async () => {
  const signature = sig(RUST_SIG);
  const cases: Array<[string, Parameters<typeof verifyEnrollment>[0]]> = [
    [
      'a different code',
      { role: 'host', code: 'WRONG123', key: key(RUST_HOST), label: 'andy-mac', signature },
    ],
    [
      'a different label -- and the label is attacker-chosen',
      { role: 'host', code: 'ABCD1234', key: key(RUST_HOST), label: 'someone-else', signature },
    ],
    [
      'the client role -- an enrolment for a machine must not enrol a browser key',
      { role: 'client', code: 'ABCD1234', key: key(RUST_HOST), label: 'andy-mac', signature },
    ],
  ];
  for (const [why, args] of cases) {
    assert.equal(await verifyEnrollment(args), false, why);
  }
});

test('the boundary between code and label cannot be moved', () => {
  // The reason the preimage is length-prefixed rather than NUL-separated.
  // Concatenated, ("ab","cd") and ("abc","d") are identical bytes, so a
  // signature over one would verify the other -- and the label is chosen by
  // whoever is enrolling.
  const k = new Uint8Array(32).fill(0x11);
  assert.notDeepEqual(
    enrollmentRequest('ab', k, 'cd'),
    enrollmentRequest('abc', k, 'd'),
    'field boundaries must be unambiguous',
  );
});

test('a malformed signature or key is false, never a throw', async () => {
  // noble raises on a wrong-sized input and on a point that does not decode.
  // A caller asking "did this verify" must not also have to handle an
  // exception, or the first bad request from a stranger is a 500.
  const cases = [
    { key: new Uint8Array(31), signature: new Uint8Array(SIGNATURE_LEN) },
    { key: new Uint8Array(KEY_LEN), signature: new Uint8Array(63) },
    // Valid lengths, nonsense contents: `[2; 32]` is not a curve point.
    { key: new Uint8Array(KEY_LEN).fill(2), signature: new Uint8Array(SIGNATURE_LEN).fill(9) },
  ];
  for (const { key: k, signature } of cases) {
    assert.equal(
      await verifyEnrollment({ role: 'host', code: 'ABCD1234', key: k, label: 'x', signature }),
      false,
      `${k.length}-byte key, ${signature.length}-byte signature`,
    );
  }
});

test('a small-order key cannot enrol', async () => {
  // The all-zero encoding is a valid point and verifies almost anything under
  // ZIP-215 semantics, which is noble's default. The Rust uses dalek's
  // `verify_strict` and refuses it; left at the default this side would admit
  // an id that answers for signatures it never made.
  assert.equal(
    await verifyEnrollment({
      role: 'host',
      code: 'ABCD1234',
      key: new Uint8Array(KEY_LEN),
      label: 'andy-mac',
      signature: new Uint8Array(SIGNATURE_LEN),
    }),
    false,
    'a small-order key must not be able to enter the registry at all',
  );
});

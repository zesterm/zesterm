/**
 * The two hand-built encoders of one format, pinned to each other.
 *
 * `clients/web/packages/auth/src/register.ts` signs; this verifies. The two
 * workspaces share no code on purpose (three projects, three lockfiles), so
 * the golden below is asserted byte-identically in BOTH — here and in
 * `clients/web/packages/auth/test/register.test.ts`. The signature can be a
 * fixture too because Ed25519 signing is deterministic: same seed, same
 * bytes, same 64 bytes out, on either side.
 *
 * Golden provenance: seed `[7; 32]` (the enrolment tests' `testKey(7)`),
 * account `user-a`, label `this browser`.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { fromHex, hex, utf8 } from '@zesterm/cloud-shared';

import { KEY_LEN, SIGNATURE_LEN, enrollmentRequest } from '../src/enroll/preimage.ts';
import {
  registerPreimage,
  registerRequest,
  verifyRegistration,
} from '../src/enroll/register-preimage.ts';

/** The public key of seed `[7; 32]` — the same key the enrolment tests use. */
const GOLDEN_KEY = 'ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c';

const GOLDEN_ACCOUNT = 'user-a';
const GOLDEN_LABEL = 'this browser';

/** `registerRequest('user-a', key, 'this browser')`, byte for byte. */
const GOLDEN_REQUEST =
  '7a65737465726d2d72656769737465722d7631' + // "zesterm-register-v1"
  '0006757365722d61' + // u16be(6) ++ "user-a"
  GOLDEN_KEY +
  '000c746869732062726f77736572'; // u16be(12) ++ "this browser"

/** Seed `[7; 32]` signing `preimage('client', 'enrollment', request)`. */
const GOLDEN_SIG =
  '262a57a3cb8eaa8fe5f5f1630c28bfc86b70d06ed27ee694f1abddb82e69e8c6' +
  '4abc148ff3859609df09c52f977dc5d2486fb53ef189dd25d5254b385c23a800';

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

test('the register request is the exact bytes the browser signs', () => {
  const bytes = registerRequest(GOLDEN_ACCOUNT, key(GOLDEN_KEY), GOLDEN_LABEL);
  assert.equal(
    hex(bytes),
    GOLDEN_REQUEST,
    'the browser builds these bytes in its own workspace; a byte of difference refuses every real registration',
  );
});

test('the signing prefix is the client role under enrollment', () => {
  const request = registerRequest(GOLDEN_ACCOUNT, key(GOLDEN_KEY), GOLDEN_LABEL);
  const preimage = registerPreimage(GOLDEN_ACCOUNT, key(GOLDEN_KEY), GOLDEN_LABEL);
  const prefix = utf8('zesterm-sig-v1\0client\0enrollment\0');
  assert.deepEqual(
    preimage,
    new Uint8Array([...prefix, ...request]),
    'the wrap must be exactly what ClientSigner.sign("enrollment", request) applies',
  );
});

test('a signature the browser workspace made verifies here', async () => {
  assert.ok(
    await verifyRegistration({
      account: GOLDEN_ACCOUNT,
      key: key(GOLDEN_KEY),
      label: GOLDEN_LABEL,
      signature: sig(GOLDEN_SIG),
    }),
    'the whole cross-workspace contract: the auth package signed it and the Worker must accept it',
  );
});

test('changing any signed field invalidates the signature', async () => {
  const signature = sig(GOLDEN_SIG);
  const cases: Array<[string, Parameters<typeof verifyRegistration>[0]]> = [
    [
      'a different account -- the binding that stops cross-account replay',
      { account: 'user-b', key: key(GOLDEN_KEY), label: GOLDEN_LABEL, signature },
    ],
    [
      'a different label -- and the label is caller-chosen',
      { account: GOLDEN_ACCOUNT, key: key(GOLDEN_KEY), label: 'someone else', signature },
    ],
  ];
  for (const [why, args] of cases) {
    assert.equal(await verifyRegistration(args), false, why);
  }
});

test('the register and enroll domains cannot produce the same bytes', () => {
  // Both sign under Purpose::Enrollment, so the outer domains are the whole
  // separation: "zesterm-enroll-v1" and "zesterm-register-v1" diverge at byte
  // 8, before any caller-supplied field, so no message under one can be bytes
  // under the other. This pins that argument to the actual constants.
  const enroll = utf8('zesterm-enroll-v1');
  const register = utf8('zesterm-register-v1');
  const at = [...enroll].findIndex((b, i) => b !== register[i]);
  assert.equal(at, 8, 'the domains must diverge before any variable content begins');

  const k = key(GOLDEN_KEY);
  assert.notDeepEqual(
    enrollmentRequest('ABCD1234', k, 'x'),
    registerRequest('ABCD1234', k, 'x'),
    'same fields, different domain, different bytes',
  );
});

test('the boundary between account and label cannot be moved', () => {
  // The reason both fields are length-prefixed: concatenated, ("ab","cd") and
  // ("abc","d") are identical bytes, and the label is chosen by whoever is
  // registering.
  const k = key(GOLDEN_KEY);
  assert.notDeepEqual(
    registerRequest('ab', k, 'cd'),
    registerRequest('abc', k, 'd'),
    'field boundaries must be unambiguous',
  );
});

test('a malformed signature or key is false, never a throw', async () => {
  // noble raises on a wrong-sized input and a point that does not decode; a
  // caller asking "did this verify" must not also handle an exception.
  const cases = [
    { key: new Uint8Array(31), signature: new Uint8Array(SIGNATURE_LEN) },
    { key: new Uint8Array(KEY_LEN), signature: new Uint8Array(63) },
    { key: new Uint8Array(KEY_LEN).fill(2), signature: new Uint8Array(SIGNATURE_LEN).fill(9) },
  ];
  for (const { key: k, signature } of cases) {
    assert.equal(
      await verifyRegistration({ account: 'user-a', key: k, label: 'x', signature }),
      false,
      `${k.length}-byte key, ${signature.length}-byte signature`,
    );
  }
});

test('a small-order key cannot register', async () => {
  // The all-zero encoding verifies almost anything under ZIP-215 semantics,
  // noble's default. `zip215: false` mirrors `verifyEnrollment`: the Rust half
  // of the fleet uses dalek's verify_strict, and a key that answers for
  // signatures it never made must not enter the registry through any door.
  assert.equal(
    await verifyRegistration({
      account: GOLDEN_ACCOUNT,
      key: new Uint8Array(KEY_LEN),
      label: GOLDEN_LABEL,
      signature: new Uint8Array(SIGNATURE_LEN),
    }),
    false,
    'a small-order key must be refused here exactly as at the claim',
  );
});

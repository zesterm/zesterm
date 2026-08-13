/**
 * This workspace's half of the register golden — the mirror of the Worker's
 * `cloud/packages/web/test/register-preimage.test.ts`.
 *
 * The two encoders are hand-built in two workspaces that cannot import each
 * other (three projects, three lockfiles), so the same fixture is asserted
 * byte-identically in both. The signature can be a fixture because Ed25519
 * signing is deterministic: the seed and the bytes decide all 64 bytes out.
 *
 * Golden provenance: seed `[7; 32]`, account `user-a`, label `this browser`.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { bytesToHex } from '@zesterm/proto';

import { generateIdentity, seedSigner } from '../src/identity.ts';
import { registerRequest, signRegistration } from '../src/register.ts';

/** The public key of seed `[7; 32]`, which is the signer's clientId. */
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

const SEED = '07'.repeat(32);

test('the register request is the exact bytes the Worker verifies', () => {
  assert.equal(
    bytesToHex(registerRequest(GOLDEN_ACCOUNT, GOLDEN_KEY, GOLDEN_LABEL)),
    GOLDEN_REQUEST,
    'the Worker rebuilds these bytes in its own workspace; a byte of difference refuses every real registration',
  );
});

test('signing through a ClientSigner produces the golden signature', async () => {
  // Through the signer rather than around it, because the wrap is the part
  // being pinned: the signer applies `zesterm-sig-v1\0client\0enrollment\0`
  // itself, and the Worker's verify assumes exactly that — a request wrapped
  // twice, or not at all, signs bytes the other side never checks.
  const identity = generateIdentity(SEED);
  assert.equal(identity.clientId, GOLDEN_KEY, 'the fixture seed must name the fixture key');
  const sig = await signRegistration(seedSigner(identity), GOLDEN_ACCOUNT, GOLDEN_LABEL);
  assert.equal(
    sig,
    GOLDEN_SIG,
    'Ed25519 is deterministic, so a byte of drift in the request or the wrap changes all of this',
  );
});

test('the boundary between account and label cannot be moved', () => {
  // The reason both fields are length-prefixed: concatenated, ("ab","cd") and
  // ("abc","d") are identical bytes, and the label is chosen by the caller.
  assert.notDeepEqual(
    registerRequest('ab', GOLDEN_KEY, 'cd'),
    registerRequest('abc', GOLDEN_KEY, 'd'),
    'field boundaries must be unambiguous',
  );
});

test('a malformed client id is refused before anything signs', () => {
  // `hexToBytes` on junk would hand noble a wrong-sized array to throw on
  // later; refusing here names the actual mistake. Uppercase is in the list
  // because the Worker's `fromHex` is lowercase-only and refuses the id on
  // shape — a signature built over an uppercase spelling would be rejected
  // every time with nothing naming the case as the cause.
  for (const bad of ['', 'ea4a', `${GOLDEN_KEY}ff`, GOLDEN_KEY.toUpperCase()]) {
    assert.throws(() => registerRequest(GOLDEN_ACCOUNT, bad, GOLDEN_LABEL), RangeError, bad);
  }
});

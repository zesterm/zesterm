/**
 * The handshake crypto, pinned to the Rust.
 *
 * The GOLDEN vectors below were printed by `zest-mesh` itself (a throwaway
 * example calling `auth_transcript`, `pairing_code` and
 * `ClientIdentity::sign` with the fixed inputs shown) — so these tests
 * compare implementations, not this package with itself. If any of them
 * fails, one side changed a byte layout, and the Rust's own warning applies:
 * changing it unpairs every device in the field.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import * as ed from '@noble/ed25519';
import { bytesToHex, hexToBytes } from '@zesterm/proto';

import {
  answerChallenge,
  authTranscript,
  ChallengeError,
  generateIdentity,
  isAbsentNonce,
  pairingCode,
  preimage,
  signAsClient,
  verifyClientSignature,
  type Transcript,
} from '../src/index.ts';

/** Inputs fed to the Rust when the goldens were printed. */
const GOLDEN_TRANSCRIPT: Transcript = {
  version: 2,
  host: '11'.repeat(32),
  client: '22'.repeat(32),
  hostNonce: '33'.repeat(32),
  clientNonce: '44'.repeat(32),
  hostLabel: 'andy-mac',
  // Astral on purpose: the label is length-prefixed in *bytes*, and an emoji
  // is where byte length and code-unit length disagree.
  clientLabel: 'web \u{1F600}',
};

const GOLDEN = {
  transcript:
    '7a65737465726d2d617574682d76310002' +
    '11'.repeat(32) +
    '22'.repeat(32) +
    '33'.repeat(32) +
    '44'.repeat(32) +
    '0008616e64792d6d6163' +
    '000877656220f09f9880',
  pairingCode: '896844',
  seed: '5e'.repeat(32),
  clientId: '8146640f02493af4fbc54fe33388e75dc2c937ae0b7727cc2b2afb1b75199a3e',
  signature:
    '3a26185999cac72b335411ef7a7c1bb51faab48ee56bb761425e7bcb5aca000d' +
    '2f27474bd91ba9ed03262b31caa608561a3c7f2d4b1c3203de6c4448706fce04',
};

test('the transcript layout matches the Rust byte for byte', () => {
  assert.equal(bytesToHex(authTranscript(GOLDEN_TRANSCRIPT)), GOLDEN.transcript);
});

test('the pairing code matches the Rust for the same transcript', () => {
  assert.equal(pairingCode(GOLDEN_TRANSCRIPT), GOLDEN.pairingCode);
});

test('a fixed seed derives the same client id the Rust derives', () => {
  const id = generateIdentity(GOLDEN.seed);
  assert.equal(id.clientId, GOLDEN.clientId, 'the id IS the public key on both sides');
});

test('signing produces the exact signature the Rust produces', () => {
  // Ed25519 is deterministic, so byte equality is meaningful — and it pins
  // the whole preimage (domain, role, purpose, separators) in one assert.
  const id = generateIdentity(GOLDEN.seed);
  const sig = signAsClient(id, 'auth', authTranscript(GOLDEN_TRANSCRIPT));
  assert.equal(sig, GOLDEN.signature);
});

test('the pairing code changes when either nonce changes', () => {
  // Its entire job: a relay runs two handshakes with two nonce pairs, so the
  // two screens show different codes.
  const other = pairingCode({ ...GOLDEN_TRANSCRIPT, clientNonce: '45'.repeat(32) });
  assert.notEqual(other, GOLDEN.pairingCode);
});

test('a pairing code keeps its leading zeroes', () => {
  // Mirrors the Rust's own property: a five-character code would make two
  // different codes read the same aloud.
  for (let i = 0; i < 64; i++) {
    const code = pairingCode({ ...GOLDEN_TRANSCRIPT, clientNonce: i.toString(16).padStart(64, '0') });
    assert.equal(code.length, 6, `code ${code} lost a digit`);
  }
});

test('roles do not cross: a client signature never verifies as a host one', () => {
  // The machine that is both host and client is the ordinary case, and this
  // is the property that keeps one role's proof from serving the other.
  const id = generateIdentity(GOLDEN.seed);
  const msg = authTranscript(GOLDEN_TRANSCRIPT);
  const sig = signAsClient(id, 'auth', msg);
  assert.ok(verifyClientSignature(id.clientId, 'auth', msg, sig));
  assert.notDeepEqual(
    preimage('client', 'auth', msg),
    preimage('host', 'auth', msg),
    'the role is part of the signed bytes',
  );
});

test('purposes do not cross either', () => {
  const id = generateIdentity(GOLDEN.seed);
  const msg = authTranscript(GOLDEN_TRANSCRIPT);
  const sig = signAsClient(id, 'auth', msg);
  assert.ok(
    !verifyClientSignature(id.clientId, 'enrollment', msg, sig),
    'an auth signature must not enrol a device',
  );
});

test('answerChallenge verifies the host before proving anything', () => {
  // Simulate the host: sign the transcript with the host role over a known key.
  const host = generateIdentity('7a'.repeat(32));
  const transcript: Transcript = { ...GOLDEN_TRANSCRIPT, host: host.clientId, client: GOLDEN.clientId };
  const bytes = authTranscript(transcript);
  // signAsClient uses the client role; build the host's signature manually via
  // the exported preimage + a second identity signing as 'host'.
  const hostSig = signHostForTest(host.seed, bytes);

  const me = generateIdentity(GOLDEN.seed);
  const answer = answerChallenge({
    identity: me,
    transcript,
    hostSignature: hostSig,
    expectedHost: host.clientId,
  });
  assert.ok(verifyClientSignature(me.clientId, 'auth', bytes, answer));

  assert.throws(
    () =>
      answerChallenge({
        identity: me,
        transcript,
        hostSignature: hostSig,
        expectedHost: 'ee'.repeat(32),
      }),
    ChallengeError,
    'a host that is not the one dialled must be refused before any proof is sent',
  );

  assert.throws(
    () =>
      answerChallenge({
        identity: me,
        transcript: { ...transcript, hostNonce: '00'.repeat(32) },
        hostSignature: hostSig,
      }),
    ChallengeError,
    'an absent nonce makes every signature a replay and must be refused',
  );

  assert.throws(
    () =>
      answerChallenge({
        identity: me,
        transcript,
        hostSignature: hostSig.replace(/^../, hostSig.startsWith('00') ? '01' : '00'),
      }),
    ChallengeError,
    'a corrupt host signature must not be answered',
  );
});

test('an all-zero nonce reads as absent', () => {
  assert.ok(isAbsentNonce('00'.repeat(32)));
  assert.ok(!isAbsentNonce('01' + '00'.repeat(31)));
});

/** The host role's signature, built from the same primitives the host uses. */
function signHostForTest(seedHex: string, message: Uint8Array): string {
  // Not exported from the package — a web client never signs as a host — so
  // the test reaches for the primitives directly.
  return bytesToHex(ed.sign(preimage('host', 'auth', message), hexToBytes(seedHex)));
}

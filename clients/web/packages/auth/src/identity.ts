/**
 * Client identity: an Ed25519 keypair whose public key *is* the `ClientId`,
 * exactly as ADR-006 has it — identities are public keys, no fingerprinting.
 *
 * Web identity in v1 is ephemeral or app-persisted (a seed the app may keep);
 * the non-extractable WebCrypto key with enrollment is M4's work and is
 * designed in `docs/design/phone/`. Nothing here writes storage.
 */

import * as ed from '@noble/ed25519';
import { sha512 } from '@noble/hashes/sha2.js';
import { bytesToHex, hexToBytes } from '@zesterm/proto';

import { preimage, type Purpose } from './preimage.ts';
import { authTranscript, isAbsentNonce, type Transcript } from './transcript.ts';

// noble-ed25519 v3 ships hashless; the sync API needs SHA-512 wired exactly
// once. Done at module load so no call path can race it.
ed.hashes.sha512 = sha512;

export interface ClientIdentity {
  /** 64 hex chars — the public key, which is the id. */
  readonly clientId: string;
  /** 64 hex chars. Kept only in memory unless the app decides otherwise. */
  readonly seed: string;
}

/** A fresh identity from the platform CSPRNG, or a deterministic one from a seed. */
export function generateIdentity(seedHex?: string): ClientIdentity {
  const seed = seedHex !== undefined ? hexToBytes(seedHex) : crypto.getRandomValues(new Uint8Array(32));
  if (seed.length !== 32) throw new Error(`a seed is 32 bytes, got ${seed.length}`);
  return {
    clientId: bytesToHex(ed.getPublicKey(seed)),
    seed: bytesToHex(seed),
  };
}

/** Sign as the client role. Returns 128 hex chars, ready for `ClientMessage.auth`. */
export function signAsClient(
  identity: ClientIdentity,
  purpose: Purpose,
  message: Uint8Array,
): string {
  return bytesToHex(ed.sign(preimage('client', purpose, message), hexToBytes(identity.seed)));
}

export class ChallengeError extends Error {
  override name = 'ChallengeError';
}

/**
 * Verify the host's half of the handshake and produce this client's answer.
 *
 * The host signs **first** so a client can pin the id an advertisement (or a
 * directory) claimed and hang up before proving anything about itself —
 * `expectedHost` is that pin. Refusing an absent nonce is what keeps a replay
 * from being free.
 *
 * Returns the `Auth` signature (hex) to send back.
 */
export function answerChallenge(args: {
  identity: ClientIdentity;
  transcript: Transcript;
  /** 128 hex chars — `Challenge.signature`. */
  hostSignature: string;
  /** Pin: 64 hex chars, or undefined when the address was given by hand. */
  expectedHost?: string;
}): string {
  const { identity, transcript, hostSignature, expectedHost } = args;

  if (expectedHost !== undefined && expectedHost !== transcript.host) {
    throw new ChallengeError(
      `dialled host ${expectedHost.slice(0, 8)}… but ${transcript.host.slice(0, 8)}… answered`,
    );
  }
  if (isAbsentNonce(transcript.hostNonce)) {
    throw new ChallengeError('the host sent no nonce; a signature over a constant is a replay');
  }
  if (transcript.client !== identity.clientId) {
    throw new ChallengeError('the transcript names a different client than the one signing');
  }

  const bytes = authTranscript(transcript);
  const ok = ed.verify(hexToBytes(hostSignature), preimage('host', 'auth', bytes), hexToBytes(transcript.host));
  if (!ok) {
    throw new ChallengeError('the host did not prove its identity');
  }
  return bytesToHex(ed.sign(preimage('client', 'auth', bytes), hexToBytes(identity.seed)));
}

/** Exposed for tests and diagnostics: verify a client-role signature. */
export function verifyClientSignature(
  clientId: string,
  purpose: Purpose,
  message: Uint8Array,
  signatureHex: string,
): boolean {
  return ed.verify(hexToBytes(signatureHex), preimage('client', purpose, message), hexToBytes(clientId));
}

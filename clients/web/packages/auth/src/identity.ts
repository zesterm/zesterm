/**
 * Client identity: an Ed25519 keypair whose public key *is* the `ClientId`,
 * exactly as ADR-006 has it — identities are public keys, no fingerprinting.
 *
 * A seed is one of two things a client can sign with — the other is a
 * non-extractable `CryptoKey`, which cannot be reduced to a seed and so signs
 * behind the [`ClientSigner`] seam in `signer.ts`. Everything in this file
 * that *verifies* stays on `@noble` and stays synchronous; see that file for
 * why the split runs exactly there. Nothing here writes storage.
 */

import * as ed from '@noble/ed25519';
import { sha512 } from '@noble/hashes/sha2.js';
import { bytesToHex, hexToBytes } from '@zesterm/proto';

import { preimage, type Purpose } from './preimage.ts';
import type { ClientSigner } from './signer.ts';
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

/**
 * Sign with a seed this process holds in memory.
 *
 * Async only to satisfy the seam — the work is synchronous and the promise is
 * already resolved. A signer rather than an identity is what every caller
 * above takes, so the two key kinds are indistinguishable to the handshake.
 */
export function seedSigner(identity: ClientIdentity): ClientSigner {
  return {
    clientId: identity.clientId,
    sign: (purpose, message) => Promise.resolve(signAsClient(identity, purpose, message)),
  };
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
 * **Every refusal happens before the first `await`.** An `async` function runs
 * synchronously up to it, so the pin, the nonce check and the host's proof are
 * all settled in the caller's own tick and `signer.sign` is reached only by a
 * host that proved itself. Move any of those checks after the await and the
 * ordering this whole handshake rests on is gone, silently — every existing
 * test would still pass, because the answer would still be correct whenever
 * the host is honest.
 *
 * Returns the `Auth` signature (hex) to send back.
 */
export async function answerChallenge(args: {
  signer: ClientSigner;
  transcript: Transcript;
  /** 128 hex chars — `Challenge.signature`. */
  hostSignature: string;
  /** Pin: 64 hex chars, or undefined when the address was given by hand. */
  expectedHost?: string;
}): Promise<string> {
  const { signer, transcript, hostSignature, expectedHost } = args;

  if (expectedHost !== undefined && expectedHost !== transcript.host) {
    throw new ChallengeError(
      `dialled host ${expectedHost.slice(0, 8)}… but ${transcript.host.slice(0, 8)}… answered`,
    );
  }
  if (isAbsentNonce(transcript.hostNonce)) {
    throw new ChallengeError('the host sent no nonce; a signature over a constant is a replay');
  }
  if (transcript.client !== signer.clientId) {
    throw new ChallengeError('the transcript names a different client than the one signing');
  }

  const bytes = authTranscript(transcript);
  const ok = ed.verify(hexToBytes(hostSignature), preimage('host', 'auth', bytes), hexToBytes(transcript.host));
  if (!ok) {
    throw new ChallengeError('the host did not prove its identity');
  }
  return signer.sign('auth', bytes);
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

/**
 * The sealed channel — a port of `zest-mesh/src/secure.rs`.
 *
 * Everything after the `Challenge` is encrypted, in both directions, which is
 * what lets a relay carry the bytes without being trusted with them. The keys
 * come out of the transcript both sides already sign, so no new signature
 * scheme and no stored key were needed to get here.
 *
 * # Held to a golden, deliberately
 *
 * Two implementations can agree on every field of the transcript, produce
 * signatures each other verifies, and still derive different keys — one
 * appended the DH publics in the other order, or hashed the transcript
 * including the domain, or wrote `host->client` where the other wrote
 * `host→client`. Each presents identically: the handshake completes and the
 * first real frame does not open. So this file is checked against
 * `crates/zest-proto/fixtures/handshake.json`, which carries every
 * intermediate value and not merely the answer.
 *
 * # ChaCha20-Poly1305, not AES-GCM
 *
 * `crypto.subtle` is async. This client decodes and applies on the main thread
 * synchronously, by measurement — AES-GCM through WebCrypto would make the
 * whole decode path async for no benefit. `@noble/ciphers`' ChaCha is
 * synchronous.
 */

import { chacha20poly1305 } from '@noble/ciphers/chacha.js';
import { x25519 } from '@noble/curves/ed25519.js';
import { expand, extract } from '@noble/hashes/hkdf.js';
import { sha256 } from '@noble/hashes/sha2.js';
import { bytesToHex, hexToBytes } from '@zesterm/proto';

import { authTranscript, type Transcript } from './transcript.ts';

const TEXT = new TextEncoder();

/** Poly1305's tag, and the reason a sealed frame is 16 bytes longer. */
export const TAG_LEN = 16;

/**
 * Records per epoch, then the key ratchets and the counter resets.
 *
 * `2 ** 24`, matching the Rust. Far below what a nonce could take, and chosen
 * so the rekey path is reachable in a test at all — an unreachable branch in
 * two independent implementations is one that has never been compared.
 */
export const REKEY_AFTER = 2 ** 24;

/** Which end of the handshake this is. Decides which key sends. */
export type Role = 'host' | 'client';

/**
 * One direction: its key, its counter, its epoch.
 *
 * The two directions never share a nonce space, which is what makes a record
 * impossible to reflect back down the other side.
 */
class Direction {
  private key: Uint8Array;
  private epoch = 0;
  private counter = 0;

  constructor(key: Uint8Array) {
    this.key = key;
  }

  /** `epoch: u32 BE || counter: u64 BE`, twelve bytes. */
  private nonce(): Uint8Array {
    const n = new Uint8Array(12);
    const view = new DataView(n.buffer);
    view.setUint32(0, this.epoch, false);
    view.setBigUint64(4, BigInt(this.counter), false);
    return n;
  }

  seal(plaintext: Uint8Array): Uint8Array {
    const out = chacha20poly1305(this.key, this.nonce()).encrypt(plaintext);
    this.advance();
    return out;
  }

  open(ciphertext: Uint8Array): Uint8Array {
    // Decrypt first: the counter must **not** advance on a failure. A record
    // that did not open was not a record, and skipping past it desynchronises
    // the two sides in a way that surfaces as corruption several frames later.
    const out = chacha20poly1305(this.key, this.nonce()).decrypt(ciphertext);
    this.advance();
    return out;
  }

  /**
   * Advance, ratcheting when the counter wraps.
   *
   * **Deterministic, and never announced on the wire.** Both sides count the
   * same records in the same order, so both turn the ratchet at the same
   * point — and a rekey *message* would be one more thing two independent
   * implementations could disagree about, at the exact moment a disagreement
   * is undetectable until the next record fails to open.
   */
  private advance(): void {
    this.counter += 1;
    if (this.counter < REKEY_AFTER) return;
    // HKDF-**Expand** alone: the current key already is the PRK. Full HKDF
    // with an empty salt is a different function and yields a different key,
    // and since this branch is 2 ** 24 records away, taking the wrong one is
    // invisible until hours into a session. `@noble/hashes/hkdf` exports both.
    this.key = expand(sha256, this.key, TEXT.encode('zesterm-rekey-v1'), 32);
    this.counter = 0;
    this.epoch += 1;
  }

  counters(): { epoch: number; counter: number } {
    return { epoch: this.epoch, counter: this.counter };
  }

  /**
   * Jump to a point in the schedule. **For the golden fixture only.**
   *
   * The alternative is running the ratchet the honest way, which means 2 ** 24
   * ChaCha operations to reach the one interesting record — minutes in a test
   * suite that otherwise takes a second. The Rust's own rekey test does the
   * same thing by assigning the private field directly.
   *
   * Ratchets `epoch` times rather than jumping the key, so this still exercises
   * the derivation it is skipping past.
   */
  seekForFixture(epoch: number, counter: number): void {
    for (let e = 0; e < epoch; e += 1) {
      this.key = expand(sha256, this.key, TEXT.encode('zesterm-rekey-v1'), 32);
    }
    this.epoch = epoch;
    this.counter = counter;
  }
}

/** Both directions of one connection. */
export class SecureChannel {
  // Explicit fields, not constructor parameter properties: Node runs these
  // files by *stripping* types, and a parameter property is not a type -- it
  // emits an assignment. `node --test` refuses the file outright.
  private readonly sendDir: Direction;
  private readonly recvDir: Direction;

  constructor(sendDir: Direction, recvDir: Direction) {
    this.sendDir = sendDir;
    this.recvDir = recvDir;
  }

  seal(plaintext: Uint8Array): Uint8Array {
    return this.sendDir.seal(plaintext);
  }

  open(ciphertext: Uint8Array): Uint8Array {
    return this.recvDir.open(ciphertext);
  }

  /** For the golden's sake; never needed on a live link. */
  counters(): { send: { epoch: number; counter: number }; recv: { epoch: number; counter: number } } {
    return { send: this.sendDir.counters(), recv: this.recvDir.counters() };
  }

  /** See [`Direction.seekForFixture`]. Not for a live link. */
  seekRecvForFixture(epoch: number, counter: number): void {
    this.recvDir.seekForFixture(epoch, counter);
  }
}

/** An ephemeral X25519 key, for one connection and no other. */
export interface Ephemeral {
  /** 64 hex chars, for the `Hello` or the `Challenge`. */
  readonly publicKey: string;
  /**
   * Agree on a channel. Consumes nothing the type system can enforce, so call
   * it once — a second channel would restart the counters under the same key.
   */
  agree(theirDh: string, transcript: Transcript, role: Role): SecureChannel;
}

export function generateEphemeral(): Ephemeral {
  return ephemeralFromSeed(crypto.getRandomValues(new Uint8Array(32)));
}

/** For the golden fixture, which needs the same keys on every run. */
export function ephemeralFromSeed(seed: Uint8Array): Ephemeral {
  const secret = seed.slice();
  return {
    publicKey: bytesToHex(x25519.getPublicKey(secret)),
    agree(theirDh: string, transcript: Transcript, role: Role): SecureChannel {
      const theirs = hexToBytes(theirDh);
      if (theirs.length !== 32) {
        throw new Error(`a DH key is 32 bytes, got ${theirs.length}`);
      }
      // All-zero is what `#[serde(default)]` produces for a peer that sent
      // none. Agreeing with a peer that contributed nothing would look like
      // success on both sides, which is the worst possible failure mode.
      if (theirs.every((b) => b === 0)) {
        throw new Error('peer sent no ephemeral key; refusing to derive a channel');
      }

      const shared = x25519.getSharedSecret(secret, theirs);
      // A low-order peer key drives the output to zero, so both sides "agree"
      // on a constant an attacker chose. noble does not reject it for us.
      if (shared.every((b) => b === 0)) {
        throw new Error("peer's ephemeral key is non-contributory");
      }

      // Salt is the hash of the signed bytes, so the keys are bound to *this*
      // handshake: the ids, the nonces, the labels, the version and both DH
      // publics. Change any of them and the two sides derive different keys.
      const th = sha256(authTranscript(transcript));
      const prk = extract(sha256, shared, th);
      const h2c = expand(sha256, prk, TEXT.encode('zesterm-e2e-v1 host->client'), 32);
      const c2h = expand(sha256, prk, TEXT.encode('zesterm-e2e-v1 client->host'), 32);

      const [send, recv] = role === 'host' ? [h2c, c2h] : [c2h, h2c];
      return new SecureChannel(new Direction(send), new Direction(recv));
    },
  };
}

/**
 * The largest plaintext that still fits a frame once sealed.
 *
 * Exported because the check belongs at the *sealer*, not at the framer: the
 * `u32` prefix describes the ciphertext, which is `TAG_LEN` longer, so a
 * sealer that bounds the plaintext instead passes every small test and fails
 * only on a maximal keyframe — that is, only on very large grids.
 */
export function maxPlaintext(maxFrame: number): number {
  return maxFrame - TAG_LEN;
}

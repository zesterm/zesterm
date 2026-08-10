/**
 * The device key a script on this origin cannot steal.
 *
 * A separate entry point (`@zesterm/auth/webcrypto`) rather than part of the
 * package's main one, because `CryptoKey` and `CryptoKeyPair` are types only
 * under `lib.dom` — `@types/node` exposes `CryptoKey` as a value alone — and
 * the packages above this one compile without the DOM on purpose:
 * `@zesterm/client` says platform-blind in its own module doc, and the
 * sidecar is a Node process. They consume `ClientSigner`; only the browser
 * app constructs one from a `CryptoKey`.
 *
 * Everything here is the *crypto*. Where the key is kept is the app's
 * business (`packages/app/src/device-key.ts`) — this package has never
 * written storage and does not start now.
 */

import { bytesToHex } from '@zesterm/proto';

import { preimage, type Purpose } from './preimage.ts';
import type { ClientSigner } from './signer.ts';

/** The algorithm name both `generateKey` and `sign` take; a bare string, not an object. */
const ED25519 = 'Ed25519';

/**
 * Sign with a `CryptoKey` — a device key the page can use but not export.
 *
 * `clientId` is trusted, not checked: nothing here can derive a public key
 * from a non-extractable private one, so a caller that pairs the wrong id
 * with the wrong key produces signatures the host rejects with
 * `AuthFailure::BadSignature`. Keep the two together at their storage layer,
 * which is the only place that ever saw both.
 */
export function webCryptoSigner(clientId: string, key: CryptoKey): ClientSigner {
  if (!/^[0-9a-f]{64}$/.test(clientId)) {
    throw new Error(`a client id is 64 lowercase hex chars, got ${clientId.length}`);
  }
  if (key.type !== 'private') {
    // The public half of a generated pair *is* extractable and structured-
    // cloneable, so storing the wrong one of the two is an easy mistake that
    // would otherwise surface as an opaque `InvalidAccessError` per handshake.
    throw new Error(`a signer needs the private key, got a ${key.type} key`);
  }
  return {
    clientId,
    async sign(purpose: Purpose, message: Uint8Array): Promise<string> {
      const sig = await crypto.subtle.sign(ED25519, key, preimage('client', purpose, message));
      return bytesToHex(new Uint8Array(sig));
    },
  };
}

/**
 * A fresh non-extractable device key, and the id that names it.
 *
 * `extractable` is `false`, which applies to the private half; the public
 * half of an asymmetric pair is always extractable regardless, and that is
 * what lets the id be read out here once and stored beside the key.
 */
export async function generateWebCryptoKey(): Promise<{
  readonly clientId: string;
  readonly keyPair: CryptoKeyPair;
}> {
  const keyPair = await crypto.subtle.generateKey(ED25519, false, ['sign', 'verify']);
  return { clientId: await clientIdOf(keyPair.publicKey), keyPair };
}

/** The `ClientId` a WebCrypto public key names: its 32 raw bytes, as hex. */
export async function clientIdOf(publicKey: CryptoKey): Promise<string> {
  const raw = await crypto.subtle.exportKey('raw', publicKey);
  return bytesToHex(new Uint8Array(raw));
}

/** Probed once per page; the answer cannot change under a running browser. */
let probe: Promise<boolean> | null = null;

/**
 * Can this browser actually sign Ed25519 through `crypto.subtle`?
 *
 * **It generates a key and signs with it**, rather than asking whether the
 * name is recognised. A device that cannot sign at all is the failure mode of
 * a weaker probe: the algorithm has shipped, been withdrawn behind a flag and
 * shipped again across the engines, and `generateKey` succeeding has not
 * always meant `sign` would. The cost is one throwaway keypair at startup, on
 * a path that already awaits a network round trip.
 *
 * Non-extractable in the probe too, because that is the mode being asked
 * about — an implementation that only supports exportable keys must answer
 * "no" here rather than at the first handshake.
 */
export function webCryptoEd25519Available(): Promise<boolean> {
  probe ??= (async () => {
    try {
      const pair = await crypto.subtle.generateKey(ED25519, false, ['sign', 'verify']);
      const sig = await crypto.subtle.sign(ED25519, pair.privateKey, Uint8Array.of(0));
      return sig.byteLength === 64;
    } catch {
      return false;
    }
  })();
  return probe;
}

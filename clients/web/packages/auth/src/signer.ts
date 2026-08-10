/**
 * Who signs — the seam between a key this process can read and one it cannot.
 *
 * **`@noble/ed25519` cannot sign with a non-extractable `CryptoKey`**, and no
 * amount of wrapping changes that: it needs the raw 32-byte scalar, and
 * refusing to yield one is the entire point of the flag. A device key that a
 * script on the origin cannot steal therefore has to sign through
 * `crypto.subtle` — which is asynchronous, which is why `sign` returns a
 * promise even for the seed implementation in `identity.ts`, where it
 * resolves immediately.
 *
 * **Verification deliberately does not live behind this seam.** It only ever
 * touches public keys, so it stays synchronous on `@noble` — and that is what
 * preserves the ordering the handshake's security rests on: the host is
 * proven *before* the client signs anything at all. Route verification
 * through an async subtle call and the two steps become interleavable, which
 * is precisely the property nobody would notice losing.
 *
 * The interface lives alone in this file, with no `CryptoKey` in sight, so
 * that the platform-blind packages above (`@zesterm/client`, the sidecar) can
 * compile it under `lib: ES2023` alone. The implementation that needs the DOM
 * types is a separate entry point, `@zesterm/auth/webcrypto`.
 */

import type { Purpose } from './preimage.ts';

/**
 * Something that can produce this client's signatures.
 *
 * The id is carried alongside rather than derived on demand: a
 * non-extractable private key cannot be asked for its public half, so the id
 * comes from whoever stored the pair.
 */
export interface ClientSigner {
  /** 64 hex chars — the public key, which is the id. */
  readonly clientId: string;
  /** 128 hex chars, ready for `ClientMessage.auth`. */
  sign(purpose: Purpose, message: Uint8Array): Promise<string>;
}

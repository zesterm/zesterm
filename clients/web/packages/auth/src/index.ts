/**
 * The Ed25519 handshake, portable to every zesterm client.
 *
 * Byte-for-byte ports of `zest-mesh`'s `preimage` and `auth_transcript`,
 * pinned by vectors printed from the Rust itself. The daemon does not care
 * which library verified it — which is the entire reason the Rust chose a
 * domain *prefix* over Ed25519ctx.
 */

export { preimage, type Role, type Purpose } from './preimage.ts';
export {
  authTranscript,
  pairingCode,
  isAbsentNonce,
  PAIRING_CODE_DIGITS,
  type Transcript,
} from './transcript.ts';
export {
  generateIdentity,
  signAsClient,
  seedSigner,
  answerChallenge,
  verifyClientSignature,
  ChallengeError,
  type ClientIdentity,
} from './identity.ts';
export { type ClientSigner } from './signer.ts';
// The WebCrypto implementation is `@zesterm/auth/webcrypto`, deliberately not
// re-exported here: it needs the DOM's `CryptoKey` type, and the packages
// that consume this entry point compile without the DOM.

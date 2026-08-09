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
  answerChallenge,
  verifyClientSignature,
  ChallengeError,
  type ClientIdentity,
} from './identity.ts';

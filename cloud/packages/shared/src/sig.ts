/**
 * The exact bytes an Ed25519 key is asked to sign — a byte-for-byte port of
 * `preimage` in `crates/zest-mesh/src/identity.rs`.
 *
 * ```
 * preimage = "zesterm-sig-v1" \0 role \0 purpose \0 message
 * ```
 *
 * Here rather than in the one Worker that uses it, because there are two. The
 * web Worker verifies an enrolment request and signs an attach ticket; the
 * relay Worker verifies that ticket (ADR-009), and the two share no code path
 * at runtime — they are separate `wrangler deploy`s on separate cadences.
 *
 * The move was pre-emptive on purpose, and this is what it bought. A signed
 * byte layout with two copies is one that can disagree, and the disagreement
 * surfaces at bring-up as a signature mismatch that names neither side. Copying
 * it once is how that starts, so there was one copy before there was a second
 * caller rather than after.
 *
 * Bytes only: no Ed25519 here, and no dependency that brings it. Verifying is
 * the caller's, in the package that already carries `@noble/ed25519`; this
 * package's dependency-free property is what lets everything in it be covered
 * by `node --test` rather than only through a deployed Worker.
 *
 * NUL separators rather than length prefixes, and the Rust says why: `role` and
 * `purpose` come from closed unions with fixed NUL-free ASCII names, so no
 * shorter field can be extended to look like a longer one, and the only
 * caller-influenced field is last. Add a caller-supplied string to the domain
 * and this must become length-prefixed — `packages/web/src/enroll/preimage.ts` is what
 * that looks like, one layer up.
 */

import { utf8, concat } from './bytes.ts';

const SIGNING_DOMAIN = 'zesterm-sig-v1';

/**
 * Which key produced a signature. Inside the signing domain, so a host's proof
 * cannot be replayed as a client's — which matters because one machine is
 * routinely both. `zest_mesh::identity::Role`, plus one variant it does not
 * have.
 *
 * **`relay` has no Rust counterpart, and must not grow one.** An attach ticket
 * is signed by the account service and by nothing else, so no key in the fleet
 * — not a host's, not a client's — can produce bytes that verify as one.
 * Adding the variant to `zest_mesh::identity::Role` would hand every daemon
 * the ability to mint its own admission to the relay, which is the one thing
 * `ticket.ts` exists to prevent.
 */
export type Role = 'host' | 'client' | 'relay';

/**
 * What a signature is *for*. A closed union rather than a string, because a
 * caller who can pass any string can pass the wrong one, and the failure is a
 * signature that is valid in a context nobody intended.
 * `zest_mesh::identity::Purpose`, spelled with its `as_domain` names.
 */
export type Purpose = 'auth' | 'enrollment' | 'attach-ticket' | 'device-attestation';

/** `zest_mesh::identity::preimage`, verbatim. */
export function signingPreimage(role: Role, purpose: Purpose, message: Uint8Array): Uint8Array {
  return concat([utf8(`${SIGNING_DOMAIN}\0${role}\0${purpose}\0`), message]);
}

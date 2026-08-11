/**
 * The exact bytes an Ed25519 key is asked to sign — a byte-for-byte port of
 * `preimage` in `crates/zest-mesh/src/identity.rs`.
 *
 * ```
 * preimage = "zesterm-sig-v1" \0 role \0 purpose \0 message
 * ```
 *
 * Here rather than in one Worker because two Workers now verify signatures a
 * daemon made — the web Worker over an enrolment request, the relay over an
 * attach challenge — and a signed byte layout with two copies is one that can
 * disagree. The disagreement surfaces at bring-up as a signature mismatch that
 * names neither side.
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
 * and this must become length-prefixed — `web/src/enroll/preimage.ts` is what
 * that looks like, one layer up.
 */

import { utf8, concat } from './bytes.ts';

const SIGNING_DOMAIN = 'zesterm-sig-v1';

/**
 * Which key produced a signature. Inside the signing domain, so a host's proof
 * cannot be replayed as a client's — which matters because one machine is
 * routinely both. `zest_mesh::identity::Role`.
 */
export type Role = 'host' | 'client';

/**
 * What a signature is *for*. A closed union rather than a string, because a
 * caller who can pass any string can pass the wrong one, and the failure is a
 * signature that is valid in a context nobody intended.
 * `zest_mesh::identity::Purpose`, spelled with its `as_domain` names.
 */
export type Purpose = 'auth' | 'enrollment' | 'attach-ticket';

/** `zest_mesh::identity::preimage`, verbatim. */
export function signingPreimage(role: Role, purpose: Purpose, message: Uint8Array): Uint8Array {
  return concat([utf8(`${SIGNING_DOMAIN}\0${role}\0${purpose}\0`), message]);
}

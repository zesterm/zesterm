/**
 * The two messages of the browser hand-off sign-in — a byte-for-byte port of
 * `crates/zest-mesh/src/link.rs`, pinned to it by
 * `crates/zest-proto/fixtures/link.json`.
 *
 * ```
 * request  = "zesterm-link-v1" ++ 0x01 ++ key32 ++ u16be(len(label)) ++ label
 * claim    = "zesterm-link-v1" ++ 0x02 ++ u16be(len(grant)) ++ grant ++ key32
 * preimage = "zesterm-sig-v1" \0 "client" \0 "enrollment" \0 message
 * ```
 *
 * No new `Purpose`, the register-preimage argument once more: the outer
 * domains diverge from `zesterm-enroll-v1` and `zesterm-register-v1` at byte
 * 8, before any caller-supplied field, so no message under one can be bytes
 * under another.
 *
 * **The tag byte is load-bearing.** The two link messages share one domain
 * and each carries the same 32-byte key plus one length-prefixed
 * caller-influenced string — in opposite orders. Order alone makes them only
 * probabilistically disjoint (a key whose leading bytes read as a plausible
 * length prefix overlaps the other layout), and the property being bought is
 * that an approval-phase signature can never be spent as a claim — the
 * fixture carries that exact replay as a MUST-fail case.
 */

import { concat, signingPreimage, utf8 } from '@zesterm/cloud-shared';
import { verifyAsync } from '@noble/ed25519';

import { KEY_LEN, SIGNATURE_LEN, pushLenPrefixed } from './preimage.ts';

const LINK_DOMAIN = 'zesterm-link-v1';

const REQUEST_TAG = 1;
const CLAIM_TAG = 2;

/** `zest_mesh::link::link_request`, verbatim. */
export function linkRequest(key: Uint8Array, label: string): Uint8Array {
  if (key.length !== KEY_LEN) {
    throw new RangeError(`a key is ${KEY_LEN} bytes, this one is ${key.length}`);
  }
  const parts: Uint8Array[] = [utf8(LINK_DOMAIN), Uint8Array.of(REQUEST_TAG), key];
  pushLenPrefixed(parts, label);
  return concat(parts);
}

/** `zest_mesh::link::link_claim`, verbatim. */
export function linkClaim(grant: string, key: Uint8Array): Uint8Array {
  if (key.length !== KEY_LEN) {
    throw new RangeError(`a key is ${KEY_LEN} bytes, this one is ${key.length}`);
  }
  const parts: Uint8Array[] = [utf8(LINK_DOMAIN), Uint8Array.of(CLAIM_TAG)];
  pushLenPrefixed(parts, grant);
  parts.push(key);
  return concat(parts);
}

/**
 * Did the holder of `key` sign this exact link request?
 *
 * Answers that and nothing else — whether a grant should be minted is the
 * route's business, kept out of here where a missing check would look like a
 * passing one. `zip215: false` mirrors every verify on this Worker: the app
 * side of this flow is dalek's `verify_strict` world, and a small-order key
 * must not be able to park grants.
 *
 * Never throws: a caller reasoning about "did this verify" must not also
 * reason about an exception. A rejection is a `false`.
 */
export async function verifyLinkRequest(args: {
  key: Uint8Array;
  label: string;
  signature: Uint8Array;
}): Promise<boolean> {
  const { key, label, signature } = args;
  if (key.length !== KEY_LEN || signature.length !== SIGNATURE_LEN) return false;
  try {
    return await verifyAsync(signature, signingPreimage('client', 'enrollment', linkRequest(key, label)), key, {
      zip215: false,
    });
  } catch {
    return false;
  }
}

/** Did the holder of `key` sign this exact claim of this exact grant? */
export async function verifyLinkClaim(args: {
  grant: string;
  key: Uint8Array;
  signature: Uint8Array;
}): Promise<boolean> {
  const { grant, key, signature } = args;
  if (key.length !== KEY_LEN || signature.length !== SIGNATURE_LEN) return false;
  try {
    return await verifyAsync(signature, signingPreimage('client', 'enrollment', linkClaim(grant, key)), key, {
      zip215: false,
    });
  } catch {
    return false;
  }
}

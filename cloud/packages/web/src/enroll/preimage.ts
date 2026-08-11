/**
 * The exact bytes a machine signs to claim an enrolment code — a byte-for-byte
 * port of `crates/zest-mesh/src/enroll.rs` and the signing prefix in
 * `crates/zest-mesh/src/identity.rs`.
 *
 * ```
 * request  = "zesterm-enroll-v1" ++ u16be(len(code)) ++ code
 *                                ++ key32
 *                                ++ u16be(len(label)) ++ label
 * preimage = "zesterm-sig-v1" \0 role \0 "enrollment" \0 request
 * ```
 *
 * **Length-prefixed, not NUL-separated**, and the Rust says why: `identity`'s
 * NUL form is sound only because its variable field is last and the others come
 * from closed enums. That reasoning does not survive two caller-supplied
 * strings — concatenated, `("ab","cd")` and `("abc","d")` are identical bytes,
 * so a signature over one would verify the other, and the label is
 * attacker-chosen. So only this outer layer lives here; the signing domain
 * itself is `@zesterm/cloud-shared`'s, so the relay Worker can build the same
 * bytes without a second copy of them when it comes to verify its own
 * challenge. Nothing in the relay imports it yet.
 *
 * Two implementations of one format is the hazard here: the daemon signs in
 * Rust and this verifies in TypeScript, and a disagreement surfaces as a
 * signature mismatch that names nothing. `test/enroll-preimage.test.ts` pins
 * them together with the Rust's own golden hex and a signature the Rust
 * actually produced.
 */

import { concat, signingPreimage, utf8, type Role } from '@zesterm/cloud-shared';
import { verifyAsync } from '@noble/ed25519';

const ENROLL_DOMAIN = 'zesterm-enroll-v1';

export type { Role };

/** The public key being enrolled, raw. Ed25519, so always 32 bytes. */
export const KEY_LEN = 32;

/** An Ed25519 signature. Always 64 bytes; a short one is an error, never a pad. */
export const SIGNATURE_LEN = 64;

function pushLenPrefixed(parts: Uint8Array[], text: string): void {
  const bytes = utf8(text);
  if (bytes.length > 0xffff) {
    // The Rust clamps to `u16::MAX` and truncates. Deliberately not ported:
    // the callers here bound both fields three orders of magnitude below this,
    // so reaching it means something upstream stopped validating — and a
    // preimage that silently drops bytes is a signature over a message nobody
    // meant to send.
    throw new RangeError(`an enrolment field is at most 65535 bytes, this one is ${bytes.length}`);
  }
  parts.push(new Uint8Array([bytes.length >>> 8, bytes.length & 0xff]), bytes);
}

/** `zest_mesh::enroll::enrollment_request`, verbatim. */
export function enrollmentRequest(code: string, key: Uint8Array, label: string): Uint8Array {
  if (key.length !== KEY_LEN) {
    throw new RangeError(`a key is ${KEY_LEN} bytes, this one is ${key.length}`);
  }
  const parts: Uint8Array[] = [utf8(ENROLL_DOMAIN)];
  pushLenPrefixed(parts, code);
  parts.push(key);
  pushLenPrefixed(parts, label);
  return concat(parts);
}

/** The enrolment request under `Purpose::Enrollment`'s signing domain. */
export function enrollmentPreimage(
  role: Role,
  code: string,
  key: Uint8Array,
  label: string,
): Uint8Array {
  return signingPreimage(role, 'enrollment', enrollmentRequest(code, key, label));
}

/**
 * Did the holder of `key` sign this exact enrolment?
 *
 * Answers that and nothing else — whether the code is fresh, unspent and
 * belongs to the account asking is the caller's business, kept out of here
 * where a missing check would look like a passing one. The Rust's
 * `verify_enrollment` draws the line in the same place and for the same reason.
 *
 * `zip215: false` is not a detail. Noble's default is ZIP-215 semantics, which
 * accept non-canonical encodings and small-order public keys; the Rust verifies
 * with dalek's `verify_strict`, which does not. Left at the default, this
 * side would accept enrolments the daemon's own half of the fleet would then
 * refuse — and a small-order key is one that verifies almost anything, so it
 * must not be able to enter the registry at all.
 *
 * Never throws: noble raises on a malformed point or a wrong-sized input, and a
 * caller reasoning about "did this verify" must not have to also reason about
 * an exception. A rejection is a `false`.
 */
export async function verifyEnrollment(args: {
  role: Role;
  code: string;
  key: Uint8Array;
  label: string;
  signature: Uint8Array;
}): Promise<boolean> {
  const { role, code, key, label, signature } = args;
  if (key.length !== KEY_LEN || signature.length !== SIGNATURE_LEN) return false;
  try {
    return await verifyAsync(signature, enrollmentPreimage(role, code, key, label), key, {
      zip215: false,
    });
  } catch {
    return false;
  }
}

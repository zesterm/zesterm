/**
 * The exact bytes a browser signs to register its device key — TypeScript on
 * both sides, with no Rust counterpart and none planned: registration is an
 * act by a signed-in *browser*, and daemons enrol with codes.
 *
 * ```
 * request  = "zesterm-register-v1" ++ u16be(len(account)) ++ account
 *                                  ++ key32
 *                                  ++ u16be(len(label)) ++ label
 * preimage = "zesterm-sig-v1" \0 "client" \0 "enrollment" \0 request
 * ```
 *
 * `account` is the session user's `users.id`, and binding it into the signed
 * bytes is the point of the format: a registration captured off one account's
 * wire replays under another account's session as a signature over the wrong
 * `account`, and verifies nothing.
 *
 * **No new `Purpose`, on purpose.** Both this and the enrolment claim sign
 * under `enrollment`, and that is sound because the outer domains do the
 * separating: `zesterm-enroll-v1` and `zesterm-register-v1` diverge at byte 8
 * (`e` vs `r`), so no message under one domain can be bytes under the other —
 * a prefix that differs before any caller-supplied field is reached cannot be
 * extended into a collision. Widening the closed `Purpose` union would touch
 * `@zesterm/cloud-shared` and the Rust for a distinction the domain already
 * makes.
 *
 * **Length-prefixed, not NUL-separated**, for the reason `preimage.ts` gives:
 * two caller-supplied strings concatenated make `("ab","cd")` and `("abc","d")`
 * the same bytes, and the label is chosen by whoever is registering.
 *
 * The signing side lives in `clients/web/packages/auth/src/register.ts` — that
 * workspace cannot import this one (three projects, three lockfiles; see
 * `cloud/README.md`), so the two hand-built encoders are pinned byte-identical
 * by a shared golden vector in both workspaces' tests
 * (`test/register-preimage.test.ts` here, `test/register.test.ts` there).
 */

import { concat, signingPreimage, utf8 } from '@zesterm/cloud-shared';
import { verifyAsync } from '@noble/ed25519';

import { KEY_LEN, SIGNATURE_LEN, pushLenPrefixed } from './preimage.ts';

const REGISTER_DOMAIN = 'zesterm-register-v1';

/** The request a browser signs: `(account, key, label)` under the register domain. */
export function registerRequest(account: string, key: Uint8Array, label: string): Uint8Array {
  if (key.length !== KEY_LEN) {
    throw new RangeError(`a key is ${KEY_LEN} bytes, this one is ${key.length}`);
  }
  const parts: Uint8Array[] = [utf8(REGISTER_DOMAIN)];
  pushLenPrefixed(parts, account);
  parts.push(key);
  pushLenPrefixed(parts, label);
  return concat(parts);
}

/**
 * The registration request under the client role's `enrollment` signing
 * domain — exactly what `ClientSigner.sign('enrollment', request)` produces on
 * the browser side.
 */
export function registerPreimage(account: string, key: Uint8Array, label: string): Uint8Array {
  return signingPreimage('client', 'enrollment', registerRequest(account, key, label));
}

/**
 * Did the holder of `key` sign this exact registration, for this account?
 *
 * Answers that and nothing else — whether the key is already enrolled, revoked
 * or someone else's is the caller's business, kept out of here where a missing
 * check would look like a passing one. `verifyEnrollment` draws the line in
 * the same place.
 *
 * `zip215: false` mirrors `verifyEnrollment`, and for the same reason: the
 * fleet's Rust half verifies with dalek's `verify_strict`, and a small-order
 * key — one that verifies almost anything — must not be able to enter the
 * registry through this door either.
 *
 * Never throws: noble raises on a malformed point or a wrong-sized input, and
 * a caller reasoning about "did this verify" must not also reason about an
 * exception. A rejection is a `false`.
 */
export async function verifyRegistration(args: {
  account: string;
  key: Uint8Array;
  label: string;
  signature: Uint8Array;
}): Promise<boolean> {
  const { account, key, label, signature } = args;
  if (key.length !== KEY_LEN || signature.length !== SIGNATURE_LEN) return false;
  try {
    return await verifyAsync(signature, registerPreimage(account, key, label), key, {
      zip215: false,
    });
  } catch {
    return false;
  }
}

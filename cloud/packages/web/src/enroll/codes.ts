/**
 * The one-shot code a person carries from a signed-in browser to a machine.
 *
 * It is read off a screen and retyped — sometimes read aloud down a phone — so
 * the alphabet matters more than the length does. Everything a human confuses
 * is gone: `0`/`O`, `1`/`I`/`L`, and `U` with it (Crockford drops `U` to keep
 * the alphabet from spelling things, and a code shown to a stranger is exactly
 * where that shows up). What is left is 30 symbols, and eight of them is
 * 30^8 ≈ 2^39 — far past guessing inside a ten-minute window, and still two
 * short groups of four to read out.
 *
 * Uppercase, and **not** case-folded on the way back in. The signature covers
 * the code as bytes, so any normalisation this end applies would have to be
 * applied identically by the daemon in Rust; a folding rule that lives in one
 * of two implementations is a signature mismatch that names nothing. Making
 * the code convenient to type is the *input's* job, and it happens entirely
 * before the string becomes bytes to sign.
 */

import { randomBytes } from '@zesterm/cloud-shared';

/** No `0`, `O`, `1`, `I`, `L` or `U`. 30 symbols. */
export const ENROLL_CODE_ALPHABET = '23456789ABCDEFGHJKMNPQRSTVWXYZ';

export const ENROLL_CODE_LENGTH = 8;

/**
 * Ten minutes.
 *
 * For the whole of its life the code is a bearer token: anyone who reads it —
 * over a shoulder, in a screenshot, in the scrollback of a shared terminal —
 * can enrol *their* machine into the account before the person who minted it
 * gets to theirs. The signature does not help, because the attacker signs with
 * their own key and that is a perfectly valid signature.
 *
 * So the window is sized to the task and nothing more: long enough to walk to
 * the other machine and type eight characters, short enough that a code left
 * on a screen at lunchtime is dead before anyone wanders past. Codes are also
 * single-use, so the common case closes the window in seconds; the expiry is
 * only what bounds the abandoned ones.
 */
export const ENROLL_CODE_TTL_MS = 10 * 60 * 1000;

/**
 * A fresh code, uniformly distributed over the alphabet.
 *
 * Rejection sampling rather than `byte % 30`: 256 is not a multiple of 30, so
 * the modulo would make the first 16 symbols 1/8 more likely than the rest.
 * That costs about a fifth of a bit here rather than anything dramatic, but the
 * fix is three lines and the alternative is a biased secret nobody re-measures.
 */
export function newEnrollCode(): string {
  const n = ENROLL_CODE_ALPHABET.length;
  const limit = 256 - (256 % n);
  let code = '';
  while (code.length < ENROLL_CODE_LENGTH) {
    for (const byte of randomBytes(ENROLL_CODE_LENGTH)) {
      if (byte >= limit) continue;
      code += ENROLL_CODE_ALPHABET[byte % n];
      if (code.length === ENROLL_CODE_LENGTH) break;
    }
  }
  return code;
}

/**
 * Reject a code that could not have come from `newEnrollCode`.
 *
 * Cheap, and it means a claim carrying junk costs no database round trip —
 * which is what stops an unauthenticated endpoint from being a way to load D1.
 */
export function looksLikeEnrollCode(code: string): boolean {
  if (code.length !== ENROLL_CODE_LENGTH) return false;
  return [...code].every((c) => ENROLL_CODE_ALPHABET.includes(c));
}

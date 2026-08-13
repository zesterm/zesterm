/**
 * Opaque session tokens, and the rule that they are never stored.
 *
 * A session token is 48 random bytes and means nothing by itself — no user id,
 * no expiry, no signature. That is the point: a token that carries claims can
 * be read, and a token that can be read is a token whose expiry an attacker
 * can see coming. The database holds the meaning.
 *
 * What *is* stored is `sha256(token)`. A dump of the `sessions` table is then
 * a list of hashes rather than a set of usable cookies, and it costs one hash
 * per request to get that.
 */

import { fromBase64Url, hex, randomBytes, toBase64Url, utf8 } from './bytes.ts';

/**
 * 48 bytes, not 32.
 *
 * 32 is ample against guessing; 48 is chosen because it base64url-encodes to
 * 64 characters with no padding, which keeps the cookie a round, obvious size
 * and removes the `=` that some middleware still mangles.
 */
export const SESSION_TOKEN_BYTES = 48;

export function newSessionToken(): string {
  return toBase64Url(randomBytes(SESSION_TOKEN_BYTES));
}

/** The `sessions.id` for a token. Hex, so it reads and indexes as a plain key. */
export async function sessionIdOf(token: string): Promise<string> {
  const digest = await crypto.subtle.digest('SHA-256', utf8(token));
  return hex(new Uint8Array(digest));
}

/**
 * Reject a token that could not have come from `newSessionToken`.
 *
 * Cheap, and it means a malformed cookie costs no database round trip at all —
 * which is what stops an unauthenticated flood of junk cookies from being a
 * way to load D1.
 */
export function looksLikeSessionToken(token: string): boolean {
  const bytes = fromBase64Url(token);
  return bytes !== null && bytes.length === SESSION_TOKEN_BYTES;
}

// --- machine tokens ---------------------------------------------------------

/**
 * The bearer credential a machine keeps after enrolling — same anatomy as a
 * session token (48 opaque bytes, only the hash is stored), because the
 * reasoning at the top of this file does not care who the bearer is.
 *
 * The `zt1_` prefix is the one deliberate difference. A session token lives in
 * a cookie jar; this one lives in an OS credential store and, inevitably, one
 * day in a pasted log line or an environment dump. A recognisable prefix makes
 * that leak greppable — secret scanners work on prefixes — and names the
 * format so a v2 can rotate in beside it.
 */
export const MACHINE_TOKEN_BYTES = 48;
const MACHINE_TOKEN_PREFIX = 'zt1_';

export function newMachineToken(): string {
  return MACHINE_TOKEN_PREFIX + toBase64Url(randomBytes(MACHINE_TOKEN_BYTES));
}

/** Shape check before any round trip, exactly as `looksLikeSessionToken`. */
export function looksLikeMachineToken(token: string): boolean {
  if (!token.startsWith(MACHINE_TOKEN_PREFIX)) return false;
  const bytes = fromBase64Url(token.slice(MACHINE_TOKEN_PREFIX.length));
  return bytes !== null && bytes.length === MACHINE_TOKEN_BYTES;
}

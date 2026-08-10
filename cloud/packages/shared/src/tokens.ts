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

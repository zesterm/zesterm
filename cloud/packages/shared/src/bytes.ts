/**
 * The byte plumbing every other file here needs, and nothing else.
 *
 * base64url rather than base64 throughout: these values travel in cookies and
 * in `Sec-WebSocket-Protocol` later, and `+`, `/` and `=` are all characters
 * that something in that path will eventually escape, truncate, or reject.
 */

const B64URL = /^[A-Za-z0-9_-]*$/;

export function toBase64Url(bytes: Uint8Array): string {
  let binary = '';
  for (const b of bytes) binary += String.fromCharCode(b);
  return btoa(binary).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

/** `null` on anything that is not well-formed base64url, rather than garbage. */
export function fromBase64Url(text: string): Uint8Array | null {
  if (!B64URL.test(text)) return null;
  const padded = text.replace(/-/g, '+').replace(/_/g, '/');
  try {
    const binary = atob(padded);
    const out = new Uint8Array(binary.length);
    for (let i = 0; i < binary.length; i++) out[i] = binary.charCodeAt(i);
    return out;
  } catch {
    return null;
  }
}

export function utf8(text: string): Uint8Array {
  return new TextEncoder().encode(text);
}

export function concat(parts: Uint8Array[]): Uint8Array {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let at = 0;
  for (const part of parts) {
    out.set(part, at);
    at += part.length;
  }
  return out;
}

export function hex(bytes: Uint8Array): string {
  let out = '';
  for (const b of bytes) out += b.toString(16).padStart(2, '0');
  return out;
}

/**
 * `null` on anything that is not `length` bytes of lowercase hex.
 *
 * The strictness is the point. Public keys and signatures arrive as hex from a
 * daemon, and the decoders below them (`@noble/ed25519`) accept a `Uint8Array`
 * of any length and then throw — so a lenient parser turns "the client sent 31
 * bytes" into an exception on a code path that was reasoning about
 * cryptography. Uppercase is refused rather than folded because these values
 * are also compared against, and stored as, `hex()`'s own lowercase output: two
 * spellings of one key are two rows.
 */
export function fromHex(text: string, length: number): Uint8Array | null {
  if (text.length !== length * 2) return null;
  const out = new Uint8Array(length);
  for (let i = 0; i < length; i++) {
    const pair = text.slice(i * 2, i * 2 + 2);
    if (!/^[0-9a-f]{2}$/.test(pair)) return null;
    out[i] = Number.parseInt(pair, 16);
  }
  return out;
}

/**
 * Compare two strings without leaking where they first differ.
 *
 * `===` on a secret returns as soon as a byte differs, so the time it takes is
 * a measure of how much of a guess was correct — which is enough to construct
 * the rest a byte at a time. Used for the OAuth `state` and for anything else
 * an attacker supplies and we compare against a value they want to learn.
 *
 * Lengths are compared by folding them into the accumulator rather than
 * returning early, so a wrong-length input costs the same as a wrong value.
 */
export function timingSafeEqual(a: string, b: string): boolean {
  const x = utf8(a);
  const y = utf8(b);
  let diff = x.length ^ y.length;
  const n = Math.max(x.length, y.length);
  for (let i = 0; i < n; i++) {
    // Out of range reads as 0 on both sides, so the loop length never depends
    // on which string was shorter.
    diff |= (x[i] ?? 0) ^ (y[i] ?? 0);
  }
  return diff === 0;
}

/** Cryptographically random bytes. */
export function randomBytes(n: number): Uint8Array {
  return crypto.getRandomValues(new Uint8Array(n));
}

/**
 * `base64url(sha256(input))` — PKCE's `S256` challenge, and nothing else needs it.
 *
 * Lives here rather than in the OAuth route because it is a byte operation and
 * the route should read as policy. `plain` is the other method the spec allows
 * and it is not offered: it sends the verifier itself, so anything that can see
 * the authorize request can complete the exchange, which is the entire attack
 * PKCE exists to stop.
 */
export async function sha256Base64Url(input: string): Promise<string> {
  const digest = await crypto.subtle.digest('SHA-256', utf8(input));
  return toBase64Url(new Uint8Array(digest));
}

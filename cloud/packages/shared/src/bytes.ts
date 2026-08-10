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

export function hex(bytes: Uint8Array): string {
  let out = '';
  for (const b of bytes) out += b.toString(16).padStart(2, '0');
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

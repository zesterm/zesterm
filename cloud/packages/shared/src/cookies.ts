/**
 * Cookie parsing, and the two cookies this service sets.
 *
 * Both carry the `__Host-` prefix, which is not decoration. The browser
 * enforces it: `Secure`, `Path=/`, and **no `Domain`** — so the cookie is
 * pinned to exactly this host and cannot be widened.
 *
 * That matters more here than usual. `workers.dev` is on the Public Suffix
 * List, so `zesterm.sigx.workers.dev` and every other Worker on the same
 * account are same-site siblings. A `Domain=`-scoped cookie would be sent to
 * all of them. `__Host-` makes that impossible to express, which is better
 * than remembering not to.
 */

import { fromBase64Url, timingSafeEqual, toBase64Url, utf8 } from './bytes.ts';

export const SESSION_COOKIE = '__Host-zt_session';
export const OAUTH_COOKIE = '__Host-zt_oauth';

/** One cookie's value from a `Cookie` header, or `null`. */
export function readCookie(header: string | null, name: string): string | null {
  if (!header) return null;
  for (const part of header.split(';')) {
    const eq = part.indexOf('=');
    if (eq === -1) continue;
    if (part.slice(0, eq).trim() !== name) continue;
    return part.slice(eq + 1).trim();
  }
  return null;
}

export interface CookieOptions {
  readonly maxAge: number;
  /**
   * `Lax`, essentially always — see `oauthStateCookie`. `Strict` is offered
   * because it is right for a cookie no cross-site navigation ever needs, not
   * because it is a safer default here.
   */
  readonly sameSite?: 'Lax' | 'Strict';
}

/** A `Set-Cookie` value with the `__Host-` requirements already met. */
export function setCookie(name: string, value: string, opts: CookieOptions): string {
  const sameSite = opts.sameSite ?? 'Lax';
  return `${name}=${value}; Path=/; HttpOnly; Secure; SameSite=${sameSite}; Max-Age=${opts.maxAge}`;
}

/** Expire a cookie. Same attributes, because a browser matches on them. */
export function clearCookie(name: string): string {
  return `${name}=; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=0`;
}

// --- signed payloads -------------------------------------------------------

/**
 * The OAuth round trip's state, MAC'd rather than stored.
 *
 * Keeping it in a cookie instead of a table means the callback needs no read
 * and an abandoned login needs no cleanup. It is safe because the cookie is
 * `HttpOnly` and signed: the browser holds it, but cannot forge it.
 */
export interface OAuthState {
  /** Which provider this round trip is for. */
  readonly p: string;
  /** The `state` parameter, echoed by the provider. */
  readonly s: string;
  /** PKCE verifier. Unused by GitHub; present so Google is a config change. */
  readonly v: string;
  /** Where to land afterwards. Validated as a path before use. */
  readonly n: string;
  /** Expiry, epoch ms. */
  readonly e: number;
}

// Return type inferred on purpose: naming `CryptoKey` would need the DOM lib,
// and this package deliberately compiles with neither DOM nor Workers types so
// that a runtime-specific API cannot creep into code both sides import.
async function macKey(secret: string) {
  return crypto.subtle.importKey(
    'raw',
    utf8(secret),
    { name: 'HMAC', hash: 'SHA-256' },
    false,
    ['sign'],
  );
}

/** `base64url(json).base64url(hmac)`. */
export async function sign(payload: unknown, secret: string): Promise<string> {
  const body = toBase64Url(utf8(JSON.stringify(payload)));
  const key = await macKey(secret);
  const mac = await crypto.subtle.sign('HMAC', key, utf8(body));
  return `${body}.${toBase64Url(new Uint8Array(mac))}`;
}

/**
 * Verify and parse, or `null`.
 *
 * Verified with `sign`-and-compare rather than `crypto.subtle.verify` so the
 * comparison goes through `timingSafeEqual`: this value is attacker-supplied
 * and compared against one they would like to produce.
 *
 * Returns `null` for every failure — bad shape, bad MAC, bad JSON — on purpose.
 * A caller that could tell "wrong signature" from "malformed" would be tempted
 * to log or report the difference, and the difference is exactly what a forging
 * attempt wants to learn.
 */
export async function unsign<T>(value: string, secret: string): Promise<T | null> {
  const dot = value.indexOf('.');
  if (dot <= 0 || dot === value.length - 1) return null;
  const body = value.slice(0, dot);
  const mac = value.slice(dot + 1);

  const key = await macKey(secret);
  const expected = toBase64Url(
    new Uint8Array(await crypto.subtle.sign('HMAC', key, utf8(body))),
  );
  if (!timingSafeEqual(mac, expected)) return null;

  const raw = fromBase64Url(body);
  if (raw === null) return null;
  try {
    return JSON.parse(new TextDecoder().decode(raw)) as T;
  } catch {
    return null;
  }
}

/**
 * The state cookie for one login attempt.
 *
 * **`SameSite=Lax`, and it must not be `Strict`.** The callback is a top-level
 * cross-site GET navigation from the provider's domain, and `Strict` withholds
 * the cookie on exactly that request. The login then fails as a state
 * mismatch — indistinguishable, in the logs, from the CSRF attack this cookie
 * exists to prevent. It is the single most common way a hand-rolled OAuth flow
 * breaks, and the symptom points at the wrong thing.
 */
export async function oauthStateCookie(
  state: OAuthState,
  secret: string,
  ttlSeconds: number,
): Promise<string> {
  return setCookie(OAUTH_COOKIE, await sign(state, secret), {
    maxAge: ttlSeconds,
    sameSite: 'Lax',
  });
}

/**
 * The `/api` and `/auth` surface.
 *
 * Everything that decides *what* to answer lives here; `index.ts` keeps only
 * what genuinely needs the runtime. `null` means "not mine" — the entrypoint
 * then hands the request to the asset binding, which is what serves the app.
 *
 * It takes `fetchImpl` and `now` so the whole OAuth round trip is testable
 * against a stubbed provider under `node --test`, with no workerd and no
 * network. Security code that can only be exercised by deploying is security
 * code that is exercised rarely.
 */

import { readCookie, SESSION_COOKIE } from '@zesterm/cloud-shared';

import { claimEnrollCode, mintEnrollCode } from './api/enroll.ts';
import { listRegistry, revokeRegistryEntry } from './api/registry.ts';
import { finishLogin, logout, startLogin } from './auth/routes.ts';
import { resolveSession } from './db/sessions.ts';
import type { Env } from './env.ts';
import { csrfOk, csrfOkWithoutOrigin, json } from './http.ts';

/**
 * The routes that are **not** protected by the `Origin` half of the CSRF rule.
 *
 * Exactly one, and it is here as a named exception rather than as a condition
 * buried in a handler, so that adding a second is a visible act. `/api/enroll/
 * claim` is answered by `zest-daemon` on someone's Mac: no browser, no origin
 * header, and — the part that makes the exemption sound rather than merely
 * convenient — no session cookie either. CSRF is the forgery of a request that
 * succeeds on credentials the page never had to know; a route that consults no
 * such credential has nothing to forge. See `csrfOkWithoutOrigin`.
 *
 * The corollary is a rule about the handler, not about this list: anything on
 * it must never read the session cookie. `/api/enroll/claim` authenticates with
 * a code the account minted and an Ed25519 signature over it.
 */
const ORIGINLESS = new Set(['/api/enroll/claim']);

/** `/api/hosts/:id/revoke`, `/api/devices/:id/revoke`. */
const REVOKE = /^\/api\/(hosts|devices)\/([^/]+)\/revoke$/;

export async function routeApi(
  request: Request,
  env: Env,
  fetchImpl: typeof fetch = fetch,
  now = Date.now(),
): Promise<Response | null> {
  const url = new URL(request.url);
  const path = url.pathname;

  if (!path.startsWith('/api/') && !path.startsWith('/auth/')) return null;

  // Checked once, before any handler, so a route cannot be added that forgets.
  const csrf = ORIGINLESS.has(path)
    ? csrfOkWithoutOrigin(request)
    : csrfOk(request, env.APP_ORIGIN);
  if (!csrf) return json({ error: 'forbidden' }, 403);

  if (path === '/auth/login') return startLogin(request, env, now);
  if (path.startsWith('/auth/callback/')) return finishLogin(request, env, fetchImpl, now);
  if (path === '/auth/logout') {
    if (request.method !== 'POST') return json({ error: 'method_not_allowed' }, 405);
    return logout(request, env);
  }

  if (path === '/api/bootstrap' || path === '/api/me') {
    if (request.method !== 'GET') return json({ error: 'method_not_allowed' }, 405);
    const user = await resolveSession(
      env.DB,
      readCookie(request.headers.get('cookie'), SESSION_COOKIE),
      now,
    );
    // `/api/me` is the same answer without the envelope -- one source of truth
    // for "who is this", so the two can never disagree about it.
    return path === '/api/me' ? json({ user }) : json({ mode: 'cloud', user });
  }

  if (path === '/api/enroll/code') {
    if (request.method !== 'POST') return json({ error: 'method_not_allowed' }, 405);
    return mintEnrollCode(request, env, now);
  }
  if (path === '/api/enroll/claim') {
    if (request.method !== 'POST') return json({ error: 'method_not_allowed' }, 405);
    return claimEnrollCode(request, env, now);
  }

  if (path === '/api/hosts' || path === '/api/devices') {
    if (request.method !== 'GET') return json({ error: 'method_not_allowed' }, 405);
    return listRegistry(request, env, path === '/api/hosts' ? 'host' : 'device', now);
  }

  const revoke = REVOKE.exec(path);
  if (revoke !== null) {
    if (request.method !== 'POST') return json({ error: 'method_not_allowed' }, 405);
    const [, table = '', id = ''] = revoke;
    return revokeRegistryEntry(request, env, table === 'hosts' ? 'host' : 'device', id, now);
  }

  // An unknown path under either prefix is JSON, never the SPA fallback:
  // falling through would hand JavaScript a 200 full of HTML, which surfaces
  // as a parse error naming a line of markup and points nowhere near the cause.
  return json({ error: 'not_found' }, 404);
}

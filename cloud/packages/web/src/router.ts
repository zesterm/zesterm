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
import { mintRelayTicket } from './api/relay.ts';
import { finishLogin, logout, startLogin } from './auth/routes.ts';
import { requestPrincipal } from './api/principal.ts';
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

/**
 * The routes that also answer machines — `Authorization: Bearer` from a daemon
 * or the desktop app, which have no cookie, no browser and no origin.
 *
 * A request carrying that header on one of these paths drops the `Origin` half
 * of the CSRF rule, and the exemption is sound for the same reason
 * `ORIGINLESS`'s is: `requestPrincipal` resolves such a request by the token
 * *alone* and never falls back to the cookie, so a request that skipped the
 * origin check is a request whose ambient credentials were never consulted.
 * The same path reached *without* the header is an ordinary cookie route and
 * keeps the full rule — membership here loosens nothing for browsers.
 */
const BEARER = new Set(['/api/me', '/api/hosts', '/api/relay/ticket']);

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
  const bearer = request.headers.get('authorization') !== null;
  const csrf =
    ORIGINLESS.has(path) || (bearer && BEARER.has(path))
      ? csrfOkWithoutOrigin(request)
      : csrfOk(request, env.APP_ORIGIN);
  if (!csrf) return json({ error: 'forbidden' }, 403);

  if (path === '/auth/login') return startLogin(request, env, now);
  if (path.startsWith('/auth/callback/')) return finishLogin(request, env, fetchImpl, now);
  if (path === '/auth/logout') {
    if (request.method !== 'POST') return json({ error: 'method_not_allowed' }, 405);
    return logout(request, env);
  }

  if (path === '/api/me') {
    if (request.method !== 'GET') return json({ error: 'method_not_allowed' }, 405);
    // One resolver for "who is this", cookie or bearer. A person keeps the
    // shape the app already reads; a machine gets its principal named back —
    // which is how the desktop app learns the `userId` its attestations must
    // carry — and `user` stays present-but-null so nothing switches on a
    // missing key.
    const principal = await requestPrincipal(request, env, now);
    if (principal === null) return json({ user: null });
    return principal.kind === 'user'
      ? json({ user: principal.user })
      : json({
          user: null,
          principal: { kind: principal.kind, id: principal.id, userId: principal.userId },
        });
  }

  if (path === '/api/bootstrap') {
    if (request.method !== 'GET') return json({ error: 'method_not_allowed' }, 405);
    const user = await resolveSession(
      env.DB,
      readCookie(request.headers.get('cookie'), SESSION_COOKIE),
      now,
    );
    // `relayOrigin` is on the envelope rather than baked into the bundle for
    // the reason `mode` is: the app ships as one `vite build` serving both the
    // loopback sidecar and the edge, so anything it learns from a `VITE_*`
    // variable is something the shipped bundle was never tested with. `null`
    // is the ordinary answer for a deployment with no relay.
    return json({ mode: 'cloud', user, relayOrigin: env.RELAY_ORIGIN ?? null });
  }

  if (path === '/api/enroll/code') {
    if (request.method !== 'POST') return json({ error: 'method_not_allowed' }, 405);
    return mintEnrollCode(request, env, now);
  }
  if (path === '/api/enroll/claim') {
    if (request.method !== 'POST') return json({ error: 'method_not_allowed' }, 405);
    return claimEnrollCode(request, env, now);
  }

  if (path === '/api/relay/ticket') {
    if (request.method !== 'POST') return json({ error: 'method_not_allowed' }, 405);
    return mintRelayTicket(request, env, now);
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

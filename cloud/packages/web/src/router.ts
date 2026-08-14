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

import { approveDevice, listAccountAttestations } from './api/attest.ts';
import { approveLink, claimLink, denyLink, getLinkGrant, startLink } from './api/link.ts';
import { registerDevice } from './api/devices.ts';
import { claimEnrollCode, mintEnrollCode } from './api/enroll.ts';
import { listRegistry, revokeRegistryEntry } from './api/registry.ts';
import { mintRelayTicket } from './api/relay.ts';
import { finishLogin, logout, startLogin } from './auth/routes.ts';
import { carriesBearer, requestPrincipal } from './api/principal.ts';
import { resolveSession } from './db/sessions.ts';
import type { Env } from './env.ts';
import { csrfOk, csrfOkWithoutOrigin, json } from './http.ts';

/**
 * The routes that are **not** protected by the `Origin` half of the CSRF rule.
 *
 * A named list rather than conditions buried in handlers, so that adding one
 * is a visible act. All three are answered by a machine — `zest-daemon` on
 * someone's Mac, or the desktop app linking itself (#226): no browser, no
 * origin header, and — the part that makes the exemption sound rather than
 * merely convenient — no session cookie either. CSRF is the forgery of a
 * request that succeeds on credentials the page never had to know; a route
 * that consults no such credential has nothing to forge. See
 * `csrfOkWithoutOrigin`.
 *
 * The corollary is a rule about the handler, not about this list: anything on
 * it must never read the session cookie. `/api/enroll/claim` authenticates
 * with a code the account minted plus an Ed25519 signature over it;
 * `/api/link/start` and `/api/link/claim` with signatures over the
 * `zesterm-link-v1` messages. The link *approval* is deliberately NOT here —
 * it is the account-holder's act, an ordinary cookie route under the full
 * rule.
 */
const ORIGINLESS = new Set(['/api/enroll/claim', '/api/link/start', '/api/link/claim']);

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
const BEARER = new Set([
  '/api/me',
  '/api/hosts',
  '/api/devices',
  '/api/relay/ticket',
  '/api/attestations',
  // The desktop app minting a *host* code for "Enroll this machine"
  // (issue #227); which principals may mint which kinds is the handler's
  // policy, stated in `mintEnrollCode`.
  '/api/enroll/code',
]);

/**
 * The one *parameterised* bearer route, which a `Set` of exact paths cannot
 * hold: the desktop app approves a device with its token, and the device id
 * is in the path. The handler keeps the exemption sound the way every BEARER
 * route does — `requestPrincipal` never falls back to the cookie — and adds
 * its own rule on top: a bearer principal may only submit vouchers it signed
 * itself.
 */
const APPROVE = /^\/api\/devices\/([^/]+)\/approve$/;

/** `/api/hosts/:id/revoke`, `/api/devices/:id/revoke`. */
const REVOKE = /^\/api\/(hosts|devices)\/([^/]+)\/revoke$/;

/** `GET /api/link/:id` — the approval page's read. Cookie, full CSRF. */
const LINK_GET = /^\/api\/link\/([^/]+)$/;

/** `POST /api/link/:id/approve`, `POST /api/link/:id/deny`. Cookie, full CSRF. */
const LINK_ACT = /^\/api\/link\/([^/]+)\/(approve|deny)$/;

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
  // `carriesBearer`, not "any Authorization header": a `Basic` header from
  // some proxy must not widen the exemption to a request the resolver would
  // never answer by token anyway.
  const csrf =
    ORIGINLESS.has(path) || (carriesBearer(request) && (BEARER.has(path) || APPROVE.test(path)))
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
    //
    // A machine's answer also carries `relayOrigin`, and only a machine's
    // (#229). A daemon holding a cloud token has to learn where to park its
    // control link, and every other place that says so is closed to it: a
    // *person* reads `/api/bootstrap`, which needs a session cookie, and
    // `/api/hosts` refuses host tokens on purpose — a machine serving shells
    // has no business enumerating its owner's other machines. Widening that
    // listing to get one string would trade a real boundary for a field. The
    // person's answer here is unchanged and still carries nothing: the rule
    // is that the *browser* learns the relay in one place, not that this
    // route never mentions it, and a daemon has no bootstrap to read.
    const principal = await requestPrincipal(request, env, now);
    if (principal === null) return json({ user: null });
    return principal.kind === 'user'
      ? json({ user: principal.user })
      : json({
          user: null,
          principal: { kind: principal.kind, id: principal.id, userId: principal.userId },
          // `null`, never absent, for `/api/bootstrap`'s reason: "this
          // deployment has no relay" is an answer, and a daemon that cannot
          // tell it from a field it failed to read would retry for ever.
          relayOrigin: env.RELAY_ORIGIN ?? null,
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

  // Deliberately in neither ORIGINLESS nor BEARER: it reads the session
  // cookie, so it keeps the full CSRF rule, and a machine credential must not
  // be able to register further keys.
  if (path === '/api/devices/register') {
    if (request.method !== 'POST') return json({ error: 'method_not_allowed' }, 405);
    return registerDevice(request, env, now);
  }

  if (path === '/api/hosts' || path === '/api/devices') {
    if (request.method !== 'GET') return json({ error: 'method_not_allowed' }, 405);
    return listRegistry(request, env, path === '/api/hosts' ? 'host' : 'device', now);
  }

  // The two ORIGINLESS halves first, then the cookie-authenticated remainder
  // of /api/link — exact paths before the :id patterns, so `start` and
  // `claim` can never be read as grant ids.
  if (path === '/api/link/start') {
    if (request.method !== 'POST') return json({ error: 'method_not_allowed' }, 405);
    return startLink(request, env, now);
  }
  if (path === '/api/link/claim') {
    if (request.method !== 'POST') return json({ error: 'method_not_allowed' }, 405);
    return claimLink(request, env, now);
  }
  const linkAct = LINK_ACT.exec(path);
  if (linkAct !== null) {
    if (request.method !== 'POST') return json({ error: 'method_not_allowed' }, 405);
    const [, id = '', act = ''] = linkAct;
    return act === 'approve' ? approveLink(request, env, id, now) : denyLink(request, env, id, now);
  }
  const linkGet = LINK_GET.exec(path);
  if (linkGet !== null) {
    if (request.method !== 'GET') return json({ error: 'method_not_allowed' }, 405);
    return getLinkGrant(request, env, linkGet[1] ?? '', now);
  }

  if (path === '/api/attestations') {
    if (request.method !== 'GET') return json({ error: 'method_not_allowed' }, 405);
    return listAccountAttestations(request, env, now);
  }

  const approve = APPROVE.exec(path);
  if (approve !== null) {
    if (request.method !== 'POST') return json({ error: 'method_not_allowed' }, 405);
    return approveDevice(request, env, approve[1] ?? '', now);
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

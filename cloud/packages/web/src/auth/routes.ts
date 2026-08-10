/**
 * `/auth/*` — the login round trip, provider-blind.
 *
 * Every security decision is here rather than in a provider file, so adding
 * Google cannot accidentally add a second, weaker copy of any of it.
 */

import {
  clearCookie,
  OAUTH_COOKIE,
  oauthStateCookie,
  randomBytes,
  readCookie,
  SESSION_COOKIE,
  setCookie,
  sha256Base64Url,
  timingSafeEqual,
  toBase64Url,
  unsign,
  type OAuthState,
} from '@zesterm/cloud-shared';

import { createSession, destroySession, SESSION_TTL_MS } from '../db/sessions.ts';
import { upsertUser } from '../db/users.ts';
import type { Env } from '../env.ts';
import { github, parseTokenResponse } from './github.ts';
import { OAuthError, type OAuthProvider } from './provider.ts';

/** Adding Google is an entry here plus one file. */
export const PROVIDERS: Record<string, OAuthProvider> = { github };

const STATE_TTL_S = 600;

/**
 * Where to land after signing in.
 *
 * A path, always. An attacker-supplied `next` that is allowed to be absolute
 * turns the login into an open redirect — and one that inherits this origin's
 * credibility, which is exactly what makes it worth phishing with.
 *
 * `//evil.example` is the case a naive `startsWith('/')` misses: the browser
 * reads it as a protocol-relative URL and leaves the site.
 */
export function safeNext(raw: string | null): string {
  if (!raw) return '/';
  if (!raw.startsWith('/')) return '/';
  if (raw.startsWith('//')) return '/';
  // A backslash is normalised to a slash by some browsers, so `/\evil.example`
  // is the same trick wearing a different hat.
  if (raw.startsWith('/\\')) return '/';
  return raw;
}

function redirect(to: string, cookies: string[] = []): Response {
  const headers = new Headers({ location: to });
  for (const c of cookies) headers.append('set-cookie', c);
  return new Response(null, { status: 302, headers });
}

/**
 * `GET /auth/login?provider=github&next=/hosts`
 *
 * Takes `now` for the same reason `finishLogin` does: the state's expiry is
 * written here and checked there, so a test that cannot control both clocks
 * cannot test expiry at all -- it silently proves that a fresh state is
 * accepted, twice.
 */
export async function startLogin(request: Request, env: Env, now = Date.now()): Promise<Response> {
  const url = new URL(request.url);
  const provider = PROVIDERS[url.searchParams.get('provider') ?? 'github'];
  if (!provider) return new Response('unknown provider', { status: 404 });

  const state: OAuthState = {
    p: provider.id,
    s: toBase64Url(randomBytes(32)),
    v: toBase64Url(randomBytes(32)),
    n: safeNext(url.searchParams.get('next')),
    e: now + STATE_TTL_S * 1000,
  };

  const authorize = new URL(provider.authorizeUrl);
  authorize.searchParams.set('client_id', provider.clientId(env));
  authorize.searchParams.set('redirect_uri', `${env.APP_ORIGIN}/auth/callback/${provider.id}`);
  authorize.searchParams.set('scope', provider.scopes.join(' '));
  authorize.searchParams.set('state', state.s);
  // Nobody should be creating an account by accident on a terminal's sign-in.
  authorize.searchParams.set('allow_signup', 'false');
  if (provider.usePkce) {
    // S256, never `plain`. `plain` sends the verifier itself, so anything that
    // can see the authorize request can complete the exchange -- the whole
    // attack PKCE exists to stop. Google rejects `plain` outright, and the
    // reason it is right here is not that Google asks: shipping the weak
    // method now means whoever wires the next provider inherits it.
    authorize.searchParams.set('code_challenge', await sha256Base64Url(state.v));
    authorize.searchParams.set('code_challenge_method', 'S256');
  }

  return redirect(authorize.toString(), [
    await oauthStateCookie(state, env.COOKIE_MAC_KEY, STATE_TTL_S),
  ]);
}

/** `GET /auth/callback/:provider?code=&state=` */
export async function finishLogin(
  request: Request,
  env: Env,
  fetchImpl: typeof fetch = fetch,
  now = Date.now(),
): Promise<Response> {
  const url = new URL(request.url);
  const id = url.pathname.slice('/auth/callback/'.length);
  const provider = PROVIDERS[id];

  // The state cookie is cleared on **every** exit path, success or failure.
  // A one-shot value left in the browser is one that can be replayed.
  const cleanup = [clearCookie(OAUTH_COOKIE)];
  const fail = (): Response => redirect('/login?error=1', cleanup);

  if (!provider) return fail();

  const raw = readCookie(request.headers.get('cookie'), OAUTH_COOKIE);
  if (raw === null) return fail();
  const state = await unsign<OAuthState>(raw, env.COOKIE_MAC_KEY);
  if (state === null) return fail();
  if (state.p !== provider.id) return fail();
  if (state.e <= now) return fail();

  const returned = url.searchParams.get('state');
  // Constant-time: this is attacker-supplied and compared against a value they
  // would like to produce.
  if (returned === null || !timingSafeEqual(returned, state.s)) return fail();

  const code = url.searchParams.get('code');
  if (code === null) return fail();

  try {
    const body = new URLSearchParams({
      client_id: provider.clientId(env),
      client_secret: provider.clientSecret(env),
      code,
      redirect_uri: `${env.APP_ORIGIN}/auth/callback/${provider.id}`,
    });
    if (provider.usePkce) body.set('code_verifier', state.v);

    const res = await fetchImpl(provider.tokenUrl, {
      method: 'POST',
      headers: { accept: 'application/json', 'content-type': 'application/x-www-form-urlencoded' },
      body: body.toString(),
    });
    // Status is not the answer here -- GitHub returns 200 with an error body.
    const tokens = parseTokenResponse(res.status, await res.json());
    const profile = await provider.profile(tokens, fetchImpl);

    const userId = await upsertUser(env.DB, provider.id, profile, now);
    const session = await createSession(env.DB, userId, now, {
      userAgent: request.headers.get('user-agent'),
      country: (request as { cf?: { country?: string } }).cf?.country ?? null,
    });

    return redirect(safeNext(state.n), [
      ...cleanup,
      setCookie(SESSION_COOKIE, session.token, { maxAge: Math.floor(SESSION_TTL_MS / 1000) }),
    ]);
  } catch (e) {
    // The access token and the client secret are both in scope above. Whatever
    // went wrong, the browser gets a redirect and nothing else.
    if (!(e instanceof OAuthError)) throw e;
    return fail();
  }
}

/** `POST /auth/logout` */
export async function logout(request: Request, env: Env): Promise<Response> {
  await destroySession(env.DB, readCookie(request.headers.get('cookie'), SESSION_COOKIE));
  return new Response(null, {
    status: 204,
    headers: { 'set-cookie': clearCookie(SESSION_COOKIE) },
  });
}

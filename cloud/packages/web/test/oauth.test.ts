/**
 * The login round trip end to end, against a stubbed GitHub.
 *
 * No workerd, no network, no OAuth app — so the flow that is otherwise only
 * exercised by a person clicking "sign in" is covered by the suite, including
 * the failure modes nobody clicks on purpose.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { OAUTH_COOKIE, SESSION_COOKIE, readCookie } from '@zesterm/cloud-shared';

import { safeNext } from '../src/auth/routes.ts';
import { routeApi } from '../src/router.ts';
import { seedUser, testDb, type TestDb } from './d1.ts';
import type { Env } from '../src/env.ts';

const ORIGIN = 'https://zesterm.sigx.workers.dev';
const NOW = 1_700_000_000_000;

function env(db: TestDb): Env {
  return {
    ASSETS: { fetch: async () => new Response('assets') },
    DB: db,
    APP_ORIGIN: ORIGIN,
    GITHUB_CLIENT_ID: 'client-id',
    GITHUB_CLIENT_SECRET: 'client-secret',
    COOKIE_MAC_KEY: 'mac-key',
  };
}

interface StubOptions {
  readonly tokenBody?: unknown;
  readonly tokenStatus?: number;
  readonly user?: unknown;
  readonly emails?: unknown;
  readonly emailsStatus?: number;
}

/** GitHub, as far as this flow can tell. */
function stubGitHub(opts: StubOptions = {}): typeof fetch {
  return (async (input: string | URL | Request) => {
    const url = typeof input === 'string' ? input : input instanceof URL ? input.href : input.url;
    if (url.includes('login/oauth/access_token')) {
      return new Response(JSON.stringify(opts.tokenBody ?? { access_token: 'gho_test' }), {
        // GitHub answers a *failed* exchange with 200 and an error body, so the
        // default here is 200 either way -- that is the trap being modelled.
        status: opts.tokenStatus ?? 200,
        headers: { 'content-type': 'application/json' },
      });
    }
    if (url.endsWith('/user')) {
      return new Response(
        JSON.stringify(
          opts.user ?? { id: 4242, login: 'andy', name: 'Andy', avatar_url: 'https://a/x.png' },
        ),
        { headers: { 'content-type': 'application/json' } },
      );
    }
    if (url.endsWith('/user/emails')) {
      return new Response(
        JSON.stringify(opts.emails ?? [{ email: 'andy@example.com', primary: true, verified: true }]),
        { status: opts.emailsStatus ?? 200, headers: { 'content-type': 'application/json' } },
      );
    }
    throw new Error(`unexpected fetch: ${url}`);
  }) as unknown as typeof fetch;
}

/** Start a login and pull the state cookie and the `state` param out of it. */
async function begin(db: TestDb, next = '/hosts', now = NOW) {
  const res = await routeApi(
    new Request(`${ORIGIN}/auth/login?provider=github&next=${encodeURIComponent(next)}`),
    env(db),
    fetch,
    now,
  );
  assert.equal(res?.status, 302);
  const location = new URL(res!.headers.get('location')!);
  const cookie = res!.headers.get('set-cookie')!;
  return {
    location,
    cookieHeader: `${OAUTH_COOKIE}=${cookie.slice(cookie.indexOf('=') + 1, cookie.indexOf(';'))}`,
    state: location.searchParams.get('state')!,
  };
}

function callback(state: string, cookieHeader: string, code = 'the-code') {
  return new Request(
    `${ORIGIN}/auth/callback/github?code=${code}&state=${encodeURIComponent(state)}`,
    { headers: { cookie: cookieHeader } },
  );
}

test('the authorize redirect carries everything GitHub needs', async () => {
  const db = testDb();
  const { location } = await begin(db);

  assert.equal(location.origin + location.pathname, 'https://github.com/login/oauth/authorize');
  assert.equal(location.searchParams.get('client_id'), 'client-id');
  assert.equal(
    location.searchParams.get('redirect_uri'),
    `${ORIGIN}/auth/callback/github`,
    'must match the OAuth app exactly or GitHub refuses',
  );
  assert.equal(location.searchParams.get('scope'), 'read:user user:email');
  assert.ok((location.searchParams.get('state') ?? '').length >= 32);
  assert.equal(location.searchParams.get('allow_signup'), 'false');
  db.close();
});

test('a full round trip signs the person in and lands them where they asked', async () => {
  const db = testDb();
  const { state, cookieHeader } = await begin(db, '/hosts');

  const res = await routeApi(callback(state, cookieHeader), env(db), stubGitHub(), NOW);
  assert.equal(res?.status, 302);
  assert.equal(res!.headers.get('location'), '/hosts');

  const setCookies = res!.headers.getSetCookie();
  const session = setCookies.find((c) => c.startsWith(SESSION_COOKIE));
  assert.ok(session, 'a session cookie must be set');
  assert.ok(session.includes('HttpOnly') && session.includes('Secure'));
  assert.ok(setCookies.some((c) => c.startsWith(OAUTH_COOKIE) && c.includes('Max-Age=0')),
    'the one-shot state cookie must be cleared');

  // And it actually works.
  const token = readCookie(session.replace(/;.*$/, ''), SESSION_COOKIE);
  const me = await routeApi(
    new Request(`${ORIGIN}/api/me`, { headers: { cookie: `${SESSION_COOKIE}=${token}` } }),
    env(db),
    fetch,
    NOW,
  );
  const { user } = (await me!.json()) as { user: { displayName: string } | null };
  assert.equal(user?.displayName, 'Andy');
  db.close();
});

test('the account is keyed on the numeric id, not the login', async () => {
  const db = testDb();
  const first = await begin(db);
  await routeApi(callback(first.state, first.cookieHeader), env(db), stubGitHub(), NOW);

  // Same person, renamed on GitHub.
  const second = await begin(db);
  await routeApi(
    callback(second.state, second.cookieHeader),
    env(db),
    stubGitHub({ user: { id: 4242, login: 'andreas', name: 'Andreas', avatar_url: null } }),
    NOW + 1000,
  );

  assert.equal(
    (db.raw.prepare('SELECT COUNT(*) AS n FROM users').get() as { n: number }).n,
    1,
    'a rename must not create a second account',
  );
  db.close();
});

// --- the ways it must fail -------------------------------------------------

test('a mismatched state is refused', async () => {
  const db = testDb();
  const { cookieHeader } = await begin(db);
  const res = await routeApi(callback('not-the-state', cookieHeader), env(db), stubGitHub(), NOW);

  assert.equal(res?.status, 302);
  assert.match(res!.headers.get('location') ?? '', /^\/login\?error=/);
  assert.equal((db.raw.prepare('SELECT COUNT(*) AS n FROM sessions').get() as { n: number }).n, 0);
  db.close();
});

test('a callback with no state cookie is refused', async () => {
  // What an attacker's cross-site link produces: they can supply `state` in the
  // URL, but not the HttpOnly cookie it must match.
  const db = testDb();
  const { state } = await begin(db);
  const res = await routeApi(
    new Request(`${ORIGIN}/auth/callback/github?code=c&state=${state}`),
    env(db),
    stubGitHub(),
    NOW,
  );
  assert.match(res!.headers.get('location') ?? '', /^\/login\?error=/);
  db.close();
});

test('an expired state is refused', async () => {
  const db = testDb();
  const { state, cookieHeader } = await begin(db);
  const res = await routeApi(
    callback(state, cookieHeader),
    env(db),
    stubGitHub(),
    NOW + 3600_000, // long past the 10-minute window
  );
  assert.match(res!.headers.get('location') ?? '', /^\/login\?error=/);
  db.close();
});

test("GitHub's 200-with-an-error is treated as the failure it is", async () => {
  // The trap: `res.ok` is true, so a status check alone reports success and the
  // failure surfaces much later as a 401 from the API, far from the expired
  // code that caused it.
  const db = testDb();
  const { state, cookieHeader } = await begin(db);
  const res = await routeApi(
    callback(state, cookieHeader),
    env(db),
    stubGitHub({ tokenBody: { error: 'bad_verification_code' } }),
    NOW,
  );

  assert.match(res!.headers.get('location') ?? '', /^\/login\?error=/);
  assert.equal((db.raw.prepare('SELECT COUNT(*) AS n FROM users').get() as { n: number }).n, 0);
  db.close();
});

test('a profile with no verified address signs in, but cannot link to anyone', async () => {
  // Private email is ordinary on GitHub, so this must not be a failure -- but
  // it must not be a link target either.
  const db = testDb();
  seedUser(db, 'someone-else');
  db.raw
    .prepare(
      `INSERT INTO oauth_identities (provider, subject, user_id, email, email_verified, created_at, last_login_at)
       VALUES ('google', 'g-1', 'someone-else', 'andy@example.com', 1, 0, 0)`,
    )
    .run();

  const { state, cookieHeader } = await begin(db);
  await routeApi(
    callback(state, cookieHeader),
    env(db),
    stubGitHub({ emails: [{ email: 'andy@example.com', primary: true, verified: false }] }),
    NOW,
  );

  const identity = db.raw
    .prepare(`SELECT user_id FROM oauth_identities WHERE provider = 'github'`)
    .get() as { user_id: string };
  assert.notEqual(identity.user_id, 'someone-else', 'an unverified claim must not join an account');
  db.close();
});

test('safeNext refuses everything that leaves the site', () => {
  assert.equal(safeNext('/hosts'), '/hosts');
  assert.equal(safeNext('/h/a/s/1?x=1'), '/h/a/s/1?x=1');
  assert.equal(safeNext(null), '/');
  assert.equal(safeNext('https://evil.example'), '/', 'absolute');
  assert.equal(safeNext('//evil.example'), '/', 'protocol-relative — the one a startsWith("/") misses');
  assert.equal(safeNext('/\\evil.example'), '/', 'backslash, which some browsers normalise');
  assert.equal(safeNext('javascript:alert(1)'), '/');
});

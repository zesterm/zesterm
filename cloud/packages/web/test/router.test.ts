import { test } from 'node:test';
import assert from 'node:assert/strict';

import { SESSION_COOKIE } from '@zesterm/cloud-shared';

import { createSession } from '../src/db/sessions.ts';
import { routeApi } from '../src/router.ts';
import { seedUser, testDb, type TestDb } from './d1.ts';
import type { Env } from '../src/env.ts';

const ORIGIN = 'https://zesterm.sigx.workers.dev';

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

const get = (path: string, cookie?: string) =>
  new Request(`${ORIGIN}${path}`, cookie ? { headers: { cookie } } : undefined);

test('bootstrap says cloud, and signed out when there is no cookie', async () => {
  const db = testDb();
  const res = await routeApi(get('/api/bootstrap'), env(db));
  assert.ok(res, '/api/bootstrap must be handled by the Worker, not the assets');
  assert.equal(res.status, 200);
  assert.deepEqual(await res.json(), { mode: 'cloud', user: null });
  db.close();
});

test('bootstrap and /api/me report the signed-in user', async () => {
  const db = testDb();
  seedUser(db, 'user-a');
  const { token } = await createSession(db, 'user-a', Date.now());
  const cookie = `${SESSION_COOKIE}=${token}`;

  const boot = await routeApi(get('/api/bootstrap', cookie), env(db));
  const body = (await boot!.json()) as { mode: string; user: { id: string } | null };
  assert.equal(body.mode, 'cloud');
  assert.equal(body.user?.id, 'user-a');

  // One source of truth for "who is this", so the two cannot disagree.
  const me = await routeApi(get('/api/me', cookie), env(db));
  assert.deepEqual((await me!.json()) as { user: unknown }, { user: body.user });
  db.close();
});

test('the user shape is built field by field, not spread from the row', async () => {
  // `users` will grow columns. A spread would ship each new one to the browser
  // by default; the safe direction for that mistake is a missing field.
  const db = testDb();
  seedUser(db, 'user-a');
  const { token } = await createSession(db, 'user-a', Date.now());

  const res = await routeApi(get('/api/me', `${SESSION_COOKIE}=${token}`), env(db));
  const { user } = (await res!.json()) as { user: Record<string, unknown> };
  assert.deepEqual(Object.keys(user).sort(), ['avatarUrl', 'displayName', 'email', 'id']);
  db.close();
});

test('a forged session cookie is simply signed out', async () => {
  const db = testDb();
  for (const bad of ['nonsense', 'A'.repeat(64), '']) {
    const res = await routeApi(get('/api/me', `${SESSION_COOKIE}=${bad}`), env(db));
    assert.deepEqual(await res!.json(), { user: null }, `${bad || '(empty)'} should not sign in`);
  }
  db.close();
});

test('api answers are never cached', async () => {
  const db = testDb();
  const res = await routeApi(get('/api/bootstrap'), env(db));
  assert.equal(res?.headers.get('cache-control'), 'no-store');
  assert.match(res?.headers.get('content-type') ?? '', /^application\/json/);
  db.close();
});

test('a non-api path is declined so the assets can serve the app', async () => {
  const db = testDb();
  for (const path of ['/', '/hosts', '/h/abc/s/1', '/assets/index-abc123.js', '/apiary']) {
    assert.equal(await routeApi(get(path), env(db)), null, `${path} must fall through to ASSETS`);
  }
  db.close();
});

test('an unknown path under /api or /auth is JSON 404, never the SPA fallback', async () => {
  const db = testDb();
  for (const path of ['/api/nope', '/auth/nope']) {
    const res = await routeApi(get(path), env(db));
    assert.equal(res?.status, 404, path);
    assert.match(res?.headers.get('content-type') ?? '', /^application\/json/);
  }
  db.close();
});

test('GET-only routes refuse other methods', async () => {
  const db = testDb();
  const res = await routeApi(
    new Request(`${ORIGIN}/api/bootstrap`, {
      method: 'POST',
      headers: { origin: ORIGIN, 'content-type': 'application/json' },
    }),
    env(db),
  );
  assert.equal(res?.status, 405);
  db.close();
});

// --- CSRF ------------------------------------------------------------------

test('a cross-site write is refused', async () => {
  const db = testDb();
  const res = await routeApi(
    new Request(`${ORIGIN}/auth/logout`, {
      method: 'POST',
      headers: { origin: 'https://evil.example', 'content-type': 'application/json' },
    }),
    env(db),
  );
  assert.equal(res?.status, 403);
  db.close();
});

test('a form-shaped write is refused even from our own origin', async () => {
  // The one cross-site request a browser makes without a preflight is a form
  // POST, and a form cannot set application/json. This is the half of the CSRF
  // rule that does not depend on Origin being present.
  const db = testDb();
  for (const ct of ['application/x-www-form-urlencoded', 'multipart/form-data', 'text/plain']) {
    const res = await routeApi(
      new Request(`${ORIGIN}/auth/logout`, {
        method: 'POST',
        headers: { origin: ORIGIN, 'content-type': ct },
      }),
      env(db),
    );
    assert.equal(res?.status, 403, ct);
  }
  db.close();
});

test('a missing Origin fails closed', async () => {
  // Some privacy tooling strips it. A rejected write is the right cost; an
  // accepted forgery is not.
  const db = testDb();
  const res = await routeApi(
    new Request(`${ORIGIN}/auth/logout`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
    }),
    env(db),
  );
  assert.equal(res?.status, 403);
  db.close();
});

test('a same-origin JSON write is allowed, and a content-type parameter does not break it', async () => {
  const db = testDb();
  const res = await routeApi(
    new Request(`${ORIGIN}/auth/logout`, {
      method: 'POST',
      headers: { origin: ORIGIN, 'content-type': 'application/json; charset=utf-8' },
    }),
    env(db),
  );
  assert.equal(res?.status, 204);
  db.close();
});

test('logout ends the session and clears the cookie', async () => {
  const db = testDb();
  seedUser(db, 'user-a');
  const { token } = await createSession(db, 'user-a', Date.now());

  const res = await routeApi(
    new Request(`${ORIGIN}/auth/logout`, {
      method: 'POST',
      headers: {
        origin: ORIGIN,
        'content-type': 'application/json',
        cookie: `${SESSION_COOKIE}=${token}`,
      },
    }),
    env(db),
  );
  assert.equal(res?.status, 204);
  assert.match(res?.headers.get('set-cookie') ?? '', /Max-Age=0/);

  const after = await routeApi(get('/api/me', `${SESSION_COOKIE}=${token}`), env(db));
  assert.deepEqual(await after!.json(), { user: null }, 'the row is gone, not just the cookie');
  db.close();
});

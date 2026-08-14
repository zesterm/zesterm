/**
 * Machine tokens: the bearer credential a claim hands back, and the routes
 * that accept it.
 *
 * Most of what is asserted is about what a token must *not* do: act as a
 * cookie, survive its principal's revocation, list what its kind may not list,
 * or — the one that keeps the router's exemption sound — fall back to the
 * session when the token is bad. The happy paths are almost incidental.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { looksLikeMachineToken, SESSION_COOKIE } from '@zesterm/cloud-shared';

import { createSession } from '../src/db/sessions.ts';
import { MACHINE_TOKEN_TTL_MS } from '../src/db/machine-tokens.ts';
import { routeApi } from '../src/router.ts';
import { rowOf, seedUser, testDb, type TestDb } from './d1.ts';
import { signEnrollment, testKey } from './keys.ts';
import type { Env } from '../src/env.ts';

const ORIGIN = 'https://zesterm.sigx.workers.dev';
const RELAY = 'https://zesterm-relay.sigx.workers.dev';
const NOW = 1_700_000_000_000;

// A real 32-byte seed, so `/api/relay/ticket` can sign for the bearer tests.
const TICKET_SEED = '11'.repeat(32);

function env(db: TestDb): Env {
  return {
    ASSETS: { fetch: async () => new Response('assets') },
    DB: db,
    APP_ORIGIN: ORIGIN,
    GITHUB_CLIENT_ID: 'client-id',
    GITHUB_CLIENT_SECRET: 'client-secret',
    COOKIE_MAC_KEY: 'mac-key',
    RELAY_ORIGIN: RELAY,
    TICKET_SIGNING_KEY: TICKET_SEED,
  };
}

/** A machine: bearer, JSON, and deliberately no origin and no cookie. */
function bearer(path: string, token: string, init?: { method?: string; body?: unknown }): Request {
  const headers: Record<string, string> = { authorization: `Bearer ${token}` };
  const method = init?.method ?? 'GET';
  if (init?.body !== undefined) headers['content-type'] = 'application/json';
  return new Request(`${ORIGIN}${path}`, {
    method,
    headers,
    body: init?.body === undefined ? null : JSON.stringify(init.body),
  });
}

function post(path: string, body: unknown, cookie?: string): Request {
  const headers: Record<string, string> = { origin: ORIGIN, 'content-type': 'application/json' };
  if (cookie !== undefined) headers['cookie'] = cookie;
  return new Request(`${ORIGIN}${path}`, { method: 'POST', headers, body: JSON.stringify(body) });
}

async function signedIn(db: TestDb, userId: string): Promise<string> {
  seedUser(db, userId);
  const { token } = await createSession(db, userId, NOW);
  return `${SESSION_COOKIE}=${token}`;
}

/** Enrol a machine the way the daemon or the app would, and keep its token. */
async function enrolled(
  db: TestDb,
  cookie: string,
  kind: 'host' | 'device',
  seed: number,
): Promise<{ id: string; token: string }> {
  const mintRes = await routeApi(post('/api/enroll/code', { kind }, cookie), env(db), fetch, NOW);
  const { code } = (await mintRes!.json()) as { code: string };
  const key = await testKey(seed);
  const role = kind === 'host' ? 'host' : ('client' as const);
  const res = await routeApi(
    new Request(`${ORIGIN}/api/enroll/claim`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        code,
        hostId: key.id,
        label: `${kind}-${seed}`,
        sig: await signEnrollment({ key, code, label: `${kind}-${seed}`, role }),
      }),
    }),
    env(db),
    fetch,
    NOW,
  );
  assert.equal(res?.status, 200, `enrolling the ${kind} must succeed`);
  const { token } = (await res!.json()) as { token: string };
  assert.ok(looksLikeMachineToken(token));
  return { id: key.id, token };
}

// --- what a token is -------------------------------------------------------

test('a device token lists hosts, with the relay origin beside them', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const host = await enrolled(db, cookie, 'host', 7);
  const device = await enrolled(db, cookie, 'device', 8);

  const res = await routeApi(bearer('/api/hosts', device.token), env(db), fetch, NOW);
  assert.equal(res?.status, 200);
  const body = (await res!.json()) as { hosts: { id: string }[]; relayOrigin: string | null };
  assert.deepEqual(
    body.hosts.map((h) => h.id),
    [host.id],
    'the account list is the fleet screen’s spine',
  );
  assert.equal(
    body.relayOrigin,
    RELAY,
    'a bearer caller has no session for /api/bootstrap, so the relay origin rides here',
  );
  db.close();
});

test('a device token lists devices — the app is an approver', async () => {
  // The widening #190 deferred and then made: the desktop app's fleet screen
  // renders pending rows and vouches from them, with the only credential it
  // has. Sound because the resolve join admits only approved devices.
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const device = await enrolled(db, cookie, 'device', 8);

  const res = await routeApi(bearer('/api/devices', device.token), env(db), fetch, NOW);
  assert.equal(res?.status, 200, 'an approved device reads the approval surface');
  const body = (await res!.json()) as { devices: { id: string; status: string }[] };
  assert.deepEqual(
    body.devices.map((d) => d.id),
    [device.id],
    'the same rows its owner sees',
  );
  db.close();
});

test('a host token may not list hosts or devices', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const host = await enrolled(db, cookie, 'host', 7);

  for (const [path, why] of [
    ['/api/hosts', 'a machine serving shells does not enumerate its owner’s fleet'],
    ['/api/devices', 'and certainly not the approval surface'],
  ] as const) {
    const res = await routeApi(bearer(path, host.token), env(db), fetch, NOW);
    assert.equal(res?.status, 401, why);
  }
  db.close();
});

test('a device token mints a relay ticket, attributed to the device', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const host = await enrolled(db, cookie, 'host', 7);
  const device = await enrolled(db, cookie, 'device', 8);

  const res = await routeApi(
    bearer('/api/relay/ticket', device.token, { method: 'POST', body: { hostId: host.id } }),
    env(db),
    fetch,
    NOW,
  );
  assert.equal(res?.status, 200, 'no origin, no cookie: the token is the whole credential');
  const { ticket } = (await res!.json()) as { ticket: string };
  const payload = JSON.parse(
    Buffer.from(ticket.split('.')[0]!, 'base64url').toString('utf8'),
  ) as { dev: string; host: string };
  assert.equal(
    payload.dev,
    `device:${device.id}`,
    'the ticket names the device, prefixed so a session hash can never be mistaken for it',
  );

  const hostRes = await routeApi(
    bearer('/api/relay/ticket', host.token, { method: 'POST', body: { hostId: host.id } }),
    env(db),
    fetch,
    NOW,
  );
  assert.equal(hostRes?.status, 401, 'a host token must not mint admission to machines');
  db.close();
});

test('a cookie-minted ticket carries the prefixed session hash', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const host = await enrolled(db, cookie, 'host', 7);

  const res = await routeApi(post('/api/relay/ticket', { hostId: host.id }, cookie), env(db), fetch, NOW);
  assert.equal(res?.status, 200);
  const { ticket } = (await res!.json()) as { ticket: string };
  const payload = JSON.parse(
    Buffer.from(ticket.split('.')[0]!, 'base64url').toString('utf8'),
  ) as { dev: string };
  assert.ok(
    payload.dev.startsWith('session:'),
    `both dev spellings are 64 hex underneath, so the prefix is what keeps relay logs honest; got ${payload.dev}`,
  );
  db.close();
});

test('/api/me answers a machine with its principal, and the userId attestations will need', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const device = await enrolled(db, cookie, 'device', 8);

  const res = await routeApi(bearer('/api/me', device.token), env(db), fetch, NOW);
  assert.equal(res?.status, 200);
  assert.deepEqual(await res!.json(), {
    user: null,
    principal: { kind: 'device', id: device.id, userId: 'user-a' },
    // Carried for every machine principal, browsers included: `env()` here is a
    // deployment that has a relay.
    relayOrigin: RELAY,
  });

  // And on a deployment with none, present-and-null rather than absent: a
  // daemon that could not tell "no relay here" from "I failed to read the
  // field" would retry for ever against an account with nothing to give it.
  const { RELAY_ORIGIN: _dropped, ...noRelay } = env(db);
  const bare = await routeApi(bearer('/api/me', device.token), noRelay, fetch, NOW);
  assert.deepEqual(await bare!.json(), {
    user: null,
    principal: { kind: 'device', id: device.id, userId: 'user-a' },
    relayOrigin: null,
  });
  db.close();
});

test('a host token learns its relay origin from /api/me, which is the only route open to it', async () => {
  // #229: a machine that holds a cloud token must be reachable without anyone
  // passing `--relay`, and this is where it learns where to park its control
  // link. `/api/bootstrap` needs a session cookie and `/api/hosts` refuses
  // host tokens on purpose, so widening either to carry one string would cost
  // a real boundary — the host answer below is deliberately the whole of what
  // a serving machine may learn about its account.
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const host = await enrolled(db, cookie, 'host', 9);
  const relay = 'https://zesterm-relay.sigx.workers.dev';

  const res = await routeApi(
    bearer('/api/me', host.token),
    { ...env(db), RELAY_ORIGIN: relay },
    fetch,
    NOW,
  );
  assert.equal(res?.status, 200);
  assert.deepEqual(await res!.json(), {
    user: null,
    principal: { kind: 'host', id: host.id, userId: 'user-a' },
    relayOrigin: relay,
  });

  // And it still cannot enumerate the fleet: the field is not a widening.
  const hosts = await routeApi(
    bearer('/api/hosts', host.token),
    { ...env(db), RELAY_ORIGIN: relay },
    fetch,
    NOW,
  );
  assert.equal(hosts?.status, 401, 'a host token has no business listing its owner\'s machines');
  db.close();
});

// --- what a token must not do ----------------------------------------------

test('a bad bearer token never falls back to the cookie', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  await enrolled(db, cookie, 'host', 7);

  // Cookie present and perfectly valid; Authorization present and junk. If
  // this answers 200 the ORIGINLESS invariant is gone: a request that skipped
  // the Origin check was authorized by ambient credentials.
  const req = new Request(`${ORIGIN}/api/hosts`, {
    headers: { authorization: 'Bearer zt1_not-a-real-token', cookie },
  });
  const res = await routeApi(req, env(db), fetch, NOW);
  assert.equal(
    res?.status,
    401,
    'an Authorization header means the token alone decides — a bad one is a refusal, not a fallback',
  );
  db.close();
});

test('revoking the principal kills its token with no second write', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const device = await enrolled(db, cookie, 'device', 8);

  const before = await routeApi(bearer('/api/hosts', device.token), env(db), fetch, NOW);
  assert.equal(before?.status, 200);

  const revoke = await routeApi(
    post(`/api/devices/${device.id}/revoke`, {}, cookie),
    env(db),
    fetch,
    NOW,
  );
  assert.equal(revoke?.status, 200);

  const after = await routeApi(bearer('/api/hosts', device.token), env(db), fetch, NOW);
  assert.equal(
    after?.status,
    401,
    'liveness is a JOIN against the principal row, so the revoke route already killed this token',
  );

  const row = rowOf<{ revoked_at: number | null }>(
    db,
    `SELECT revoked_at FROM machine_tokens WHERE principal_id = ?`,
    device.id,
  );
  assert.deepEqual(
    row,
    { revoked_at: null },
    'and deliberately not by writing revoked_at here — the join is the mechanism',
  );
  db.close();
});

test('a pending device’s token resolves to nothing', async () => {
  // Approval is in the resolver's JOIN beside the liveness conditions, so a
  // device that is not `approved` has no working credential — however its
  // holder came by a token string, and with no second write anywhere when a
  // row is demoted. Enrolment mints the token while the row is approved (a
  // typed code is the account's explicit act); the flip below is the demotion.
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const device = await enrolled(db, cookie, 'device', 8);

  db.raw.prepare(`UPDATE devices SET status = 'pending' WHERE id = ?`).run(device.id);
  const res = await routeApi(bearer('/api/hosts', device.token), env(db), fetch, NOW);
  assert.equal(res?.status, 401, 'pending means awaiting approval, and a credential does not wait');

  db.raw.prepare(`UPDATE devices SET status = 'approved' WHERE id = ?`).run(device.id);
  const approved = await routeApi(bearer('/api/hosts', device.token), env(db), fetch, NOW);
  assert.equal(
    approved?.status,
    200,
    'the token itself was never revoked — approval alone is what gates it',
  );
  db.close();
});

test('re-enrolling rotates: at most one live token per principal', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const first = await enrolled(db, cookie, 'host', 7);
  const again = await enrolled(db, cookie, 'host', 7);
  assert.notEqual(first.token, again.token);

  const live = rowOf<{ n: number }>(
    db,
    `SELECT COUNT(*) AS n FROM machine_tokens WHERE principal_id = ? AND revoked_at IS NULL`,
    first.id,
  );
  assert.deepEqual(live, { n: 1 }, 'a machine with three live tokens is three leaks and no capability');

  const old = await routeApi(
    bearer('/api/relay/ticket', first.token, { method: 'POST', body: { hostId: first.id } }),
    env(db),
    fetch,
    NOW,
  );
  assert.equal(old?.status, 401, 'the superseded token is dead, not merely redundant');
  db.close();
});

test('an expired token is refused, and use slides the expiry', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const device = await enrolled(db, cookie, 'device', 8);

  // Two hours on: past the hour granularity, so this resolve refreshes.
  const later = NOW + 2 * 60 * 60 * 1000;
  const res = await routeApi(bearer('/api/hosts', device.token), env(db), fetch, later);
  assert.equal(res?.status, 200);

  const row = rowOf<{ expires_at: number; last_seen_at: number }>(
    db,
    `SELECT expires_at, last_seen_at FROM machine_tokens WHERE principal_id = ?`,
    device.id,
  );
  assert.deepEqual(
    row,
    { expires_at: later + MACHINE_TOKEN_TTL_MS, last_seen_at: later },
    'a machine that phones home at all keeps its credential alive',
  );

  const device_row = rowOf<{ last_seen_at: number | null }>(
    db,
    `SELECT last_seen_at FROM devices WHERE id = ?`,
    device.id,
  );
  assert.deepEqual(
    device_row,
    { last_seen_at: later },
    'the same resolve is the only writer devices.last_seen_at has',
  );

  const dead = await routeApi(
    bearer('/api/hosts', device.token),
    env(db),
    fetch,
    later + MACHINE_TOKEN_TTL_MS + 1,
  );
  assert.equal(dead?.status, 401, 'ninety days dark means re-enrolling, which is a person’s act');
  db.close();
});

test('a non-Bearer Authorization header widens nothing', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const host = await enrolled(db, cookie, 'host', 7);

  // `Basic` from some proxy, on a BEARER path, as a POST with no Origin: the
  // exemption is keyed on the Bearer shape, so the full CSRF rule applies and
  // this dies at the origin check — before any resolver runs at all.
  const res = await routeApi(
    new Request(`${ORIGIN}/api/relay/ticket`, {
      method: 'POST',
      headers: { authorization: 'Basic dXNlcjpwdw==', 'content-type': 'application/json', cookie },
      body: JSON.stringify({ hostId: host.id }),
    }),
    env(db),
    fetch,
    NOW,
  );
  assert.equal(
    res?.status,
    403,
    'an Authorization header that is not Bearer must not drop the Origin half of the rule',
  );
  db.close();
});

test('a bearer request on a non-bearer route keeps the full CSRF rule', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const device = await enrolled(db, cookie, 'device', 8);

  // POST with a token but no Origin, to a route outside BEARER: the origin
  // half of the rule must still apply, or membership of BEARER stops being
  // the visible act the router says it is.
  const res = await routeApi(
    bearer(`/api/devices/${device.id}/revoke`, device.token, { method: 'POST', body: {} }),
    env(db),
    fetch,
    NOW,
  );
  assert.equal(res?.status, 403, 'only paths in BEARER may drop the Origin check');
  db.close();
});

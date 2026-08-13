/**
 * `POST /api/devices/:id/approve` and `GET /api/attestations`, end to end
 * against the real migrations and real Ed25519.
 *
 * Most of what is asserted is what the approve route must refuse — an
 * attestation for another account, about another device, by an unapproved or
 * self-vouching key, outside its window, or wearing a signature its named
 * approver never made — and Model B's other half: that revoking a device
 * revokes its attestations in both directions and lands it on the served
 * revocation list.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { ATTESTATION_TTL_MS, SESSION_COOKIE } from '@zesterm/cloud-shared';

import { createSession } from '../src/db/sessions.ts';
import { ATTESTATION_IAT_SKEW_MS } from '../src/api/attest.ts';
import { routeApi } from '../src/router.ts';
import { rowOf, seedUser, testDb, type TestDb } from './d1.ts';
import { attestationBlob, signEnrollment, signRegistration, testKey, type TestKey } from './keys.ts';
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

function post(path: string, body: unknown, cookie?: string): Request {
  const headers: Record<string, string> = { origin: ORIGIN, 'content-type': 'application/json' };
  if (cookie !== undefined) headers['cookie'] = cookie;
  return new Request(`${ORIGIN}${path}`, { method: 'POST', headers, body: JSON.stringify(body) });
}

/** A machine: bearer, JSON, and deliberately no origin and no cookie. */
function bearer(path: string, token: string, init?: { method?: string; body?: unknown }): Request {
  const headers: Record<string, string> = { authorization: `Bearer ${token}` };
  if (init?.body !== undefined) headers['content-type'] = 'application/json';
  return new Request(`${ORIGIN}${path}`, {
    method: init?.method ?? 'GET',
    headers,
    body: init?.body === undefined ? null : JSON.stringify(init.body),
  });
}

async function signedIn(db: TestDb, userId: string): Promise<string> {
  seedUser(db, userId);
  const { token } = await createSession(db, userId, NOW);
  return `${SESSION_COOKIE}=${token}`;
}

/** Register a browser key over the cookie path. First one is born approved. */
async function registered(
  db: TestDb,
  cookie: string,
  account: string,
  seed: number,
  label: string,
): Promise<TestKey> {
  const key = await testKey(seed);
  const res = await routeApi(
    post(
      '/api/devices/register',
      { deviceId: key.id, label, sig: await signRegistration({ key, account, label }) },
      cookie,
    ),
    env(db),
    fetch,
    NOW,
  );
  assert.equal(res?.status, 200, `registering ${label} must succeed`);
  return key;
}

/** Enrol a principal by code, keeping its bearer token. */
async function enrolled(
  db: TestDb,
  cookie: string,
  kind: 'host' | 'device',
  seed: number,
): Promise<{ key: TestKey; token: string }> {
  const mintRes = await routeApi(post('/api/enroll/code', { kind }, cookie), env(db), fetch, NOW);
  const { code } = (await mintRes!.json()) as { code: string };
  const key = await testKey(seed);
  const res = await routeApi(
    new Request(`${ORIGIN}/api/enroll/claim`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        code,
        hostId: key.id,
        label: `${kind}-${seed}`,
        sig: await signEnrollment({ key, code, label: `${kind}-${seed}`, role: kind === 'host' ? 'host' : 'client' }),
      }),
    }),
    env(db),
    fetch,
    NOW,
  );
  assert.equal(res?.status, 200, `enrolling the ${kind} must succeed`);
  const { token } = (await res!.json()) as { token: string };
  return { key, token };
}

/** An account with an approved browser (A) and a pending one (B). */
async function twoBrowsers(db: TestDb) {
  const cookie = await signedIn(db, 'user-a');
  const a = await registered(db, cookie, 'user-a', 7, 'first browser');
  const b = await registered(db, cookie, 'user-a', 8, 'second browser');
  assert.deepEqual(
    rowOf(db, `SELECT status FROM devices WHERE id = ?`, b.id),
    { status: 'pending' },
    'the setup depends on B being pending, or approval proves nothing',
  );
  return { cookie, a, b };
}

const WINDOW = { iat: NOW, exp: NOW + ATTESTATION_TTL_MS };

// --- the happy paths -------------------------------------------------------

test('an approved browser vouches for a pending one, and the voucher is served back', async () => {
  const db = testDb();
  const { cookie, a, b } = await twoBrowsers(db);
  const blob = await attestationBlob(a, {
    account: 'user-a',
    device: b.id,
    label: 'second browser',
    ...WINDOW,
  });

  const res = await routeApi(post(`/api/devices/${b.id}/approve`, { attestation: blob }, cookie), env(db), fetch, NOW);
  assert.equal(res?.status, 200);
  const { device } = (await res!.json()) as { device: { status: string } };
  assert.equal(device.status, 'approved', 'the row the UI refetches must already say so');

  assert.deepEqual(
    rowOf(db, `SELECT status, approved_at, approved_by FROM devices WHERE id = ?`, b.id),
    { status: 'approved', approved_at: NOW, approved_by: a.id },
    'approved_by names the attestor — NULL is reserved for code-or-bootstrap',
  );
  assert.deepEqual(
    rowOf(db, `SELECT blob, user_id, iat, exp, revoked_at FROM device_attestations WHERE device_id = ?`, b.id),
    { blob, user_id: 'user-a', iat: NOW, exp: NOW + ATTESTATION_TTL_MS, revoked_at: null },
    'the blob is stored VERBATIM — daemons verify the same bytes the route verified',
  );

  const list = await routeApi(
    new Request(`${ORIGIN}/api/attestations`, { headers: { cookie } }),
    env(db),
    fetch,
    NOW,
  );
  assert.equal(list?.status, 200);
  assert.deepEqual(
    await list!.json(),
    { attestations: [blob], revoked: [] },
    'what daemons pull is the voucher as signed, plus an empty revocation list',
  );
  db.close();
});

test('re-approving renews: one row per (device, approver), timestamps replaced', async () => {
  const db = testDb();
  const { cookie, a, b } = await twoBrowsers(db);
  const first = await attestationBlob(a, { account: 'user-a', device: b.id, label: 'second browser', ...WINDOW });
  await routeApi(post(`/api/devices/${b.id}/approve`, { attestation: first }, cookie), env(db), fetch, NOW);

  const later = NOW + 60_000;
  const renewal = await attestationBlob(a, {
    account: 'user-a',
    device: b.id,
    label: 'second browser',
    iat: later,
    exp: later + ATTESTATION_TTL_MS,
  });
  const res = await routeApi(post(`/api/devices/${b.id}/approve`, { attestation: renewal }, cookie), env(db), fetch, later);
  assert.equal(res?.status, 200, 'a re-vouch is an idempotent renewal, not a conflict');

  assert.deepEqual(
    rowOf(db, `SELECT COUNT(*) AS n FROM device_attestations WHERE device_id = ?`, b.id),
    { n: 1 },
    'the PK (device_id, by_device) makes renewal replace rather than accumulate',
  );
  assert.deepEqual(
    rowOf(db, `SELECT blob, iat FROM device_attestations WHERE device_id = ?`, b.id),
    { blob: renewal, iat: later },
    'the served voucher is the fresh statement',
  );
  assert.deepEqual(
    rowOf(db, `SELECT approved_at, approved_by FROM devices WHERE id = ?`, b.id),
    { approved_at: NOW, approved_by: a.id },
    'first-approval history does not move on renewal',
  );
  db.close();
});

test('the desktop app approves with its bearer token — but only as itself', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  // Enrolled by code, so the desktop is born approved and the browser that
  // registers after it is pending — no bootstrap in the way.
  const desktop = await enrolled(db, cookie, 'device', 9);
  const browserA = await registered(db, cookie, 'user-a', 7, 'first browser');
  assert.deepEqual(rowOf(db, `SELECT status FROM devices WHERE id = ?`, browserA.id), { status: 'pending' });

  // Signed by the desktop, submitted by the desktop: the honest path.
  const own = await attestationBlob(desktop.key, {
    account: 'user-a',
    device: browserA.id,
    label: 'first browser',
    ...WINDOW,
  });
  const res = await routeApi(
    bearer(`/api/devices/${browserA.id}/approve`, desktop.token, { method: 'POST', body: { attestation: own } }),
    env(db),
    fetch,
    NOW,
  );
  assert.equal(res?.status, 200, 'no origin, no cookie: the token plus the signature is the whole credential');

  // A voucher some OTHER approved device signed — browser A, just approved —
  // submitted through the desktop's token: refused, or one leaked token could
  // submit captured vouchers under every approver's name.
  const browserB = await registered(db, cookie, 'user-a', 8, 'second browser');
  const byBrowserA = await attestationBlob(browserA, {
    account: 'user-a',
    device: browserB.id,
    label: 'second browser',
    ...WINDOW,
  });
  const refused = await routeApi(
    bearer(`/api/devices/${browserB.id}/approve`, desktop.token, { method: 'POST', body: { attestation: byBrowserA } }),
    env(db),
    fetch,
    NOW,
  );
  assert.equal(refused?.status, 403, 'a bearer principal may only submit vouchers it signed itself');
  db.close();
});

// --- what the route refuses ------------------------------------------------

test('every dishonest voucher is refused, and refusal writes nothing', async () => {
  const db = testDb();
  const { cookie, a, b } = await twoBrowsers(db);
  const fields = { account: 'user-a', device: b.id, label: 'second browser', ...WINDOW };
  const send = (blob: string, path = `/api/devices/${b.id}/approve`) =>
    routeApi(post(path, { attestation: blob }, cookie), env(db), fetch, NOW);

  // Another account inside the signed bytes: a voucher for one account must
  // not admit the same key to another.
  const wrongAccount = await attestationBlob(a, { ...fields, account: 'user-b' });
  assert.equal((await send(wrongAccount))?.status, 400);

  // The statement is about a different device than the route names.
  const other = await testKey(5);
  const aboutOther = await attestationBlob(a, { ...fields, device: other.id });
  assert.equal((await send(aboutOther))?.status, 400, 'the URL and the signed bytes must agree');

  // About a key the account has never seen: same 404 as a revoke, because the
  // ids are public keys and strangers must not be enumerable.
  const unknown = await attestationBlob(a, { ...fields, device: other.id });
  assert.equal((await send(unknown, `/api/devices/${other.id}/approve`))?.status, 404);

  // Self-vouching: pending exists because the session alone could not promote
  // a key, and a key vouching for itself is the same act wearing a signature.
  const selfVouch = await attestationBlob(b, { ...fields, by: b.id });
  assert.equal((await send(selfVouch))?.status, 400);

  // A pending approver: vouching is what approval GRANTS, not a way to get it.
  const c = await registered(db, cookie, 'user-a', 6, 'third browser');
  const byPending = await attestationBlob(c, { ...fields });
  assert.equal((await send(byPending))?.status, 400);

  // A window longer than the TTL, a window that never opens, and an iat
  // outside clock skew: each refused before any signature work.
  assert.equal(
    (await send(await attestationBlob(a, { ...fields, exp: NOW + ATTESTATION_TTL_MS + 1 })))?.status,
    400,
    'the TTL is the server’s, not the client’s to stretch',
  );
  assert.equal((await send(await attestationBlob(a, { ...fields, exp: fields.iat })))?.status, 400);
  assert.equal(
    (await send(
      await attestationBlob(a, {
        ...fields,
        iat: NOW - ATTESTATION_IAT_SKEW_MS - 1,
        exp: NOW + ATTESTATION_TTL_MS - ATTESTATION_IAT_SKEW_MS - 1,
      }),
    ))?.status,
    400,
    'a stale iat is a replayed approval, not a fresh one',
  );

  // A signature the named approver never made — valid Ed25519, wrong key.
  const impostor = await testKey(4);
  const forged = await attestationBlob(impostor, { ...fields, by: a.id });
  const res = await send(forged);
  assert.equal(res?.status, 400);
  assert.deepEqual(await res!.json(), { error: 'bad_signature' });

  // And after all of it: nothing entered the account.
  assert.deepEqual(
    rowOf(db, `SELECT status FROM devices WHERE id = ?`, b.id),
    { status: 'pending' },
    'every refusal above must leave the device exactly as it was',
  );
  assert.deepEqual(
    rowOf(db, `SELECT COUNT(*) AS n FROM device_attestations`),
    { n: 0 },
    'a refused voucher must not enter the distribution channel',
  );
  db.close();
});

test('a revoked approver cannot vouch', async () => {
  const db = testDb();
  const { cookie, a, b } = await twoBrowsers(db);
  await routeApi(post(`/api/devices/${a.id}/revoke`, {}, cookie), env(db), fetch, NOW);

  const blob = await attestationBlob(a, { account: 'user-a', device: b.id, label: 'second browser', ...WINDOW });
  const res = await routeApi(post(`/api/devices/${b.id}/approve`, { attestation: blob }, cookie), env(db), fetch, NOW);
  assert.equal(res?.status, 400, 'revocation is a positive statement, and a revoked key’s word is worth nothing');
  db.close();
});

test('approval requires a principal, and a host token is not one here', async () => {
  const db = testDb();
  const { cookie, a, b } = await twoBrowsers(db);
  const host = await enrolled(db, cookie, 'host', 10);
  const blob = await attestationBlob(a, { account: 'user-a', device: b.id, label: 'second browser', ...WINDOW });

  const nobody = await routeApi(post(`/api/devices/${b.id}/approve`, { attestation: blob }), env(db), fetch, NOW);
  assert.equal(nobody?.status, 401);

  const asHost = await routeApi(
    bearer(`/api/devices/${b.id}/approve`, host.token, { method: 'POST', body: { attestation: blob } }),
    env(db),
    fetch,
    NOW,
  );
  assert.equal(
    asHost?.status,
    401,
    'a machine serving shells must not be the doorman for the account’s devices',
  );
  db.close();
});

test('the cookie path keeps the full CSRF rule; the bearer path is exempt by the token', async () => {
  const db = testDb();
  const { cookie, a, b } = await twoBrowsers(db);
  const blob = await attestationBlob(a, { account: 'user-a', device: b.id, label: 'second browser', ...WINDOW });

  const noOrigin = await routeApi(
    new Request(`${ORIGIN}/api/devices/${b.id}/approve`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', cookie },
      body: JSON.stringify({ attestation: blob }),
    }),
    env(db),
    fetch,
    NOW,
  );
  assert.equal(noOrigin?.status, 403, 'a cross-site page with the victim’s cookie dies at the origin check');

  const method = await routeApi(new Request(`${ORIGIN}/api/devices/${b.id}/approve`), env(db), fetch, NOW);
  assert.equal(method?.status, 405, 'approve is POST-only');
  db.close();
});

// --- the served set and Model B --------------------------------------------

test('daemons read the set with a host token; a device token may not', async () => {
  const db = testDb();
  const { cookie, a, b } = await twoBrowsers(db);
  const host = await enrolled(db, cookie, 'host', 10);
  const desktop = await enrolled(db, cookie, 'device', 9);
  const blob = await attestationBlob(a, { account: 'user-a', device: b.id, label: 'second browser', ...WINDOW });
  await routeApi(post(`/api/devices/${b.id}/approve`, { attestation: blob }, cookie), env(db), fetch, NOW);

  const asHost = await routeApi(bearer('/api/attestations', host.token), env(db), fetch, NOW);
  assert.equal(asHost?.status, 200, 'the daemon is who the list exists for');
  assert.deepEqual(await asHost!.json(), { attestations: [blob], revoked: [] });

  const asDevice = await routeApi(bearer('/api/attestations', desktop.token), env(db), fetch, NOW);
  assert.equal(asDevice?.status, 401, 'a leaked device token must not enumerate the account’s vouchers');
  db.close();
});

test('revoking a device revokes its attestations both ways, and lists it as revoked', async () => {
  const db = testDb();
  const { cookie, a, b } = await twoBrowsers(db);
  // A vouches for B; then B (now approved) vouches for C — so B appears as
  // subject of one attestation and approver of another.
  const vouchB = await attestationBlob(a, { account: 'user-a', device: b.id, label: 'second browser', ...WINDOW });
  await routeApi(post(`/api/devices/${b.id}/approve`, { attestation: vouchB }, cookie), env(db), fetch, NOW);
  const c = await registered(db, cookie, 'user-a', 6, 'third browser');
  const vouchC = await attestationBlob(b, { account: 'user-a', device: c.id, label: 'third browser', ...WINDOW });
  await routeApi(post(`/api/devices/${c.id}/approve`, { attestation: vouchC }, cookie), env(db), fetch, NOW);

  await routeApi(post(`/api/devices/${b.id}/revoke`, {}, cookie), env(db), fetch, NOW + 1);

  const list = await routeApi(
    new Request(`${ORIGIN}/api/attestations`, { headers: { cookie } }),
    env(db),
    fetch,
    NOW + 2,
  );
  assert.deepEqual(
    await list!.json(),
    { attestations: [], revoked: [b.id] },
    'the voucher FOR b and the voucher BY b both stop being served — an approver ' +
      'thrown out of the account must not keep introducing devices — and b lands ' +
      'on the list that un-introduces it from daemons that already recorded it',
  );
  assert.deepEqual(
    rowOf(db, `SELECT COUNT(*) AS n FROM device_attestations WHERE revoked_at IS NOT NULL`),
    { n: 2 },
    'revoked rather than deleted, so "why can this device not attach" keeps an answer',
  );
  db.close();
});

test('an expired attestation is not served, with no write needed', async () => {
  const db = testDb();
  const { cookie, a, b } = await twoBrowsers(db);
  const shortLived = await attestationBlob(a, {
    account: 'user-a',
    device: b.id,
    label: 'second browser',
    iat: NOW,
    exp: NOW + 1_000,
  });
  await routeApi(post(`/api/devices/${b.id}/approve`, { attestation: shortLived }, cookie), env(db), fetch, NOW);

  const live = await routeApi(new Request(`${ORIGIN}/api/attestations`, { headers: { cookie } }), env(db), fetch, NOW + 500);
  assert.deepEqual(((await live!.json()) as { attestations: string[] }).attestations, [shortLived]);

  const later = await routeApi(new Request(`${ORIGIN}/api/attestations`, { headers: { cookie } }), env(db), fetch, NOW + 1_000);
  assert.deepEqual(
    ((await later!.json()) as { attestations: string[] }).attestations,
    [],
    'exp is exclusive and the daemons would refuse the bytes anyway; serving them only spends their verify',
  );
  db.close();
});

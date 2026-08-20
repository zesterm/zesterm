/**
 * `POST /api/devices/register`, end to end against the real migration and
 * real Ed25519.
 *
 * Most of what is asserted is the two policies the route exists to carry: the
 * bootstrap rule (an account's first device is born approved, every later
 * registration is born pending), and what a pending row must *not* be able to
 * do — refresh itself into approval, or exceed the per-account lid. The
 * account binding in the signature gets its own test because it is the one
 * property no status can compensate for.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { SESSION_COOKIE } from '@zesterm/cloud-shared';

import { createSession } from '../src/db/sessions.ts';
import { MAX_PENDING_DEVICES } from '../src/api/devices.ts';
import { routeApi } from '../src/router.ts';
import { rowOf, seedUser, testDb, type TestDb } from './d1.ts';
import { signEnrollment, signRegistration, testKey } from './keys.ts';
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

/** A signed-in browser: our origin, JSON, and a session cookie. */
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

/** A complete, valid registration body for `key` under `account`. */
async function bodyFor(seed: number, account: string, label: string) {
  const key = await testKey(seed);
  return {
    key,
    body: {
      deviceId: key.id,
      label,
      sig: await signRegistration({ key, account, label }),
    },
  };
}

async function register(
  db: TestDb,
  cookie: string | undefined,
  body: unknown,
  now = NOW,
): Promise<Response | null> {
  return routeApi(post('/api/devices/register', body, cookie), env(db), fetch, now);
}

// --- the bootstrap rule ----------------------------------------------------

test('an account’s first device is born approved, by dint of the session', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const { key, body } = await bodyFor(7, 'user-a', 'this browser');

  const res = await register(db, cookie, body);
  assert.equal(res?.status, 200);
  const answer = (await res!.json()) as { device: unknown };
  assert.deepEqual(
    answer.device,
    {
      id: key.id,
      label: 'this browser',
      kind: 'browser',
      extractable: true,
      status: 'approved',
      enrolledAt: NOW,
      lastSeenAt: null,
    },
    'with zero live approved devices there is no incumbent to approve this one, so pending would be forever',
  );
  assert.deepEqual(
    rowOf(db, `SELECT status, approved_by FROM devices WHERE id = ?`, key.id),
    { status: 'approved', approved_by: null },
    'approved_by NULL means code-or-bootstrap, never an attestor',
  );
  db.close();
});

test('every registration after the first is born pending', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const first = await bodyFor(7, 'user-a', 'first browser');
  const second = await bodyFor(8, 'user-a', 'second browser');
  await register(db, cookie, first.body);

  const res = await register(db, cookie, second.body);
  assert.equal(res?.status, 200);
  const { device } = (await res!.json()) as { device: { status: string } };
  assert.equal(
    device.status,
    'pending',
    'an approved device exists to do the approving, so the session alone no longer decides',
  );

  // And the pending row is on the devices screen — that is where its owner
  // learns it is waiting.
  const list = await routeApi(
    new Request(`${ORIGIN}/api/devices`, { headers: { cookie } }),
    env(db),
    fetch,
    NOW,
  );
  const { devices } = (await list!.json()) as { devices: { id: string; status: string }[] };
  assert.deepEqual(
    devices.map((d) => d.status).sort(),
    ['approved', 'pending'],
    'pending rows list beside approved ones rather than being hidden',
  );
  db.close();
});

test('a code-enrolled device is born approved: the typed code was the explicit act', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const mintRes = await routeApi(post('/api/enroll/code', { kind: 'device' }, cookie), env(db), fetch, NOW);
  const { code } = (await mintRes!.json()) as { code: string };
  const key = await testKey(9);

  const res = await routeApi(
    new Request(`${ORIGIN}/api/enroll/claim`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        code,
        hostId: key.id,
        label: 'andy-phone',
        sig: await signEnrollment({ key, code, label: 'andy-phone', role: 'client' }),
      }),
    }),
    env(db),
    fetch,
    NOW,
  );
  assert.equal(res?.status, 200);
  assert.deepEqual(
    rowOf(db, `SELECT status FROM devices WHERE id = ?`, key.id),
    { status: 'approved' },
    'the schema DEFAULT does this for a fresh key; the conflict branch says it out loud (#372)',
  );
  db.close();
});

// --- what the signature binds ----------------------------------------------

test('a captured registration does not replay under another account', async () => {
  // The `account` inside the signed bytes is the whole reason the format
  // exists: the same body, byte for byte, presented with a different session,
  // verifies against that session's user id and fails.
  const db = testDb();
  const mine = await signedIn(db, 'user-a');
  const theirs = await signedIn(db, 'user-b');
  const { key, body } = await bodyFor(7, 'user-a', 'this browser');

  const replay = await register(db, theirs, body);
  assert.equal(replay?.status, 400);
  assert.deepEqual(await replay!.json(), { error: 'bad_signature' });
  assert.equal(
    rowOf(db, `SELECT 1 AS x FROM devices WHERE id = ?`, key.id),
    null,
    'a refused registration writes nothing',
  );

  const genuine = await register(db, mine, body);
  assert.equal(genuine?.status, 200, 'the account the signature names can still register it');
  db.close();
});

test('a wrong signature registers nothing', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const real = await testKey(7);
  const impostor = await testKey(8);

  const res = await register(db, cookie, {
    deviceId: real.id,
    label: 'this browser',
    // A perfectly valid signature -- over the same bytes, by the wrong key.
    sig: await signRegistration({ key: impostor, account: 'user-a', label: 'this browser' }),
  });
  assert.equal(res?.status, 400);
  assert.deepEqual(await res!.json(), { error: 'bad_signature' });
  assert.equal(
    (rowOf(db, `SELECT COUNT(*) AS c FROM devices`) as { c: number }).c,
    0,
    'holding a session is not holding the key',
  );
  db.close();
});

// --- incumbents ------------------------------------------------------------

test('re-registering the same key refreshes the label and touches nothing else', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const first = await bodyFor(7, 'user-a', 'first browser');
  await register(db, cookie, first.body);
  const second = await bodyFor(8, 'user-a', 'second browser');
  await register(db, cookie, second.body);

  // Re-register the pending one under a new label. Status must not move: a
  // label refresh happens on every visit, and a refresh that could flip
  // status would make the column mean "whatever the last request said".
  const renamed = await bodyFor(8, 'user-a', 'renamed browser');
  const res = await register(db, cookie, renamed.body, NOW + 60_000);
  assert.equal(res?.status, 200);
  const { device } = (await res!.json()) as { device: { label: string; status: string } };
  assert.equal(device.label, 'renamed browser');
  assert.equal(device.status, 'pending', 'idempotent re-registration is not approval');
  assert.deepEqual(
    rowOf(db, `SELECT enrolled_at FROM devices WHERE id = ?`, renamed.key.id),
    { enrolled_at: NOW },
    'the same key re-proving itself keeps the date it joined',
  );
  db.close();
});

test('another account’s key, and a revoked key, are both already_enrolled', async () => {
  const db = testDb();
  const mine = await signedIn(db, 'user-a');
  const theirs = await signedIn(db, 'user-b');

  // user-a registers key 7; user-b, who legitimately holds a session, signs
  // for the same key under their own account. The signature verifies -- the
  // impostor scenario here is a *key* trying to be in two accounts.
  const first = await bodyFor(7, 'user-a', 'mine');
  await register(db, mine, first.body);
  const stolen = await bodyFor(7, 'user-b', 'not-yours');
  const res = await register(db, theirs, stolen.body);
  assert.equal(res?.status, 409);
  assert.deepEqual(await res!.json(), { error: 'already_enrolled', detail: 'other_account' });
  assert.deepEqual(
    rowOf(db, `SELECT user_id, label FROM devices WHERE id = ?`, first.key.id),
    { user_id: 'user-a', label: 'mine' },
    'holding a key is not a claim on somebody else’s row',
  );

  // Revoke it, then try to register it again as its own account: revocation
  // is a positive statement, and a key that could re-register itself has
  // un-revoked itself.
  await routeApi(post(`/api/devices/${first.key.id}/revoke`, {}, mine), env(db), fetch, NOW);
  const again = await bodyFor(7, 'user-a', 'mine');
  const refused = await register(db, mine, again.body, NOW + 1);
  assert.equal(refused?.status, 409);
  assert.deepEqual(await refused!.json(), { error: 'already_enrolled', detail: 'revoked' });
  db.close();
});

// --- the pending lid -------------------------------------------------------

test('pending rows are bounded per account, and the bound spares the bootstrap', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');

  // Seed 1 is the bootstrap device (born approved); 32 more go pending.
  // Registration is the one write a session performs without a person's
  // explicit act, so unbounded it is a table any script can grow forever.
  for (let seed = 1; seed <= 1 + MAX_PENDING_DEVICES; seed++) {
    const { body } = await bodyFor(seed, 'user-a', `browser ${seed}`);
    const res = await register(db, cookie, body);
    assert.equal(res?.status, 200, `registration ${seed} is within the lid`);
  }

  const { body } = await bodyFor(60, 'user-a', 'one too many');
  const res = await register(db, cookie, body);
  assert.equal(res?.status, 429);
  assert.deepEqual(await res!.json(), { error: 'too_many_pending' });
  assert.equal(
    (rowOf(db, `SELECT COUNT(*) AS c FROM devices WHERE status = 'pending'`) as { c: number }).c,
    MAX_PENDING_DEVICES,
    'the refused registration wrote nothing',
  );

  // Revoking a pending row frees its slot: the lid counts live rows, so the
  // owner cleaning up is what un-sticks a stuffed account.
  const victim = rowOf(db, `SELECT id FROM devices WHERE status = 'pending' LIMIT 1`) as {
    id: string;
  };
  await routeApi(post(`/api/devices/${victim.id}/revoke`, {}, cookie), env(db), fetch, NOW);
  const freed = await register(db, cookie, body, NOW + 1);
  assert.equal(freed?.status, 200, 'a revoked pending row no longer counts against the lid');
  db.close();
});

// --- shape and posture -----------------------------------------------------

test('registration requires a session', async () => {
  const db = testDb();
  const { body } = await bodyFor(7, 'user-a', 'this browser');
  const res = await register(db, undefined, body);
  assert.equal(res?.status, 401, 'the session is what the signature binds to; without one there is no account');
  db.close();
});

test('a malformed registration is refused on shape, after only the session read', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const { key, body: ok } = await bodyFor(7, 'user-a', 'this browser');

  const cases: Array<[string, Record<string, unknown>]> = [
    ['a key that is not 32 bytes of hex', { ...ok, deviceId: `${key.id}ff` }],
    [
      'an uppercase key -- a different primary key, not the same device',
      { ...ok, deviceId: key.id.toUpperCase() },
    ],
    ['an empty label', { ...ok, label: '' }],
    ['a label carrying control characters', { ...ok, label: 'this[2Jbrowser' }],
    ['a kind nobody renders', { ...ok, kind: 'toaster' }],
    ['an extractable that is not a boolean', { ...ok, extractable: 'yes' }],
    ['a signature that is not 64 bytes of hex', { ...ok, sig: 'a'.repeat(127) }],
  ];

  for (const [why, body] of cases) {
    // One query is the session resolve, which necessarily precedes shape --
    // the shape errors name fields to a caller, and only a signed-in one may
    // hear them. Anything beyond that single read means a guard leaked.
    let queries = 0;
    const counting = { ...db, prepare: (sql: string) => (queries++, db.prepare(sql)) };
    const res = await routeApi(post('/api/devices/register', body, cookie), env(counting), fetch, NOW);
    assert.equal(res?.status, 400, why);
    assert.ok(queries <= 1, `${why}: the body was rejected only after ${queries} queries`);
  }

  // The control, last: unmutated, the same body registers -- so a 400 above
  // can only have come from the field that was changed.
  const good = await register(db, cookie, ok);
  assert.equal(good?.status, 200, 'the unmutated body must actually register');
  db.close();
});

test('registration keeps the full CSRF rule: no Origin, no write', async () => {
  // Never ORIGINLESS: this route reads the session cookie, so it has exactly
  // the ambient authority the Origin check exists to protect.
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const { body } = await bodyFor(7, 'user-a', 'this browser');
  const res = await routeApi(
    new Request(`${ORIGIN}/api/devices/register`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', cookie },
      body: JSON.stringify(body),
    }),
    env(db),
    fetch,
    NOW,
  );
  assert.equal(res?.status, 403, 'a cross-site page with the victim’s cookie must die at the origin check');
  db.close();
});

test('the register route is POST-only', async () => {
  const db = testDb();
  const res = await routeApi(new Request(`${ORIGIN}/api/devices/register`), env(db), fetch, NOW);
  assert.equal(res?.status, 405);
  db.close();
});

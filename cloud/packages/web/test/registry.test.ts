/**
 * `/api/hosts`, `/api/devices` and the two revoke routes.
 *
 * The whole surface is "the caller's own or nothing", so what is asserted here
 * is mostly the negative: that another account's machine is invisible, that it
 * is also un-revokable, and that neither answer says whether it exists.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { CONTROL_SEEN_BOUND_MS, SESSION_COOKIE } from '@zesterm/cloud-shared';

import { createEnrollCode, enrolHost, spendEnrollCode } from '../src/db/registry.ts';
import { createMachineToken } from '../src/db/machine-tokens.ts';
import { createSession } from '../src/db/sessions.ts';
import { routeApi } from '../src/router.ts';
import { rowOf, seedUser, testDb, type TestDb } from './d1.ts';
import type { Env } from '../src/env.ts';

const ORIGIN = 'https://zesterm.sigx.workers.dev';
const NOW = 1_700_000_000_000;

const MAC = 'aa'.repeat(32);
const WINDOWS = 'bb'.repeat(32);
const PHONE = 'cc'.repeat(32);

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

async function signedIn(db: TestDb, userId: string): Promise<string> {
  seedUser(db, userId);
  const { token } = await createSession(db, userId, NOW);
  return `${SESSION_COOKIE}=${token}`;
}

function seedHost(
  db: TestDb,
  args: {
    id: string;
    userId: string;
    label: string;
    enrolledAt?: number;
    revokedAt?: number;
    /** What the relay last recorded; absent means it has never dialled in. */
    controlSeenAt?: number;
  },
): void {
  db.raw
    .prepare(
      `INSERT INTO hosts (id, user_id, label, platform, enrolled_at, revoked_at, control_seen_at)
       VALUES (?, ?, ?, 'macos', ?, ?, ?)`,
    )
    .run(
      args.id,
      args.userId,
      args.label,
      args.enrolledAt ?? NOW,
      args.revokedAt ?? null,
      args.controlSeenAt ?? null,
    );
}

function seedDevice(db: TestDb, args: { id: string; userId: string; label: string }): void {
  db.raw
    .prepare(
      `INSERT INTO devices (id, user_id, label, kind, enrolled_at) VALUES (?, ?, ?, 'phone', ?)`,
    )
    .run(args.id, args.userId, args.label, NOW);
}

/**
 * A live device bearer token for this account.
 *
 * Minted directly rather than through the enrol-by-code flow: what is under
 * test here is that both principals read the same listing, and going the long
 * way round would make the test about enrolment instead.
 */
async function deviceToken(db: TestDb, _cookie: string): Promise<string> {
  seedDevice(db, { id: PHONE, userId: 'user-a', label: 'andy-phone' });
  const { token } = await createMachineToken(db, {
    userId: 'user-a',
    kind: 'device',
    principalId: PHONE,
    now: NOW,
  });
  return token;
}

const get = (path: string, cookie?: string) =>
  new Request(`${ORIGIN}${path}`, cookie === undefined ? undefined : { headers: { cookie } });

const revoke = (path: string, cookie?: string) => {
  const headers: Record<string, string> = { origin: ORIGIN, 'content-type': 'application/json' };
  if (cookie !== undefined) headers['cookie'] = cookie;
  return new Request(`${ORIGIN}${path}`, { method: 'POST', headers, body: '{}' });
};

// --- listing ---------------------------------------------------------------

test('listing requires a session', async () => {
  const db = testDb();
  for (const path of ['/api/hosts', '/api/devices']) {
    const res = await routeApi(get(path), env(db), fetch, NOW);
    assert.equal(res?.status, 401, path);
  }
  db.close();
});

test('a listing is the caller’s own machines, and never anybody else’s', async () => {
  const db = testDb();
  const mine = await signedIn(db, 'user-a');
  await signedIn(db, 'user-b');
  seedHost(db, { id: MAC, userId: 'user-a', label: 'andy-mac' });
  seedHost(db, { id: WINDOWS, userId: 'user-b', label: 'not-mine' });

  const res = await routeApi(get('/api/hosts', mine), env(db), fetch, NOW);
  const { hosts } = (await res!.json()) as { hosts: Array<{ id: string }> };
  assert.deepEqual(
    hosts.map((h) => h.id),
    [MAC],
    'ownership is in the WHERE clause, not a check the caller could forget',
  );
  db.close();
});

test('a revoked machine is simply not in the listing', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  seedHost(db, { id: MAC, userId: 'user-a', label: 'andy-mac' });
  seedHost(db, { id: WINDOWS, userId: 'user-a', label: 'old-box', revokedAt: NOW - 1 });

  const res = await routeApi(get('/api/hosts', cookie), env(db), fetch, NOW);
  const { hosts } = (await res!.json()) as { hosts: Array<{ id: string }> };
  assert.deepEqual(hosts.map((h) => h.id), [MAC], 'the row is kept for the audit, not for the screen');
  db.close();
});

test('a host is described field by field, never as the row', async () => {
  // `hosts` already has a column the browser has no business with -- `do_id`,
  // reserved for the relay -- and it will grow more. A spread would ship each
  // new one by default; the safe direction for that mistake is a missing field.
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  seedHost(db, { id: MAC, userId: 'user-a', label: 'andy-mac' });

  const res = await routeApi(get('/api/hosts', cookie), env(db), fetch, NOW);
  const { hosts } = (await res!.json()) as { hosts: Array<Record<string, unknown>> };
  assert.deepEqual(Object.keys(hosts[0] ?? {}).sort(), [
    'enrolledAt',
    'id',
    'label',
    'lastSeenAt',
    // A verdict rather than the `control_seen_at` column behind it: the bound
    // it is read against is the relay's own business (#237).
    'online',
    'platform',
  ]);
  db.close();
});

test('hosts and devices are separate listings', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  seedHost(db, { id: MAC, userId: 'user-a', label: 'andy-mac' });
  seedDevice(db, { id: PHONE, userId: 'user-a', label: 'andy-phone' });

  const devices = (await (
    await routeApi(get('/api/devices', cookie), env(db), fetch, NOW)
  )!.json()) as { devices: Array<{ id: string; kind: string; extractable: boolean }> };
  assert.deepEqual(devices.devices.map((d) => d.id), [PHONE]);
  assert.equal(devices.devices[0]?.kind, 'phone');
  assert.equal(
    devices.devices[0]?.extractable,
    true,
    'the schema stores 1/0; the screen must be told which, not shown a tick that is not true',
  );
  db.close();
});

// --- revoking --------------------------------------------------------------

test('revoking my own machine takes it out of the listing', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  seedHost(db, { id: MAC, userId: 'user-a', label: 'andy-mac' });

  const res = await routeApi(revoke(`/api/hosts/${MAC}/revoke`, cookie), env(db), fetch, NOW);
  assert.equal(res?.status, 200);
  assert.deepEqual(await res!.json(), { id: MAC, revokedAt: NOW });

  const after = (await (
    await routeApi(get('/api/hosts', cookie), env(db), fetch, NOW)
  )!.json()) as { hosts: unknown[] };
  assert.equal(after.hosts.length, 0);
  db.close();
});

test('revoking twice does not move the timestamp', async () => {
  // "Since when" is the fact anyone asking later wants, and a second click on a
  // slow page must not rewrite it.
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  seedHost(db, { id: MAC, userId: 'user-a', label: 'andy-mac' });

  await routeApi(revoke(`/api/hosts/${MAC}/revoke`, cookie), env(db), fetch, NOW);
  const again = await routeApi(revoke(`/api/hosts/${MAC}/revoke`, cookie), env(db), fetch, NOW + 5_000);

  assert.equal(again?.status, 200, 'revoking an already-revoked machine is not an error');
  assert.deepEqual(await again!.json(), { id: MAC, revokedAt: NOW });
  db.close();
});

test('another account’s machine cannot be revoked, and is not admitted to exist', async () => {
  const db = testDb();
  const mine = await signedIn(db, 'user-a');
  await signedIn(db, 'user-b');
  seedHost(db, { id: WINDOWS, userId: 'user-b', label: 'not-mine' });

  const theirs = await routeApi(revoke(`/api/hosts/${WINDOWS}/revoke`, mine), env(db), fetch, NOW);
  const nobody = await routeApi(revoke(`/api/hosts/${MAC}/revoke`, mine), env(db), fetch, NOW);

  assert.equal(theirs?.status, 404);
  assert.deepEqual(
    await theirs!.json(),
    await nobody!.json(),
    'the ids are public keys, so telling the two apart answers "is this key enrolled with zesterm" for strangers',
  );
  assert.deepEqual(
    rowOf(db, `SELECT revoked_at FROM hosts WHERE id = ?`, WINDOWS),
    { revoked_at: null },
    'and it must not have been revoked on the way to that 404',
  );
  db.close();
});

test('revoking requires a session', async () => {
  const db = testDb();
  await signedIn(db, 'user-a');
  seedHost(db, { id: MAC, userId: 'user-a', label: 'andy-mac' });

  const res = await routeApi(revoke(`/api/hosts/${MAC}/revoke`), env(db), fetch, NOW);
  assert.equal(res?.status, 401);
  assert.deepEqual(rowOf(db, `SELECT revoked_at FROM hosts WHERE id = ?`, MAC), {
    revoked_at: null,
  });
  db.close();
});

test('a device revoke does not reach a host with the same id', async () => {
  // The two tables have separate key spaces by design -- `HostId` and
  // `ClientId` are kept apart so an authorization check cannot ask the wrong
  // question -- and one machine holds a key in each.
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  seedHost(db, { id: MAC, userId: 'user-a', label: 'andy-mac' });

  const res = await routeApi(revoke(`/api/devices/${MAC}/revoke`, cookie), env(db), fetch, NOW);
  assert.equal(res?.status, 404);
  assert.deepEqual(rowOf(db, `SELECT revoked_at FROM hosts WHERE id = ?`, MAC), {
    revoked_at: null,
  });
  db.close();
});

test('an id that is not 32 bytes of lowercase hex is refused without a round trip', async () => {
  // The 'not a round trip' half of the old name was never asserted, and the
  // uppercase case would have 404'd anyway -- there is no such row -- so it
  // never exercised the case-strictness it was named for.
  //
  // Counting queries is what distinguishes "refused on shape" from "looked up
  // and not found", which both answer 404. It is also the property worth
  // having: a malformed id must not cost a database round trip.
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');

  for (const id of ['abc', MAC.toUpperCase(), `${MAC}ff`, "'; DROP TABLE hosts; --"]) {
    let queries = 0;
    const counting = { ...db, prepare: (sql: string) => (queries++, db.prepare(sql)) };
    const res = await routeApi(
      revoke(`/api/hosts/${encodeURIComponent(id)}/revoke`, cookie),
      env(counting),
      fetch,
      NOW,
    );
    assert.equal(res?.status, 404, id);
    // The session lookup is one query; anything beyond it means the id reached
    // the registry.
    assert.ok(queries <= 1, `${id}: reached the database (${queries} queries)`);
  }
  db.close();
});

test('the registry routes refuse the wrong method', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');

  const list = await routeApi(revoke('/api/hosts', cookie), env(db), fetch, NOW);
  assert.equal(list?.status, 405, 'a listing is a GET');

  const rev = await routeApi(get(`/api/hosts/${MAC}/revoke`, cookie), env(db), fetch, NOW);
  assert.equal(rev?.status, 405, 'revoking is a POST');
  db.close();
});

// --- the store's own guards, below the route -------------------------------

test('the insert refuses a revoked or foreign key on its own, not only via the route', async () => {
  // The route checks first and answers 409, which is what a caller sees. These
  // conditions on `ON CONFLICT DO UPDATE` are the backstop for the window
  // between that check and this write — an owner revoking a machine while it is
  // mid-enrolment — so they need a test that reaches past the route, or the
  // only thing proving them is a race nobody can schedule.
  const db = testDb();
  seedUser(db, 'user-a');
  seedUser(db, 'user-b');
  seedHost(db, { id: MAC, userId: 'user-a', label: 'andy-mac', revokedAt: NOW - 1 });
  seedHost(db, { id: WINDOWS, userId: 'user-b', label: 'not-mine' });

  assert.equal(
    await enrolHost(db, { id: MAC, userId: 'user-a', label: 'back-please', platform: '', now: NOW }),
    null,
    'a revoked machine that could re-enrol itself has un-revoked itself',
  );
  assert.equal(
    await enrolHost(db, { id: WINDOWS, userId: 'user-a', label: 'mine-now', platform: '', now: NOW }),
    null,
    'holding a key is not a claim on somebody else’s row',
  );
  assert.deepEqual(
    rowOf(db, `SELECT user_id, label, revoked_at FROM hosts WHERE id = ?`, WINDOWS),
    { user_id: 'user-b', label: 'not-mine', revoked_at: null },
    'and a refused write must not have changed anything on the way to refusing',
  );
  db.close();
});

test('a code cannot be spent twice, whatever the caller does', async () => {
  // The compare-and-set that makes a replay inert, tested where the race can be
  // written down: two spends of one code, with no route in between.
  const db = testDb();
  seedUser(db, 'user-a');
  const { code } = await createEnrollCode(db, 'user-a', 'host', NOW);

  assert.equal(await spendEnrollCode(db, code, MAC, NOW), 'host');
  assert.equal(
    await spendEnrollCode(db, code, WINDOWS, NOW),
    null,
    'the second caller must learn it lost, rather than enrolling a second machine',
  );
  assert.deepEqual(rowOf(db, `SELECT used_by FROM enroll_codes WHERE code = ?`, code), {
    used_by: MAC,
  });
  db.close();
});


// --- online, and why it is a verdict rather than a timestamp (#237) --------

test('a machine whose control link is parked reads online', async () => {
  // The bug in one test. Before this, the fleet screen had nothing to read but
  // `lastSeenAt` — which records *arrival* and never expires — so a machine
  // sitting there with a parked link was rendered "asleep" while clicking it
  // opened a shell immediately.
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  seedHost(db, { id: MAC, userId: 'user-a', label: 'andy-mac', controlSeenAt: NOW });

  const res = await routeApi(get('/api/hosts', cookie), env(db), fetch, NOW + 1_000);
  const { hosts } = (await res!.json()) as { hosts: Array<{ online: boolean }> };
  assert.equal(hosts[0]?.online, true, 'the relay proved this link was parked a second ago');
  db.close();
});

test('a machine last proved alive longer ago than the bound is not online', async () => {
  // The decay that keeps a room evicted without its close handler from lying
  // for ever. Nothing can clear that row, so it has to expire on its own.
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  seedHost(db, { id: MAC, userId: 'user-a', label: 'andy-mac', controlSeenAt: NOW });

  const inside = await routeApi(get('/api/hosts', cookie), env(db), fetch, NOW + CONTROL_SEEN_BOUND_MS - 1);
  assert.equal(
    ((await inside!.json()) as { hosts: Array<{ online: boolean }> }).hosts[0]?.online,
    true,
    'the last millisecond inside the bound is still online — an alarm is retried rather than punctual, and flapping a reachable machine is the failure being fixed',
  );

  const outside = await routeApi(get('/api/hosts', cookie), env(db), fetch, NOW + CONTROL_SEEN_BOUND_MS);
  assert.equal(
    ((await outside!.json()) as { hosts: Array<{ online: boolean }> }).hosts[0]?.online,
    false,
    'and at the bound exactly it is not: the relay stopped refreshing, so there is no longer any evidence anyone can reach it',
  );
  db.close();
});

test('a machine that has never dialled a relay is not online', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  seedHost(db, { id: MAC, userId: 'user-a', label: 'andy-mac' });

  const res = await routeApi(get('/api/hosts', cookie), env(db), fetch, NOW);
  const { hosts } = (await res!.json()) as { hosts: Array<{ online: boolean; lastSeenAt: null }> };
  assert.equal(
    hosts[0]?.online,
    false,
    'enrolment is not reachability — a machine can be in the account and have never connected to anything',
  );
  assert.equal(hosts[0]?.lastSeenAt, null, 'and the two facts stay separate columns');
  db.close();
});

test('the cookie and the device-bearer paths answer the same online', async () => {
  // Two callers, one truth. The desktop app reads this list with a machine
  // token and the browser with a session; a screen that disagreed with itself
  // depending on which asked would be #237 again, one layer up.
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  seedHost(db, { id: MAC, userId: 'user-a', label: 'andy-mac', controlSeenAt: NOW });
  const token = await deviceToken(db, cookie);

  const asPerson = await routeApi(get('/api/hosts', cookie), env(db), fetch, NOW + 1_000);
  const asDevice = await routeApi(
    new Request(`${ORIGIN}/api/hosts`, { headers: { authorization: `Bearer ${token}` } }),
    env(db),
    fetch,
    NOW + 1_000,
  );

  const person = ((await asPerson!.json()) as { hosts: Array<{ online: boolean }> }).hosts;
  const device = ((await asDevice!.json()) as { hosts: Array<{ online: boolean }> }).hosts;
  assert.equal(person[0]?.online, true);
  assert.deepEqual(device, person, 'the same row, the same verdict, whoever is asking');
  db.close();
});

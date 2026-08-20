/**
 * The registry's audit log (#373): every act that changes what an account
 * trusts writes exactly one event, and the log is the owner's to read.
 *
 * What is asserted is mostly the negatives that make an audit log worth
 * believing: a refused act writes nothing, another account's history is
 * invisible, and no machine credential can read it.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { SESSION_COOKIE } from '@zesterm/cloud-shared';

import { createSession } from '../src/db/sessions.ts';
import { routeApi } from '../src/router.ts';
import { rowOf, seedUser, testDb, type TestDb } from './d1.ts';
import { signEnrollment, testKey } from './keys.ts';
import type { Env } from '../src/env.ts';

const ORIGIN = 'https://zesterm.sigx.workers.dev';
const NOW = 1_700_000_000_000;

const MAC = 'aa'.repeat(32);

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

const get = (path: string, cookie?: string) =>
  new Request(`${ORIGIN}${path}`, cookie === undefined ? undefined : { headers: { cookie } });

const post = (path: string, cookie?: string) => {
  const headers: Record<string, string> = { origin: ORIGIN, 'content-type': 'application/json' };
  if (cookie !== undefined) headers['cookie'] = cookie;
  return new Request(`${ORIGIN}${path}`, { method: 'POST', headers, body: '{}' });
};

function seedHost(db: TestDb, id: string, userId: string, label: string, revokedAt?: number): void {
  db.raw
    .prepare(
      `INSERT INTO hosts (id, user_id, label, platform, enrolled_at, revoked_at)
       VALUES (?, ?, ?, 'macos', ?, ?)`,
    )
    .run(id, userId, label, NOW, revokedAt ?? null);
}

type EventRow = {
  user_id: string;
  actor: string;
  action: string;
  subject_kind: string;
  subject_id: string;
  subject_label: string;
  at: number;
};

// Plain objects, `rowOf`'s reason: node:sqlite rows are null-prototype and
// `assert.deepEqual` compares prototypes.
function events(db: TestDb): EventRow[] {
  return (
    db.raw
      .prepare(`SELECT user_id, actor, action, subject_kind, subject_id, subject_label, at
                  FROM registry_events ORDER BY id`)
      .all() as EventRow[]
  ).map((r) => ({ ...r }));
}

test('a revoke and its restore each write one owner event carrying the label', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  seedHost(db, MAC, 'user-a', 'andy-mac');

  await routeApi(post(`/api/hosts/${MAC}/revoke`, cookie), env(db), fetch, NOW);
  await routeApi(post(`/api/hosts/${MAC}/restore`, cookie), env(db), fetch, NOW + 1);

  assert.deepEqual(events(db), [
    {
      user_id: 'user-a',
      actor: 'owner',
      action: 'revoke',
      subject_kind: 'host',
      subject_id: MAC,
      subject_label: 'andy-mac',
      at: NOW,
    },
    {
      user_id: 'user-a',
      actor: 'owner',
      action: 'restore',
      subject_kind: 'host',
      subject_id: MAC,
      subject_label: 'andy-mac',
      at: NOW + 1,
    },
  ]);
  db.close();
});

test('a refused revoke writes no event', async () => {
  // An audit line for an act that did not happen is worse than a missing one:
  // it answers "what happened" with something that did not.
  const db = testDb();
  const mine = await signedIn(db, 'user-a');
  await signedIn(db, 'user-b');
  seedHost(db, MAC, 'user-b', 'not-mine');

  const res = await routeApi(post(`/api/hosts/${MAC}/revoke`, mine), env(db), fetch, NOW);
  assert.equal(res?.status, 404);
  assert.deepEqual(events(db), []);
  db.close();
});

test('a code claim records the machine as the actor', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const mintRes = await routeApi(
    new Request(`${ORIGIN}/api/enroll/code`, {
      method: 'POST',
      headers: { origin: ORIGIN, 'content-type': 'application/json', cookie },
      body: JSON.stringify({ kind: 'host' }),
    }),
    env(db),
    fetch,
    NOW,
  );
  const { code } = (await mintRes!.json()) as { code: string };
  const key = await testKey(7);

  await routeApi(
    new Request(`${ORIGIN}/api/enroll/claim`, {
      method: 'POST',
      headers: { 'content-type': 'application/json' },
      body: JSON.stringify({
        code,
        hostId: key.id,
        label: 'andy-mac',
        sig: await signEnrollment({ key, code, label: 'andy-mac' }),
      }),
    }),
    env(db),
    fetch,
    NOW,
  );

  const all = events(db);
  assert.equal(all.length, 1, 'one claim, one event');
  assert.equal(all[0]?.action, 'enroll');
  assert.equal(
    all[0]?.actor,
    'machine',
    'the code was a person’s act, but this request is the key proving possession of itself',
  );
  assert.equal(all[0]?.subject_label, 'andy-mac');
  db.close();
});

test('the events listing is the owner’s and nobody else’s', async () => {
  const db = testDb();
  const mine = await signedIn(db, 'user-a');
  const theirs = await signedIn(db, 'user-b');
  seedHost(db, MAC, 'user-a', 'andy-mac');
  await routeApi(post(`/api/hosts/${MAC}/revoke`, mine), env(db), fetch, NOW);

  const own = await routeApi(get('/api/registry/events', mine), env(db), fetch, NOW);
  const { events: listed } = (await own!.json()) as {
    events: Array<{ action: string; subjectLabel: string; at: number }>;
  };
  assert.equal(listed.length, 1);
  assert.equal(listed[0]?.action, 'revoke');
  assert.equal(listed[0]?.subjectLabel, 'andy-mac');

  const other = await routeApi(get('/api/registry/events', theirs), env(db), fetch, NOW);
  assert.deepEqual(
    ((await other!.json()) as { events: unknown[] }).events,
    [],
    'another account’s history is not this caller’s to read',
  );

  const anonymous = await routeApi(get('/api/registry/events'), env(db), fetch, NOW);
  assert.equal(anonymous?.status, 401);
  db.close();
});

test('a machine token cannot read the account’s history', async () => {
  // Cookie-only like the revoked view it explains: which keys were cast out
  // and by whom is the owner's reading, not something any bearer credential
  // needs to reach the machines that exist.
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  seedHost(db, MAC, 'user-a', 'andy-mac');
  await routeApi(post(`/api/hosts/${MAC}/revoke`, cookie), env(db), fetch, NOW);

  const res = await routeApi(
    new Request(`${ORIGIN}/api/registry/events`, {
      headers: { authorization: 'Bearer zt1_not-even-checked' },
    }),
    env(db),
    fetch,
    NOW,
  );
  assert.notEqual(res?.status, 200, 'a bearer must not read the audit log');
  db.close();
});

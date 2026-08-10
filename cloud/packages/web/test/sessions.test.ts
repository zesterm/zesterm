import { test } from 'node:test';
import assert from 'node:assert/strict';

import { sessionIdOf } from '@zesterm/cloud-shared';

import {
  createSession,
  destroySession,
  resolveSession,
  LAST_SEEN_GRANULARITY_MS,
  SESSION_TTL_MS,
} from '../src/db/sessions.ts';
import { seedUser, testDb } from './d1.ts';

const NOW = 1_700_000_000_000;

test('a fresh session resolves to its user', async () => {
  const db = testDb();
  seedUser(db, 'user-a');
  const { token } = await createSession(db, 'user-a', NOW);

  const user = await resolveSession(db, token, NOW + 1_000);
  assert.equal(user?.id, 'user-a');
  assert.equal(user?.displayName, 'user-a');
  db.close();
});

test('the token is never written to the database', async () => {
  // The whole reason for hashing: a dump of `sessions` must not be a set of
  // usable cookies.
  const db = testDb();
  seedUser(db, 'user-a');
  const { token } = await createSession(db, 'user-a', NOW);

  const rows = db.raw.prepare('SELECT id FROM sessions').all() as Array<{ id: string }>;
  assert.equal(rows.length, 1);
  assert.notEqual(rows[0]?.id, token);
  assert.equal(rows[0]?.id, await sessionIdOf(token), 'the row key is sha256(token)');
  db.close();
});

test('an expired session resolves to nobody', async () => {
  const db = testDb();
  seedUser(db, 'user-a');
  const { token } = await createSession(db, 'user-a', NOW);

  assert.notEqual(await resolveSession(db, token, NOW + SESSION_TTL_MS - 1), null);
  assert.equal(await resolveSession(db, token, NOW + SESSION_TTL_MS), null, 'exactly at expiry');
  assert.equal(await resolveSession(db, token, NOW + SESSION_TTL_MS + 1), null);
  db.close();
});

test('a revoked session resolves to nobody even before it expires', async () => {
  const db = testDb();
  seedUser(db, 'user-a');
  const { token } = await createSession(db, 'user-a', NOW);
  db.raw.prepare('UPDATE sessions SET revoked_at = ?').run(NOW);

  assert.equal(await resolveSession(db, token, NOW + 1_000), null);
  db.close();
});

test('a disabled account cannot be signed in as, on any of its sessions', async () => {
  // The check belongs on the read path, not on disabling: sessions created
  // before the account was disabled must stop working immediately, without
  // anyone having to remember to sweep them.
  const db = testDb();
  seedUser(db, 'user-a');
  const { token } = await createSession(db, 'user-a', NOW);
  db.raw.prepare('UPDATE users SET disabled_at = ? WHERE id = ?').run(NOW, 'user-a');

  assert.equal(await resolveSession(db, token, NOW + 1_000), null);
  db.close();
});

test('every rejection is the same answer', async () => {
  // Expired, unknown, malformed and absent all return null. A caller that
  // could tell them apart would be tempted to say which, and "this token
  // existed but expired" is worth more to someone guessing than to the person
  // who is simply logged out.
  const db = testDb();
  seedUser(db, 'user-a');

  assert.equal(await resolveSession(db, null, NOW), null, 'no cookie');
  assert.equal(await resolveSession(db, 'not-a-token', NOW), null, 'malformed');
  assert.equal(await resolveSession(db, 'A'.repeat(64), NOW), null, 'well-formed but unknown');
  db.close();
});

test('a malformed token costs no database round trip', async () => {
  // An unauthenticated flood of junk cookies must not become a way to load D1.
  const db = testDb();
  let prepared = 0;
  const counting = { ...db, prepare: (sql: string) => (prepared++, db.prepare(sql)) };

  assert.equal(await resolveSession(counting, 'obviously-not-a-token', NOW), null);
  assert.equal(prepared, 0, 'shape is checked before the query is built');
  db.close();
});

test('last_seen_at slides, but not on every request', async () => {
  const db = testDb();
  seedUser(db, 'user-a');
  const { token } = await createSession(db, 'user-a', NOW);
  const seen = () =>
    (db.raw.prepare('SELECT last_seen_at AS t FROM sessions').get() as { t: number }).t;

  await resolveSession(db, token, NOW + LAST_SEEN_GRANULARITY_MS - 1);
  assert.equal(seen(), NOW, 'a write per request would be a D1 round trip nothing reads');

  const later = NOW + LAST_SEEN_GRANULARITY_MS + 1;
  await resolveSession(db, token, later);
  assert.equal(seen(), later, 'but it does eventually slide');
  db.close();
});

test('signing out ends that session and no other', async () => {
  const db = testDb();
  seedUser(db, 'user-a');
  const a = await createSession(db, 'user-a', NOW);
  const b = await createSession(db, 'user-a', NOW);

  await destroySession(db, a.token);
  assert.equal(await resolveSession(db, a.token, NOW + 1), null, 'this browser');
  assert.notEqual(await resolveSession(db, b.token, NOW + 1), null, 'the other one still works');
  db.close();
});

test('deleting a user takes their sessions with them', async () => {
  // ON DELETE CASCADE, which only holds because the fake turns foreign keys on
  // the way the migration does.
  const db = testDb();
  seedUser(db, 'user-a');
  const { token } = await createSession(db, 'user-a', NOW);

  db.raw.prepare('DELETE FROM users WHERE id = ?').run('user-a');
  assert.equal(await resolveSession(db, token, NOW + 1), null);
  assert.equal((db.raw.prepare('SELECT COUNT(*) AS n FROM sessions').get() as { n: number }).n, 0);
  db.close();
});

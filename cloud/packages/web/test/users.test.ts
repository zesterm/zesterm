import { test } from 'node:test';
import assert from 'node:assert/strict';

import { upsertUser, type Profile } from '../src/db/users.ts';
import { testDb } from './d1.ts';

const NOW = 1_700_000_000_000;

const profile = (over: Partial<Profile> = {}): Profile => ({
  subject: '12345',
  email: 'andy@example.com',
  emailVerified: true,
  displayName: 'Andy',
  avatarUrl: null,
  ...over,
});

test('a first sign-in creates exactly one account and one identity', async () => {
  const db = testDb();
  const id = await upsertUser(db, 'github', profile(), NOW);

  const users = db.raw.prepare('SELECT id FROM users').all() as Array<{ id: string }>;
  assert.equal(users.length, 1);
  assert.equal(users[0]?.id, id);
  assert.equal(
    (db.raw.prepare('SELECT COUNT(*) AS n FROM oauth_identities').get() as { n: number }).n,
    1,
  );
  db.close();
});

test('signing in again is the same account, not a second one', async () => {
  const db = testDb();
  const first = await upsertUser(db, 'github', profile(), NOW);
  const second = await upsertUser(db, 'github', profile(), NOW + 1000);

  assert.equal(second, first);
  assert.equal((db.raw.prepare('SELECT COUNT(*) AS n FROM users').get() as { n: number }).n, 1);
  db.close();
});

test('the identity is the numeric subject, so a renamed login is still you', async () => {
  // GitHub logins are renameable and reusable. Keying on one means whoever
  // claims the name next inherits the account, so `subject` is the numeric id
  // and the display name is allowed to change freely underneath it.
  const db = testDb();
  const first = await upsertUser(db, 'github', profile({ displayName: 'Andy' }), NOW);
  const again = await upsertUser(
    db,
    'github',
    profile({ displayName: 'Andreas', email: 'new@example.com' }),
    NOW + 1000,
  );

  assert.equal(again, first, 'same subject, same account');
  const row = db.raw.prepare('SELECT display_name FROM users WHERE id = ?').get(first) as {
    display_name: string;
  };
  assert.equal(row.display_name, 'Andreas', 'and the profile follows');
  db.close();
});

test('a different subject at the same provider is a different account', async () => {
  const db = testDb();
  const a = await upsertUser(db, 'github', profile({ subject: '1', email: 'a@example.com' }), NOW);
  const b = await upsertUser(db, 'github', profile({ subject: '2', email: 'b@example.com' }), NOW);

  assert.notEqual(a, b);
  db.close();
});

test('two providers with the same VERIFIED email are one person', async () => {
  const db = testDb();
  const viaGithub = await upsertUser(db, 'github', profile({ subject: 'gh-1' }), NOW);
  const viaGoogle = await upsertUser(db, 'google', profile({ subject: 'goog-1' }), NOW + 1000);

  assert.equal(viaGoogle, viaGithub, 'the same person arriving a second way');
  assert.equal((db.raw.prepare('SELECT COUNT(*) AS n FROM users').get() as { n: number }).n, 1);
  assert.equal(
    (db.raw.prepare('SELECT COUNT(*) AS n FROM oauth_identities').get() as { n: number }).n,
    2,
    'one account, two ways in',
  );
  db.close();
});

test('an UNVERIFIED email never links into an existing account', async () => {
  // The one that matters. Linking on an unverified address is a complete
  // account-takeover primitive: sign up anywhere claiming the victim's email
  // and get merged into their account. A separate account is a support
  // question; this would be a breach.
  const db = testDb();
  const victim = await upsertUser(db, 'github', profile({ subject: 'gh-1' }), NOW);
  const attacker = await upsertUser(
    db,
    'google',
    profile({ subject: 'goog-evil', emailVerified: false }),
    NOW + 1000,
  );

  assert.notEqual(attacker, victim, 'an unverified claim must not reach an existing account');
  assert.equal((db.raw.prepare('SELECT COUNT(*) AS n FROM users').get() as { n: number }).n, 2);
  db.close();
});

test('an unverified identity on file is not a link target either', async () => {
  // Both directions. The stored identity must also be verified, which is why
  // `email_verified` lives per identity rather than per user -- an account
  // created from an unverified address must not become a magnet for verified
  // ones later.
  const db = testDb();
  const unverified = await upsertUser(
    db,
    'github',
    profile({ subject: 'gh-1', emailVerified: false }),
    NOW,
  );
  const verified = await upsertUser(db, 'google', profile({ subject: 'goog-1' }), NOW + 1000);

  assert.notEqual(verified, unverified);
  db.close();
});

test('a profile with no email at all never links', async () => {
  const db = testDb();
  const a = await upsertUser(db, 'github', profile({ subject: '1', email: null }), NOW);
  const b = await upsertUser(db, 'google', profile({ subject: '2', email: null }), NOW + 1);

  assert.notEqual(a, b, 'two people without addresses are not the same person');
  db.close();
});

test('a null email cannot be claimed as verified into a link', async () => {
  // `emailVerified: true` with `email: null` is nonsense a provider adapter
  // could produce by mistake; it must not match the other null-email identity.
  const db = testDb();
  const a = await upsertUser(db, 'github', profile({ subject: '1', email: null }), NOW);
  const b = await upsertUser(
    db,
    'google',
    profile({ subject: '2', email: null, emailVerified: true }),
    NOW + 1,
  );

  assert.notEqual(a, b);
  db.close();
});

test('last_login_at moves on every sign-in', async () => {
  const db = testDb();
  await upsertUser(db, 'github', profile(), NOW);
  await upsertUser(db, 'github', profile(), NOW + 5_000);

  const row = db.raw
    .prepare('SELECT created_at, last_login_at FROM oauth_identities')
    .get() as { created_at: number; last_login_at: number };
  assert.equal(row.created_at, NOW, 'created_at is when we first saw them');
  assert.equal(row.last_login_at, NOW + 5_000);
  db.close();
});

test('a lost race lands on one account and leaves no orphan', async () => {
  // One person, two tabs: both callbacks find no identity, both mint a `users`
  // row, and only one identity insert can win. Before `ON CONFLICT DO NOTHING`
  // the loser threw a primary-key conflict out of the OAuth callback as a 500
  // *and* left its `users` row behind with nothing pointing at it.
  //
  // Staged rather than genuinely concurrent: the window is between this call's
  // "no identity yet" lookup and its insert, so the rival's identity is written
  // exactly there, by wrapping the statement the loser is about to run.
  const db = testDb();
  const rival = 'rival-account';
  db.raw
    .prepare(
      `INSERT INTO users (id, email, display_name, avatar_url, created_at, updated_at)
       VALUES (?, ?, ?, ?, ?, ?)`,
    )
    .run(rival, 'andy@example.com', 'Andy', null, NOW, NOW);

  let raced = false;
  const racing = {
    ...db,
    prepare(sql: string) {
      if (sql.includes('INSERT INTO oauth_identities') && !raced) {
        raced = true;
        // The other tab gets there first.
        db.raw
          .prepare(
            `INSERT INTO oauth_identities
               (provider, subject, user_id, email, email_verified, created_at, last_login_at)
             VALUES ('github', '12345', ?, 'andy@example.com', 1, ?, ?)`,
          )
          .run(rival, NOW, NOW);
      }
      return db.prepare(sql);
    },
  };

  const settled = await upsertUser(racing, 'github', profile(), NOW);

  assert.ok(raced, 'the interleaving must actually have been staged');
  assert.equal(settled, rival, 'the identity that landed first decides the account');

  const users = db.raw.prepare('SELECT id FROM users').all() as Array<{ id: string }>;
  assert.deepEqual(
    users.map((u) => u.id),
    [rival],
    'the account this call created must not be left orphaned',
  );
  assert.equal(
    (db.raw.prepare('SELECT COUNT(*) AS n FROM oauth_identities').get() as { n: number }).n,
    1,
  );
  db.close();
});

test('a duplicate identity insert does not throw', async () => {
  // The direct form of the same thing: whatever the interleaving, a second
  // insert for a subject that already exists must be a no-op rather than a 500
  // out of the callback.
  const db = testDb();
  const first = await upsertUser(db, 'github', profile(), NOW);
  const again = await upsertUser(db, 'github', profile(), NOW + 1);
  const third = await upsertUser(db, 'github', profile(), NOW + 2);

  assert.equal(again, first);
  assert.equal(third, first);
  assert.equal((db.raw.prepare('SELECT COUNT(*) AS n FROM users').get() as { n: number }).n, 1);
  db.close();
});

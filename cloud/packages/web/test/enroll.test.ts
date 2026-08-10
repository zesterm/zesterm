/**
 * `/api/enroll/code` and `/api/enroll/claim`, end to end against the real
 * migration and real Ed25519.
 *
 * The claim is the only unauthenticated write this Worker has, so most of what
 * is asserted here is about *ordering* and *what is not consulted* rather than
 * about the happy path: that a wrong signature cannot burn a code, that a
 * replay enrols nothing, that the session cookie decides nothing, and that the
 * account the machine lands in is the one that minted the code.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { SESSION_COOKIE } from '@zesterm/cloud-shared';

import { ENROLL_CODE_ALPHABET, ENROLL_CODE_LENGTH, ENROLL_CODE_TTL_MS } from '../src/enroll/codes.ts';
import { createSession } from '../src/db/sessions.ts';
import { routeApi } from '../src/router.ts';
import { rowOf, seedUser, testDb, type TestDb } from './d1.ts';
import { signEnrollment, testKey } from './keys.ts';
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

/** A daemon: JSON, no origin, no cookie. */
function daemonPost(path: string, body: unknown): Request {
  return new Request(`${ORIGIN}${path}`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
}

async function signedIn(db: TestDb, userId: string): Promise<string> {
  seedUser(db, userId);
  const { token } = await createSession(db, userId, NOW);
  return `${SESSION_COOKIE}=${token}`;
}

async function mint(db: TestDb, cookie: string, kind: 'host' | 'device', now = NOW) {
  const res = await routeApi(post('/api/enroll/code', { kind }, cookie), env(db), fetch, now);
  assert.equal(res?.status, 200, 'minting must succeed for a signed-in caller');
  return (await res!.json()) as { code: string; expiresAt: number };
}

// --- minting ---------------------------------------------------------------

test('minting a code requires a session', async () => {
  const db = testDb();
  const res = await routeApi(post('/api/enroll/code', { kind: 'host' }), env(db), fetch, NOW);
  assert.equal(res?.status, 401, 'a code is an act by a person, and there is nobody here');
  db.close();
});

test('a minted code is short, unambiguous, and short-lived', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const { code, expiresAt } = await mint(db, cookie, 'host');

  assert.equal(code.length, ENROLL_CODE_LENGTH);

  // The alphabet itself, deterministically.
  //
  // Asserting only that each *generated* character is safe catches a bad
  // alphabet just 73% of the time -- eight draws from thirty-five characters
  // miss any given five about a quarter of the time -- so a regression would
  // land as an intermittent failure someone reruns away. The constant is what
  // is actually being claimed, and checking it cannot flake.
  for (const c of '0O1Il') {
    assert.ok(
      !ENROLL_CODE_ALPHABET.includes(c),
      `${c} is confusable and must not be in the alphabet: the code is read off \
       one screen and typed into another`,
    );
  }

  // And the generated code is drawn from it. Membership in the alphabet is a
  // tautology on its own -- `newEnrollCode()` builds the code by indexing that
  // same constant -- but paired with the check above it is the half that says
  // the generator uses the alphabet it was given.
  for (const c of code) {
    assert.ok(ENROLL_CODE_ALPHABET.includes(c), `${c} is not from the alphabet`);
  }
  assert.equal(
    expiresAt,
    NOW + ENROLL_CODE_TTL_MS,
    'the code is a bearer token for its whole life, so the window is minutes rather than days',
  );
  db.close();
});

test('minting refuses a kind it does not know', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  for (const kind of ['relay', '', null, undefined]) {
    const res = await routeApi(post('/api/enroll/code', { kind }, cookie), env(db), fetch, NOW);
    assert.equal(res?.status, 400, `kind=${String(kind)} must not mint anything`);
  }
  db.close();
});

// --- claiming --------------------------------------------------------------

test('a machine that holds the key is enrolled into the code owner’s account', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const { code } = await mint(db, cookie, 'host');
  const key = await testKey(7);

  const res = await routeApi(
    daemonPost('/api/enroll/claim', {
      code,
      hostId: key.id,
      label: 'andy-mac',
      sig: await signEnrollment({ key, code, label: 'andy-mac' }),
      platform: 'macos',
    }),
    env(db),
    fetch,
    NOW,
  );

  assert.equal(res?.status, 200);
  assert.deepEqual(await res!.json(), {
    host: {
      id: key.id,
      label: 'andy-mac',
      platform: 'macos',
      enrolledAt: NOW,
      lastSeenAt: null,
    },
  });

  const row = rowOf(db, `SELECT user_id, label FROM hosts WHERE id = ?`, key.id);
  assert.deepEqual(row, { user_id: 'user-a', label: 'andy-mac' });

  const spent = rowOf(db, `SELECT used_at, used_by FROM enroll_codes WHERE code = ?`, code);
  assert.deepEqual(spent, { used_at: NOW, used_by: key.id }, 'the code is spent, and says by whom');
  db.close();
});

test('a wrong signature does not burn the code', async () => {
  // The ordering this endpoint exists to get right. Verify, then spend --
  // reversed, anyone who can reach the endpoint can make a person mint codes
  // until they give up, while holding no key at all.
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const { code } = await mint(db, cookie, 'host');
  const real = await testKey(7);
  const impostor = await testKey(8);

  const forged = await routeApi(
    daemonPost('/api/enroll/claim', {
      code,
      hostId: real.id,
      // A perfectly valid signature -- over the same bytes, by the wrong key.
      sig: await signEnrollment({ key: impostor, code, label: 'andy-mac' }),
      label: 'andy-mac',
    }),
    env(db),
    fetch,
    NOW,
  );
  // The same answer an unknown code gets, deliberately: telling these apart
  // let an unauthenticated caller confirm a code was live for free and without
  // spending it, and the signature proves only key possession -- so a live
  // code found that way enrols the finder's own machine.
  assert.equal(forged?.status, 400);
  assert.deepEqual(await forged!.json(), { error: 'invalid_code' });

  const still = rowOf(db, `SELECT used_at FROM enroll_codes WHERE code = ?`, code);
  assert.deepEqual(still, { used_at: null }, 'a rejected claim must leave the code usable');

  // And a code that never existed answers identically, byte for byte -- which
  // is what makes the pair useless as a liveness oracle.
  const unknown = await routeApi(
    daemonPost('/api/enroll/claim', {
      code: 'ZZZZZZZZ',
      hostId: real.id,
      label: 'andy-mac',
      sig: await signEnrollment({ key: real, code: 'ZZZZZZZZ', label: 'andy-mac' }),
    }),
    env(db),
    fetch,
    NOW,
  );
  assert.equal(unknown?.status, forged?.status, 'a dead code and a bad signature must not differ');
  assert.deepEqual(await unknown!.json(), { error: 'invalid_code' });

  const good = await routeApi(
    daemonPost('/api/enroll/claim', {
      code,
      hostId: real.id,
      label: 'andy-mac',
      sig: await signEnrollment({ key: real, code, label: 'andy-mac' }),
    }),
    env(db),
    fetch,
    NOW,
  );
  assert.equal(good?.status, 200, 'the machine that was meant to answer still can');
  db.close();
});

test('a replayed claim enrols nothing a second time', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const { code } = await mint(db, cookie, 'host');
  const key = await testKey(7);
  const body = {
    code,
    hostId: key.id,
    label: 'andy-mac',
    sig: await signEnrollment({ key, code, label: 'andy-mac' }),
  };

  assert.equal((await routeApi(daemonPost('/api/enroll/claim', body), env(db), fetch, NOW))?.status, 200);

  const replay = await routeApi(daemonPost('/api/enroll/claim', body), env(db), fetch, NOW + 1);
  assert.equal(replay?.status, 400, 'a captured claim is a complete credential until the code is spent');
  assert.deepEqual(await replay!.json(), { error: 'invalid_code' });

  const { count } = rowOf(db, `SELECT COUNT(*) AS count FROM hosts`) as { count: number };
  assert.equal(count, 1, 'one code, one machine');
  db.close();
});

test('an expired code is refused', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const { code, expiresAt } = await mint(db, cookie, 'host');
  const key = await testKey(7);

  const res = await routeApi(
    daemonPost('/api/enroll/claim', {
      code,
      hostId: key.id,
      label: 'andy-mac',
      sig: await signEnrollment({ key, code, label: 'andy-mac' }),
    }),
    env(db),
    fetch,
    expiresAt,
  );
  assert.equal(res?.status, 400, 'expiry is inclusive: at the stated instant it is already gone');
  db.close();
});

test('a spent code and a code that never existed are the same answer', async () => {
  // The database keeps them apart -- `used_at` is set, the row is not deleted --
  // so "why can this not enrol" has an answer for whoever looks. The response
  // does not, because "that code existed and you were a minute late" is worth
  // more to whoever is guessing than to the person who mistyped.
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const { code } = await mint(db, cookie, 'host');
  const key = await testKey(7);
  const sig = await signEnrollment({ key, code, label: 'andy-mac' });
  await routeApi(
    daemonPost('/api/enroll/claim', { code, hostId: key.id, label: 'andy-mac', sig }),
    env(db),
    fetch,
    NOW,
  );

  const spent = await routeApi(
    daemonPost('/api/enroll/claim', { code, hostId: key.id, label: 'andy-mac', sig }),
    env(db),
    fetch,
    NOW,
  );
  const never = await routeApi(
    daemonPost('/api/enroll/claim', {
      code: 'ZZZZZZZZ',
      hostId: key.id,
      label: 'andy-mac',
      sig: await signEnrollment({ key, code: 'ZZZZZZZZ', label: 'andy-mac' }),
    }),
    env(db),
    fetch,
    NOW,
  );

  assert.equal(spent?.status, never?.status);
  assert.deepEqual(await spent!.json(), await never!.json());

  const row = rowOf(db, `SELECT used_at FROM enroll_codes WHERE code = ?`, code) as {
    used_at: number | null;
  };
  assert.equal(row.used_at, NOW, 'the row is kept and marked, not deleted');
  db.close();
});

test('the claim reads no session cookie: the code decides whose machine it is', async () => {
  // The reason this route may skip the Origin check at all. A route that took
  // both a cookie and that exemption would be the CSRF hole the rule closes.
  const db = testDb();
  const owner = await signedIn(db, 'user-a');
  const somebodyElse = await signedIn(db, 'user-b');
  const { code } = await mint(db, owner, 'host');
  const key = await testKey(7);

  const res = await routeApi(
    post(
      '/api/enroll/claim',
      {
        code,
        hostId: key.id,
        label: 'andy-mac',
        sig: await signEnrollment({ key, code, label: 'andy-mac' }),
      },
      somebodyElse,
    ),
    env(db),
    fetch,
    NOW,
  );

  assert.equal(res?.status, 200);
  const row = rowOf(db, `SELECT user_id FROM hosts WHERE id = ?`, key.id);
  assert.deepEqual(row, { user_id: 'user-a' }, 'signing in as somebody else must not redirect the machine');
  db.close();
});

test('a device code demands a client-role signature', async () => {
  // The role is inside the signing prefix and is taken from the code the
  // account minted, never from the request. Without that, one enrolment
  // signature would enrol either kind of key.
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const { code } = await mint(db, cookie, 'device');
  const key = await testKey(9);

  const wrongRole = await routeApi(
    daemonPost('/api/enroll/claim', {
      code,
      hostId: key.id,
      label: 'andy-phone',
      sig: await signEnrollment({ key, code, label: 'andy-phone', role: 'host' }),
    }),
    env(db),
    fetch,
    NOW,
  );
  // 400 rather than 401: a failed signature is answered exactly as an unknown
  // code is, so the pair cannot be used to probe whether a code is live.
  assert.equal(wrongRole?.status, 400, 'a host proof must not enrol a client key');

  const res = await routeApi(
    daemonPost('/api/enroll/claim', {
      code,
      hostId: key.id,
      label: 'andy-phone',
      deviceKind: 'phone',
      sig: await signEnrollment({ key, code, label: 'andy-phone', role: 'client' }),
    }),
    env(db),
    fetch,
    NOW,
  );
  assert.equal(res?.status, 200);
  const body = (await res!.json()) as { device?: { id: string }; host?: unknown };
  assert.equal(
    body.device?.id,
    key.id,
    'the envelope names which table it landed in -- and `PublicDevice` has a `kind` of its own, so it must not be flattened into one',
  );
  assert.equal(body.host, undefined);
  assert.deepEqual(
    rowOf(db, `SELECT user_id, kind FROM devices WHERE id = ?`, key.id),
    { user_id: 'user-a', kind: 'phone' },
    'a device code enrols into devices, never into hosts',
  );
  assert.equal(
    (rowOf(db, `SELECT COUNT(*) AS c FROM hosts`) as { c: number }).c,
    0,
    'and the wrong-role attempt left nothing behind either',
  );
  db.close();
});

test('a revoked key cannot re-enrol itself, and the fresh code survives the refusal', async () => {
  // The schema's own rule: revocation is a positive statement, so a machine
  // that could re-enrol with a new code has un-revoked itself.
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const key = await testKey(7);
  const first = await mint(db, cookie, 'host');
  await routeApi(
    daemonPost('/api/enroll/claim', {
      code: first.code,
      hostId: key.id,
      label: 'andy-mac',
      sig: await signEnrollment({ key, code: first.code, label: 'andy-mac' }),
    }),
    env(db),
    fetch,
    NOW,
  );
  await routeApi(post(`/api/hosts/${key.id}/revoke`, {}, cookie), env(db), fetch, NOW);

  const second = await mint(db, cookie, 'host');
  const res = await routeApi(
    daemonPost('/api/enroll/claim', {
      code: second.code,
      hostId: key.id,
      label: 'andy-mac',
      sig: await signEnrollment({ key, code: second.code, label: 'andy-mac' }),
    }),
    env(db),
    fetch,
    NOW + 1,
  );
  assert.equal(res?.status, 409);
  assert.deepEqual(await res!.json(), { error: 'already_enrolled' });
  assert.deepEqual(
    rowOf(db, `SELECT used_at FROM enroll_codes WHERE code = ?`, second.code),
    { used_at: null },
    'the refusal is about the key, so the code is still the person’s to use elsewhere',
  );
  db.close();
});

test('another account cannot take over a key that is already enrolled', async () => {
  const db = testDb();
  const mine = await signedIn(db, 'user-a');
  const theirs = await signedIn(db, 'user-b');
  const key = await testKey(7);

  const first = await mint(db, mine, 'host');
  await routeApi(
    daemonPost('/api/enroll/claim', {
      code: first.code,
      hostId: key.id,
      label: 'andy-mac',
      sig: await signEnrollment({ key, code: first.code, label: 'andy-mac' }),
    }),
    env(db),
    fetch,
    NOW,
  );

  const second = await mint(db, theirs, 'host');
  const res = await routeApi(
    daemonPost('/api/enroll/claim', {
      code: second.code,
      hostId: key.id,
      label: 'not-yours',
      sig: await signEnrollment({ key, code: second.code, label: 'not-yours' }),
    }),
    env(db),
    fetch,
    NOW + 1,
  );
  assert.equal(res?.status, 409);
  assert.deepEqual(
    rowOf(db, `SELECT user_id, label FROM hosts WHERE id = ?`, key.id),
    { user_id: 'user-a', label: 'andy-mac' },
    'holding a key is not a claim on somebody else’s row',
  );
  db.close();
});

test('re-enrolling the same machine renames it and keeps the date it joined', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const key = await testKey(7);
  const first = await mint(db, cookie, 'host');
  await routeApi(
    daemonPost('/api/enroll/claim', {
      code: first.code,
      hostId: key.id,
      label: 'andy-mac',
      sig: await signEnrollment({ key, code: first.code, label: 'andy-mac' }),
    }),
    env(db),
    fetch,
    NOW,
  );

  const second = await mint(db, cookie, 'host', NOW + 60_000);
  const res = await routeApi(
    daemonPost('/api/enroll/claim', {
      code: second.code,
      hostId: key.id,
      label: 'andy-mac-mini',
      sig: await signEnrollment({ key, code: second.code, label: 'andy-mac-mini' }),
    }),
    env(db),
    fetch,
    NOW + 60_000,
  );

  assert.equal(res?.status, 200);
  assert.deepEqual(
    rowOf(db, `SELECT label, enrolled_at FROM hosts WHERE id = ?`, key.id),
    { label: 'andy-mac-mini', enrolled_at: NOW },
    'it is the same machine proving the same key, so "yours since" must not move',
  );
  db.close();
});

// --- what the claim refuses before it touches the database -----------------

test('a malformed claim is refused on shape, before the code is even looked up', async () => {
  // Two earlier versions of this test could not fail, and the second is the
  // more instructive.
  //
  // The first built every case from a code that was never minted, so all of
  // them 400'd at the lookup regardless of the guards they named.
  //
  // The second used a *live* code — and still could not fail, because a
  // malformed code that reaches the lookup is `invalid_code`, which is also
  // 400. Asserting the status can never distinguish "rejected on shape" from
  // "rejected on lookup" when both answer the same way.
  //
  // So it asserts the thing the name actually claims: that no query is built
  // at all. That is also the property worth having — an unauthenticated
  // endpoint must not let a malformed body cost a database round trip.
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const { code } = await mint(db, cookie, 'host');
  const key = await testKey(7);
  const sig = await signEnrollment({ key, code, label: 'andy-mac' });
  const ok = { code, hostId: key.id, label: 'andy-mac', sig };

  const cases: Array<[string, Record<string, unknown>]> = [
    ['a code outside the alphabet', { ...ok, code: 'ABCD0OI1' }],
    ['a code of the wrong length', { ...ok, code: 'ABCD' }],
    ['a key that is not 32 bytes of hex', { ...ok, hostId: `${key.id}ff` }],
    [
      'an uppercase key -- a different primary key, not the same machine',
      { ...ok, hostId: key.id.toUpperCase() },
    ],
    ['a signature that is not 64 bytes of hex', { ...ok, sig: 'a'.repeat(127) }],
    ['an empty label', { ...ok, label: '' }],
    ['a label carrying control characters', { ...ok, label: 'andy\u001b[2Jmac' }],
    ['a device kind nobody renders', { ...ok, deviceKind: 'toaster' }],
  ];

  for (const [why, body] of cases) {
    let queries = 0;
    const counting = { ...db, prepare: (sql: string) => (queries++, db.prepare(sql)) };
    const res = await routeApi(daemonPost('/api/enroll/claim', body), env(counting), fetch, NOW);
    assert.equal(res?.status, 400, why);
    assert.equal(queries, 0, `${why}: the body was rejected only after a query`);
  }

  // The control, last: unmutated, the same body enrols — so a 400 above can
  // only have come from the field that was changed, and the setup is sound.
  const good = await routeApi(daemonPost('/api/enroll/claim', ok), env(db), fetch, NOW);
  assert.equal(good?.status, 200, 'the unmutated body must actually enrol');
  db.close();
});

test('a label is measured in code points, not UTF-16 units', async () => {
  // `label.length` counts code units, so one astral-plane emoji counts as two
  // and a 64-character name is refused at what looks like the wrong size.
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const { code } = await mint(db, cookie, 'host');
  const key = await testKey(7);
  const label = '🖥'.repeat(64);

  const res = await routeApi(
    daemonPost('/api/enroll/claim', {
      code,
      hostId: key.id,
      label,
      sig: await signEnrollment({ key, code, label }),
    }),
    env(db),
    fetch,
    NOW,
  );
  assert.equal(res?.status, 200, '64 emoji is 64 characters, however many code units that is');
  db.close();
});

// --- CSRF ------------------------------------------------------------------

test('the claim is exempt from the Origin check, and nothing else is', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');

  const originless = await routeApi(
    daemonPost('/api/enroll/claim', { code: 'ABCD2345' }),
    env(db),
    fetch,
    NOW,
  );
  assert.notEqual(originless?.status, 403, 'a daemon has no Origin header and never will');

  for (const path of ['/api/enroll/code', '/api/hosts/abc/revoke']) {
    const res = await routeApi(
      new Request(`${ORIGIN}${path}`, {
        method: 'POST',
        headers: { 'content-type': 'application/json', cookie },
        body: '{}',
      }),
      env(db),
      fetch,
      NOW,
    );
    assert.equal(res?.status, 403, `${path} still has a cookie to forge, so it keeps the check`);
  }
  db.close();
});

test('the claim still refuses a form POST', async () => {
  // Dropping Origin does not drop the content-type half: a form POST is the one
  // cross-site request a browser makes without a preflight, and a form cannot
  // set application/json. What an attacker can send with curl is their problem;
  // what a victim's browser can be made to send is ours.
  const db = testDb();
  for (const ct of ['application/x-www-form-urlencoded', 'multipart/form-data', 'text/plain']) {
    const res = await routeApi(
      new Request(`${ORIGIN}/api/enroll/claim`, {
        method: 'POST',
        headers: { 'content-type': ct },
        body: 'code=ABCD2345',
      }),
      env(db),
      fetch,
      NOW,
    );
    assert.equal(res?.status, 403, ct);
  }
  db.close();
});

test('the enrolment routes are POST-only', async () => {
  const db = testDb();
  for (const path of ['/api/enroll/code', '/api/enroll/claim']) {
    const res = await routeApi(new Request(`${ORIGIN}${path}`), env(db), fetch, NOW);
    assert.equal(res?.status, 405, path);
  }
  db.close();
});

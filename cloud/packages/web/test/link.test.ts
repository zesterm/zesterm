/**
 * The link flow end to end: `/api/link/start`, the approval page's read,
 * approve/deny, and `/api/link/claim` — against the real migrations and real
 * Ed25519.
 *
 * The two ORIGINLESS routes get the code claim's scrutiny: what is asserted
 * is ordering and what is *not* consulted — that a signature refusal writes
 * and burns nothing, that the session cookie decides nothing on either, that
 * every way a grant can be dead is one collapsed answer, and that the CAS
 * admits exactly one claim. The pending answer is asserted as a 200, because
 * waiting is this endpoint's ordinary state, not an error.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { looksLikeMachineToken, SESSION_COOKIE } from '@zesterm/cloud-shared';

import { createSession } from '../src/db/sessions.ts';
import { LINK_GRANT_TTL_MS } from '../src/db/link.ts';
import { routeApi } from '../src/router.ts';
import { rowOf, seedUser, testDb, type TestDb } from './d1.ts';
import { signLinkClaim, signLinkRequest, testKey, type TestKey } from './keys.ts';
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

/** The app: JSON, and deliberately no origin and no cookie. */
function appPost(path: string, body: unknown): Request {
  return new Request(`${ORIGIN}${path}`, {
    method: 'POST',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(body),
  });
}

function get(path: string, cookie?: string): Request {
  const headers: Record<string, string> = {};
  if (cookie !== undefined) headers['cookie'] = cookie;
  return new Request(`${ORIGIN}${path}`, { headers });
}

async function signedIn(db: TestDb, userId: string): Promise<string> {
  seedUser(db, userId);
  const { token } = await createSession(db, userId, NOW);
  return `${SESSION_COOKIE}=${token}`;
}

/** Start a grant for `key` the way the app does, and hand back its id. */
async function started(db: TestDb, key: TestKey, label = 'andy-desktop', now = NOW): Promise<string> {
  const res = await routeApi(
    appPost('/api/link/start', {
      deviceId: key.id,
      label,
      platform: 'macos',
      sig: await signLinkRequest(key, label),
    }),
    env(db),
    fetch,
    now,
  );
  assert.equal(res?.status, 200, 'starting a grant must succeed for a proven key');
  const { grant } = (await res!.json()) as { grant: string };
  return grant;
}

async function claim(db: TestDb, key: TestKey, grant: string, now = NOW): Promise<Response> {
  const res = await routeApi(
    appPost('/api/link/claim', { grant, deviceId: key.id, sig: await signLinkClaim(key, grant) }),
    env(db),
    fetch,
    now,
  );
  assert.ok(res, 'the claim route must answer');
  return res;
}

// --- the whole flow --------------------------------------------------------

test('open, approve, done: the app is enrolled into the approver’s account', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const key = await testKey(7);
  const grant = await started(db, key);

  // The approval page's read: what the person compares against the app.
  const details = await routeApi(get(`/api/link/${grant}`, cookie), env(db), fetch, NOW);
  assert.equal(details?.status, 200);
  assert.deepEqual(await details!.json(), {
    label: 'andy-desktop',
    kind: 'desktop',
    platform: 'macos',
    fingerprint: key.id.slice(0, 8),
    approved: false,
    expiresAt: NOW + LINK_GRANT_TTL_MS,
  });

  // Polling before approval is the ordinary state, and it is a 200: an app
  // treating non-200 as fatal must not give up while the person walks to the
  // browser.
  const waiting = await claim(db, key, grant);
  assert.equal(waiting.status, 200);
  assert.deepEqual(await waiting.json(), { status: 'pending' });

  const approve = await routeApi(post(`/api/link/${grant}/approve`, {}, cookie), env(db), fetch, NOW);
  assert.equal(approve?.status, 200);
  assert.deepEqual(await approve!.json(), { approvedAt: NOW });

  const res = await claim(db, key, grant, NOW + 3_000);
  assert.equal(res.status, 200);
  const answer = (await res.json()) as {
    device: { id: string; status: string };
    token: string;
    account: string;
  };
  assert.equal(answer.device.id, key.id);
  assert.equal(
    answer.device.status,
    'approved',
    'born approved: the person clicked Approve in their session, the same explicit act a typed code is',
  );
  assert.ok(looksLikeMachineToken(answer.token), 'the claim must hand the app a credential');
  assert.equal(answer.account, 'user-a', 'the app prints "signed in as <account>"');

  assert.deepEqual(
    rowOf(db, `SELECT user_id, status, approved_by FROM devices WHERE id = ?`, key.id),
    { user_id: 'user-a', status: 'approved', approved_by: null },
    'approved_by NULL is code-or-bootstrap territory: this approval came from a session, not a device',
  );

  // And the credential actually works, as the device it says it is.
  const hosts = await routeApi(
    new Request(`${ORIGIN}/api/hosts`, { headers: { authorization: `Bearer ${answer.token}` } }),
    env(db),
    fetch,
    NOW + 4_000,
  );
  assert.equal(hosts?.status, 200, 'the minted token is a working device credential');
  db.close();
});

test('asking again rotates: one live grant per key, and the old id dies', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const key = await testKey(7);
  const first = await started(db, key, 'first ask');
  const second = await started(db, key, 'second ask', NOW + 1_000);
  assert.notEqual(first, second, 'a fresh ask is a fresh capability');

  assert.deepEqual(
    rowOf(db, `SELECT COUNT(*) AS n FROM link_grants WHERE device_id = ?`, key.id),
    { n: 1 },
    'UNIQUE(device_id) makes the replace structural — an unauthenticated caller cannot farm rows',
  );

  const stale = await routeApi(get(`/api/link/${first}`, cookie), env(db), fetch, NOW + 1_000);
  assert.equal(stale?.status, 404, 'the replaced id no longer names anything');

  // An approval given to the first ask must not carry to the second: the
  // replace wiped approval state, so the fresh grant still reads pending.
  await routeApi(post(`/api/link/${second}/approve`, {}, cookie), env(db), fetch, NOW + 2_000);
  const third = await started(db, key, 'third ask', NOW + 3_000);
  const res = await claim(db, key, third, NOW + 4_000);
  assert.deepEqual(await res.json(), { status: 'pending' }, 'a fresh ask must not inherit an old approval');
  db.close();
});

// --- what start refuses, and what it never reads ---------------------------

test('a malformed start is refused on shape, before any query at all', async () => {
  const db = testDb();
  const key = await testKey(7);
  const ok = {
    deviceId: key.id,
    label: 'andy-desktop',
    platform: 'macos',
    sig: await signLinkRequest(key, 'andy-desktop'),
  };

  const cases: Array<[string, Record<string, unknown>]> = [
    ['a key that is not 32 bytes of hex', { ...ok, deviceId: `${key.id}ff` }],
    ['an uppercase key', { ...ok, deviceId: key.id.toUpperCase() }],
    ['an empty label', { ...ok, label: '' }],
    ['a label carrying control characters', { ...ok, label: 'andy[2Jdesktop' }],
    ['a kind nobody renders', { ...ok, kind: 'toaster' }],
    ['a platform carrying control characters', { ...ok, platform: 'macos[2J' }],
    ['a signature that is not 64 bytes of hex', { ...ok, sig: 'a'.repeat(127) }],
  ];
  for (const [why, body] of cases) {
    let queries = 0;
    const counting = { ...db, prepare: (sql: string) => (queries++, db.prepare(sql)) };
    const res = await routeApi(appPost('/api/link/start', body), env(counting), fetch, NOW);
    assert.equal(res?.status, 400, why);
    assert.equal(queries, 0, `${why}: an unauthenticated route must not let junk cost a round trip`);
  }

  const good = await routeApi(appPost('/api/link/start', ok), env(db), fetch, NOW);
  assert.equal(good?.status, 200, 'the unmutated body must actually start a grant');
  db.close();
});

test('a wrong start signature parks nothing', async () => {
  const db = testDb();
  const key = await testKey(7);
  const impostor = await testKey(8);
  const res = await routeApi(
    appPost('/api/link/start', {
      deviceId: key.id,
      label: 'andy-desktop',
      // A perfectly valid signature -- over the same bytes, by the wrong key.
      sig: await signLinkRequest(impostor, 'andy-desktop'),
    }),
    env(db),
    fetch,
    NOW,
  );
  assert.equal(res?.status, 400);
  assert.deepEqual(await res!.json(), { error: 'bad_signature' });
  assert.deepEqual(
    rowOf(db, `SELECT COUNT(*) AS n FROM link_grants`),
    { n: 0 },
    'the control plane must never park a grant for a key nobody proved',
  );
  db.close();
});

test('start and claim read no cookie: the approval decides the account, nothing else', async () => {
  // The ORIGINLESS invariant, exercised rather than stated. Both requests
  // carry user-b's perfectly valid session; the grant still lands in the
  // account of the user who APPROVED — a cookie on these routes must decide
  // nothing, or the exemption from the Origin check is a CSRF hole.
  const db = testDb();
  const approver = await signedIn(db, 'user-a');
  const bystander = await signedIn(db, 'user-b');
  const key = await testKey(7);

  const startRes = await routeApi(
    new Request(`${ORIGIN}/api/link/start`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', cookie: bystander },
      body: JSON.stringify({
        deviceId: key.id,
        label: 'andy-desktop',
        sig: await signLinkRequest(key, 'andy-desktop'),
      }),
    }),
    env(db),
    fetch,
    NOW,
  );
  assert.equal(startRes?.status, 200, 'no Origin plus a cookie must not 403: the route is ORIGINLESS');
  const { grant } = (await startRes!.json()) as { grant: string };

  await routeApi(post(`/api/link/${grant}/approve`, {}, approver), env(db), fetch, NOW);

  const claimRes = await routeApi(
    new Request(`${ORIGIN}/api/link/claim`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', cookie: bystander },
      body: JSON.stringify({ grant, deviceId: key.id, sig: await signLinkClaim(key, grant) }),
    }),
    env(db),
    fetch,
    NOW,
  );
  assert.equal(claimRes?.status, 200);
  assert.deepEqual(
    rowOf(db, `SELECT user_id FROM devices WHERE id = ?`, key.id),
    { user_id: 'user-a' },
    'user-b’s cookie riding along must not redirect the device into user-b’s account',
  );
  db.close();
});

// --- the approval page and its verbs ---------------------------------------

test('the grant read requires a session, and every dead grant is the same 404', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const key = await testKey(7);
  const grant = await started(db, key);

  const anonymous = await routeApi(get(`/api/link/${grant}`), env(db), fetch, NOW);
  assert.equal(anonymous?.status, 401, 'an anonymous reader would turn grant ids into a probe');

  const unknown = await routeApi(get(`/api/link/${'A'.repeat(43)}`, cookie), env(db), fetch, NOW);
  const junk = await routeApi(get(`/api/link/not-a-grant`, cookie), env(db), fetch, NOW);
  const expired = await routeApi(get(`/api/link/${grant}`, cookie), env(db), fetch, NOW + LINK_GRANT_TTL_MS);
  for (const [what, res] of [['unknown', unknown], ['junk', junk], ['expired', expired]] as const) {
    assert.equal(res?.status, 404, `${what} must be indistinguishable from never-issued`);
    assert.deepEqual(await res!.json(), { error: 'not_found' });
  }
  db.close();
});

test('approve is idempotent, and a second account cannot steal an approved grant', async () => {
  const db = testDb();
  const first = await signedIn(db, 'user-a');
  const second = await signedIn(db, 'user-b');
  const key = await testKey(7);
  const grant = await started(db, key);

  await routeApi(post(`/api/link/${grant}/approve`, {}, first), env(db), fetch, NOW);
  const again = await routeApi(post(`/api/link/${grant}/approve`, {}, second), env(db), fetch, NOW + 1_000);
  assert.equal(again?.status, 200);
  assert.deepEqual(
    await again!.json(),
    { approvedAt: NOW },
    'COALESCE keeps the first approval: a race of two sessions must not move the account',
  );

  const res = await claim(db, key, grant, NOW + 2_000);
  assert.equal(res.status, 200);
  assert.deepEqual(
    rowOf(db, `SELECT user_id FROM devices WHERE id = ?`, key.id),
    { user_id: 'user-a' },
    'the device lands with whoever approved first',
  );
  db.close();
});

test('approve keeps the full CSRF rule and a session requirement', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const key = await testKey(7);
  const grant = await started(db, key);

  const noCookie = await routeApi(post(`/api/link/${grant}/approve`, {}), env(db), fetch, NOW);
  assert.equal(noCookie?.status, 401);

  const noOrigin = await routeApi(
    new Request(`${ORIGIN}/api/link/${grant}/approve`, {
      method: 'POST',
      headers: { 'content-type': 'application/json', cookie },
      body: '{}',
    }),
    env(db),
    fetch,
    NOW,
  );
  assert.equal(
    noOrigin?.status,
    403,
    'approval is the account-holder’s act — a cross-site page with their cookie dies at the origin check',
  );

  const unknown = await routeApi(post(`/api/link/${'A'.repeat(43)}/approve`, {}, cookie), env(db), fetch, NOW);
  assert.equal(unknown?.status, 404);
  db.close();
});

test('deny deletes: the claim finds nothing, and the key may ask again', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const key = await testKey(7);
  const grant = await started(db, key);

  const deny = await routeApi(post(`/api/link/${grant}/deny`, {}, cookie), env(db), fetch, NOW);
  assert.equal(deny?.status, 200);
  assert.deepEqual(await deny!.json(), { denied: true });

  const res = await claim(db, key, grant);
  assert.equal(res.status, 400);
  assert.deepEqual(
    await res.json(),
    { error: 'invalid_grant' },
    'denied and never-issued must be the same answer to the app',
  );

  const again = await routeApi(post(`/api/link/${grant}/deny`, {}, cookie), env(db), fetch, NOW);
  assert.equal(again?.status, 404, 'the second deny finds nothing to deny, which is honest');

  const fresh = await started(db, key, 'asking again', NOW + 1_000);
  assert.ok(fresh, 'deletion frees the key to ask again with one click');
  db.close();
});

// --- what the claim refuses ------------------------------------------------

test('every dead grant is one collapsed claim refusal, byte for byte', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const key = await testKey(7);
  const stranger = await testKey(8);

  // A grant that was claimed, one that expired, one that never existed, and
  // someone else's unapproved grant claimed by a different (proven!) key.
  const spent = await started(db, key);
  await routeApi(post(`/api/link/${spent}/approve`, {}, cookie), env(db), fetch, NOW);
  assert.equal((await claim(db, key, spent)).status, 200);

  const answers: string[] = [];
  for (const [name, res] of [
    ['already claimed', await claim(db, key, spent, NOW + 1_000)],
    ['never issued', await claim(db, key, 'A'.repeat(43), NOW + 1_000)],
    // The wrong-device case must NOT leak "pending": only the key that asked
    // may learn the grant is waiting.
    ['someone else’s grant', await claim(db, stranger, await started(db, key, 'again', NOW + 2_000), NOW + 3_000)],
  ] as const) {
    assert.equal(res.status, 400, name);
    answers.push(JSON.stringify(await res.json()));
  }
  const expired = await started(db, key, 'expiring', NOW + 4_000);
  const late = await claim(db, key, expired, NOW + 4_000 + LINK_GRANT_TTL_MS);
  assert.equal(late.status, 400, 'expiry is inclusive: at the stated instant it is already gone');
  answers.push(JSON.stringify(await late.json()));

  assert.equal(
    new Set(answers).size,
    1,
    'four different deaths, one answer — a grant id must not be a liveness oracle',
  );
  db.close();
});

test('a wrong claim signature neither spends the grant nor reveals it', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const key = await testKey(7);
  const impostor = await testKey(8);
  const grant = await started(db, key);
  await routeApi(post(`/api/link/${grant}/approve`, {}, cookie), env(db), fetch, NOW);

  const forged = await routeApi(
    appPost('/api/link/claim', {
      grant,
      deviceId: key.id,
      // Valid Ed25519 over the right bytes, by the wrong key.
      sig: await signLinkClaim(impostor, grant),
    }),
    env(db),
    fetch,
    NOW,
  );
  assert.equal(forged?.status, 400);
  assert.deepEqual(await forged!.json(), { error: 'invalid_grant' });

  const res = await claim(db, key, grant, NOW + 1_000);
  assert.equal(res.status, 200, 'the rejected forgery must leave the grant for the key that asked');
  db.close();
});

test('one grant admits one device, however many claims race', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const key = await testKey(7);
  const grant = await started(db, key);
  await routeApi(post(`/api/link/${grant}/approve`, {}, cookie), env(db), fetch, NOW);

  assert.equal((await claim(db, key, grant)).status, 200);
  const replay = await claim(db, key, grant, NOW + 1);
  assert.equal(replay.status, 400, 'a captured claim is a complete credential until the CAS spends it');
  assert.deepEqual(
    rowOf(db, `SELECT COUNT(*) AS n FROM devices`),
    { n: 1 },
    'one grant, one device',
  );
  db.close();
});

test('a revoked key cannot re-link itself, and the refusal leaves the grant alive', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  const key = await testKey(7);

  const first = await started(db, key);
  await routeApi(post(`/api/link/${first}/approve`, {}, cookie), env(db), fetch, NOW);
  assert.equal((await claim(db, key, first)).status, 200);
  await routeApi(post(`/api/devices/${key.id}/revoke`, {}, cookie), env(db), fetch, NOW + 1_000);

  const second = await started(db, key, 'coming back', NOW + 2_000);
  await routeApi(post(`/api/link/${second}/approve`, {}, cookie), env(db), fetch, NOW + 3_000);
  const res = await claim(db, key, second, NOW + 4_000);
  assert.equal(res.status, 409, 'revocation is a positive statement; a fresh grant does not un-revoke');
  assert.deepEqual(await res!.json(), { error: 'already_enrolled' });
  assert.deepEqual(
    rowOf(db, `SELECT claimed_at FROM link_grants WHERE id = ?`, second),
    { claimed_at: null },
    'the refusal is about the key, so the grant is left for whoever debugs it',
  );
  db.close();
});

test('the link routes answer only their methods', async () => {
  const db = testDb();
  for (const path of ['/api/link/start', '/api/link/claim']) {
    const res = await routeApi(new Request(`${ORIGIN}${path}`), env(db), fetch, NOW);
    assert.equal(res?.status, 405, `${path} is POST-only, and GET must not read it as a grant id`);
  }
  const res = await routeApi(
    new Request(`${ORIGIN}/api/link/${'A'.repeat(43)}`, { method: 'POST', headers: { origin: ORIGIN, 'content-type': 'application/json' }, body: '{}' }),
    env(db),
    fetch,
    NOW,
  );
  assert.equal(res?.status, 405, 'the details read is GET-only');
  db.close();
});

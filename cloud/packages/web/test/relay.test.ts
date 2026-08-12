/**
 * `POST /api/relay/ticket`.
 *
 * Two things are being proved. The **posture** is `registry.test.ts`'s, because
 * the route is `api/registry.ts`'s line for line: signed out is 401, a host
 * belonging to somebody else is the same 404 as one that never existed, and a
 * malformed id costs no round trip. The **ticket** is the new part: real
 * Ed25519 over the shared preimage, so what this mint produces is what
 * `packages/relay` verifies. Neither package imports the other; the bytes are
 * `@zesterm/cloud-shared`'s, once.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { getPublicKeyAsync, verifyAsync } from '@noble/ed25519';
import {
  ATTACH_TICKET_TTL_MS,
  attachTicketPreimage,
  decodeAttachTicket,
  hex,
  sessionIdOf,
  SESSION_COOKIE,
} from '@zesterm/cloud-shared';

import { createSession } from '../src/db/sessions.ts';
import { mintAttachTicket } from '../src/relay/ticket.ts';
import { routeApi } from '../src/router.ts';
import { seedUser, testDb, type TestDb } from './d1.ts';
import type { Env } from '../src/env.ts';

const ORIGIN = 'https://zesterm.sigx.workers.dev';
const NOW = 1_700_000_000_000;

const MAC = 'aa'.repeat(32);
const WINDOWS = 'bb'.repeat(32);

/** The account service's key. A seed of fixed bytes, so a failure names one key. */
const SEED = new Uint8Array(32).fill(7);
const SEED_HEX = hex(SEED);

/**
 * `null` means the binding is absent — not `undefined`, which a default
 * parameter cannot tell from "not passed" and would have quietly given every
 * caller the key they were trying to do without.
 */
function env(db: TestDb, signingKey: string | null = SEED_HEX): Env {
  return {
    ASSETS: { fetch: async () => new Response('assets') },
    DB: db,
    APP_ORIGIN: ORIGIN,
    GITHUB_CLIENT_ID: 'client-id',
    GITHUB_CLIENT_SECRET: 'client-secret',
    COOKIE_MAC_KEY: 'mac-key',
    ...(signingKey === null ? {} : { TICKET_SIGNING_KEY: signingKey }),
  };
}

async function signedIn(db: TestDb, userId: string): Promise<string> {
  seedUser(db, userId);
  const { token } = await createSession(db, userId, NOW);
  return `${SESSION_COOKIE}=${token}`;
}

function seedHost(
  db: TestDb,
  args: { id: string; userId: string; label: string; revokedAt?: number },
): void {
  db.raw
    .prepare(
      `INSERT INTO hosts (id, user_id, label, platform, enrolled_at, revoked_at)
       VALUES (?, ?, ?, 'macos', ?, ?)`,
    )
    .run(args.id, args.userId, args.label, NOW, args.revokedAt ?? null);
}

function post(body: unknown, cookie?: string): Request {
  const headers: Record<string, string> = { origin: ORIGIN, 'content-type': 'application/json' };
  if (cookie !== undefined) headers['cookie'] = cookie;
  return new Request(`${ORIGIN}/api/relay/ticket`, {
    method: 'POST',
    headers,
    body: JSON.stringify(body),
  });
}

test('minting requires a session', async () => {
  const db = testDb();
  const res = await routeApi(post({ hostId: MAC }), env(db), fetch, NOW);
  assert.equal(res?.status, 401, 'the relay must never be reachable without the account that owns the host');
  db.close();
});

test('a malformed host id is refused before the registry is queried at all', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');

  // Every statement the route prepares, recorded. Asserting the status alone
  // would pass whether or not the shape check came first, and "costs no round
  // trip" is the claim — resolving the session is one query and unavoidable,
  // reaching `hosts` with an id no host could have is the one being ruled out.
  const prepared: string[] = [];
  const watched: Env = {
    ...env(db),
    DB: {
      prepare: (sql: string) => {
        prepared.push(sql);
        return db.prepare(sql);
      },
    },
  };

  for (const hostId of [
    MAC.slice(1),
    `${MAC}ff`,
    MAC.toUpperCase(),
    'zz'.repeat(32),
    42,
    null,
    undefined,
  ]) {
    const res = await routeApi(post({ hostId }, cookie), watched, fetch, NOW);
    assert.equal(res?.status, 400, `${String(hostId)} is not a host id`);
  }

  assert.ok(
    !prepared.some((sql) => sql.includes('hosts')),
    `an id that could not be any host's must not reach the registry; it prepared ${prepared.join(' | ')}`,
  );
  db.close();
});

test('somebody else’s machine is a 404, exactly like one that does not exist', async () => {
  // The ids are public keys. An endpoint that told the two apart would answer
  // "is this key enrolled with zesterm" for strangers' machines.
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  await signedIn(db, 'user-b');
  seedHost(db, { id: WINDOWS, userId: 'user-b', label: 'not-mine' });

  const theirs = await routeApi(post({ hostId: WINDOWS }, cookie), env(db), fetch, NOW);
  const nobodys = await routeApi(post({ hostId: MAC }, cookie), env(db), fetch, NOW);

  assert.equal(theirs?.status, 404);
  assert.deepEqual(
    await theirs?.json(),
    await nobodys?.json(),
    'the body must not distinguish them either',
  );
  db.close();
});

test('a revoked machine cannot be attached to', async () => {
  // Revocation is what a person clicks when a laptop is stolen. A ticket minted
  // afterwards would be a pipe to it, thirty seconds at a time, for as long as
  // the session cookie lives.
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  seedHost(db, { id: MAC, userId: 'user-a', label: 'andy-mac', revokedAt: NOW - 1 });

  const res = await routeApi(post({ hostId: MAC }, cookie), env(db), fetch, NOW);
  assert.equal(res?.status, 404, 'a revoked host is gone, not merely hidden from the listing');
  db.close();
});

test('the ticket names the host, the account and the session, and dies in thirty seconds', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  seedHost(db, { id: MAC, userId: 'user-a', label: 'andy-mac' });

  const res = await routeApi(post({ hostId: MAC }, cookie), env(db), fetch, NOW);
  assert.equal(res?.status, 200);
  const body = (await res?.json()) as { ticket: string; expiresAt: number };

  assert.equal(
    body.expiresAt,
    NOW + ATTACH_TICKET_TTL_MS,
    'the caller is told when to stop trying, and it is the same instant the relay stops accepting',
  );

  const decoded = decodeAttachTicket(body.ticket);
  assert.ok(decoded, 'the minted ticket must decode with the shared decoder the relay uses');
  assert.deepEqual(
    { ...decoded.ticket, jti: '<random>' },
    {
      v: 1,
      jti: '<random>',
      aud: 'relay',
      host: MAC,
      user: 'user-a',
      dev: await sessionIdOf(cookie.slice(SESSION_COOKIE.length + 1)),
      iat: NOW,
      exp: NOW + ATTACH_TICKET_TTL_MS,
    },
    '`dev` is sha256(cookie) — the sessions row id, which is not a credential — because nothing in the schema links a session to a device',
  );

  assert.ok(
    await verifyAsync(decoded.signature, attachTicketPreimage(decoded.payload), await getPublicKeyAsync(SEED), {
      zip215: false,
    }),
    'the signature is over the shared preimage, or the relay refuses every ticket this Worker mints and the error names neither side',
  );
  db.close();
});

test('two mints for the same host differ, so the relay’s replay set can tell them apart', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  seedHost(db, { id: MAC, userId: 'user-a', label: 'andy-mac' });

  const one = await routeApi(post({ hostId: MAC }, cookie), env(db), fetch, NOW);
  const two = await routeApi(post({ hostId: MAC }, cookie), env(db), fetch, NOW);
  const jti = async (res: Response | null) =>
    decodeAttachTicket(((await res?.json()) as { ticket: string }).ticket)?.ticket.jti;

  assert.notEqual(
    await jti(one),
    await jti(two),
    'the relay spends a jti once, so a mint that repeated one would refuse the second honest attach of a pair — two tabs on one machine, and the second never opens',
  );
  db.close();
});

test('a deployment with no signing key says so, and only after ownership', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  seedHost(db, { id: MAC, userId: 'user-a', label: 'andy-mac' });

  const mine = await routeApi(post({ hostId: MAC }, cookie), env(db, null), fetch, NOW);
  assert.equal(mine?.status, 503, 'a forgotten `wrangler secret put` is a diagnosable answer, not a 500');

  const nobodys = await routeApi(post({ hostId: WINDOWS }, cookie), env(db, null), fetch, NOW);
  assert.equal(
    nobodys?.status,
    404,
    'the 503 must not arrive only for hosts that exist — that would make a missing secret an oracle for which machines are enrolled',
  );

  // A key of the wrong length is the same failure with a different cause.
  const short = await routeApi(post({ hostId: MAC }, cookie), env(db, 'ab'), fetch, NOW);
  assert.equal(short?.status, 503, 'a truncated secret must not become a signature nobody can verify');
  db.close();
});

test('a wrong-length signing key fails where it is passed, not inside the signature', async () => {
  // The route above never lets one through, so this guard is for the second
  // caller. It earns its line by naming the *signing key*: noble raises its own
  // RangeError from `signAsync` one line later, and that one says only
  // "expected Uint8Array of length 32", which points at a byte array rather
  // than at a `wrangler secret put` somebody got wrong.
  await assert.rejects(
    () => mintAttachTicket({ signingKey: new Uint8Array(2), host: MAC, user: 'user-a', dev: 'd', now: NOW }),
    /a signing key is 32 bytes, this one is 2/,
    'the mint must refuse a key it cannot sign with, in its own words',
  );
});

test('the route is a POST, and it is not exempt from the Origin check', async () => {
  const db = testDb();
  const cookie = await signedIn(db, 'user-a');
  seedHost(db, { id: MAC, userId: 'user-a', label: 'andy-mac' });

  const get = new Request(`${ORIGIN}/api/relay/ticket`, { headers: { cookie } });
  assert.equal((await routeApi(get, env(db), fetch, NOW))?.status, 405);

  // It reads the session cookie, so it must never join ORIGINLESS: a route that
  // does both is exactly the CSRF hole `csrfOkWithoutOrigin` dropped the
  // defence against. A cross-site POST carrying the victim's cookie is 403.
  const crossSite = new Request(`${ORIGIN}/api/relay/ticket`, {
    method: 'POST',
    headers: { origin: 'https://evil.example', 'content-type': 'application/json', cookie },
    body: JSON.stringify({ hostId: MAC }),
  });
  assert.equal(
    (await routeApi(crossSite, env(db), fetch, NOW))?.status,
    403,
    'a page on another origin must not be able to mint a pipe to this account’s machines',
  );
  db.close();
});

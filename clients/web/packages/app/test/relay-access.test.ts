/**
 * The browser's half of `POST /api/relay/ticket`.
 *
 * `cloud/packages/web/test/relay.test.ts` covers what the Worker answers; this
 * covers what the app asks and what it does with the answer. The two meet at
 * one request shape, and the part worth pinning is that this one satisfies the
 * Worker's CSRF rule — a mint that quietly 403s would look exactly like a fleet
 * that cannot connect.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import type { CloudBootstrap } from '../src/bootstrap.ts';
import { relayAccess } from '../src/relay-access.ts';

const HOST = 'ab'.repeat(32);

const cloud = (relayOrigin: string | null): CloudBootstrap => ({
  mode: 'cloud',
  user: null,
  relayOrigin,
});

/** A `fetch` that records the one call it is given and answers with `body`. */
function stubFetch(body: unknown, status = 200) {
  const calls: Array<{ url: string; init: RequestInit | undefined }> = [];
  const impl = (async (url: string, init?: RequestInit) => {
    calls.push({ url, init });
    return new Response(JSON.stringify(body), {
      status,
      headers: { 'content-type': 'application/json' },
    });
  }) as unknown as typeof fetch;
  return { impl, calls };
}

test('no relay origin is no relay, which is an ordinary deployment', () => {
  assert.equal(
    relayAccess(cloud(null)),
    null,
    'a build with no relay still reaches every machine it can see over ws — this must not be an error state',
  );
});

test('the mint is a same-origin JSON POST, because that is what the Worker demands', async () => {
  const { impl, calls } = stubFetch({ ticket: 'tkt-123', expiresAt: 1 });
  const access = relayAccess(cloud('https://relay.example.com'), impl);
  assert.ok(access, 'a relay origin must produce access');

  assert.equal(await access.mintTicket(HOST), 'tkt-123');
  assert.equal(calls.length, 1, 'one attach, one mint — a ticket is spent by the socket that follows it');

  const [call] = calls;
  assert.equal(
    call?.url,
    '/api/relay/ticket',
    'relative, so the cookie goes with it: the mint is same-origin and the relay is not, which is the entire reason a ticket exists',
  );
  assert.equal(call?.init?.method, 'POST');
  assert.deepEqual(
    call?.init?.headers,
    { 'content-type': 'application/json' },
    'the Worker refuses a state-changing request without it — a form POST is the one cross-site request a browser makes without a preflight, and it cannot set this',
  );
  assert.deepEqual(JSON.parse(String(call?.init?.body)), { hostId: HOST });
});

test('the relay origin is carried through untouched, for relayDial to turn into a URL', async () => {
  const { impl } = stubFetch({ ticket: 't' });
  assert.equal(relayAccess(cloud('http://localhost:8788'), impl)?.origin, 'http://localhost:8788');
});

test('a refused or nonsensical mint rejects, which the dial already treats as a dropped dial', async () => {
  // `relayDial`'s 200ms→5s ladder retries exactly this: a signed-out tab, a
  // revoked host, a relay that was never configured. Returning a sentinel
  // instead would put an empty subprotocol on the wire and move the failure a
  // round trip later, where nothing names it.
  for (const [body, status, why] of [
    [{ error: 'unauthorized' }, 401, 'signed out'],
    [{ error: 'not_found' }, 404, 'a host this account does not own'],
    [{ error: 'relay_unavailable' }, 503, 'no signing key deployed'],
    [{ expiresAt: 1 }, 200, 'a 200 carrying no ticket'],
    [{ ticket: '' }, 200, 'an empty ticket, which is a legal JSON string and an illegal subprotocol'],
    [{ ticket: 42 }, 200, 'a ticket that is not a string'],
    // The status is the authority, not the body. Every refusal above happens to
    // carry no ticket, so without this one the status check could be deleted and
    // nothing would notice -- and what would then reach the relay is whatever an
    // error page happened to have a `ticket` field in.
    [{ ticket: 'tkt-oops' }, 503, 'a refusal whose body is ticket-shaped anyway'],
  ] as Array<[unknown, number, string]>) {
    const { impl } = stubFetch(body, status);
    const access = relayAccess(cloud('https://relay.example.com'), impl);
    await assert.rejects(async () => access?.mintTicket(HOST), why);
  }
});

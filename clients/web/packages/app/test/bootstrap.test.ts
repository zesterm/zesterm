import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  fetchBootstrap,
  parseBootstrap,
  parseUser,
  FALLBACK,
  type Bootstrap,
} from '../src/bootstrap.ts';

/** A `fetch` that answers once with this body, so no server is needed. */
const serving = (body: unknown, status = 200): typeof fetch =>
  (async () =>
    new Response(JSON.stringify(body), {
      status,
      headers: { 'content-type': 'application/json' },
    })) as unknown as typeof fetch;

test('the sidecar and the Worker are both understood', async () => {
  // These two literals are the contract. The type is deliberately duplicated
  // in cloud/packages/web/src/bootstrap.ts rather than shared across the two
  // lockfiles, so this is where the shapes are pinned to each other -- and
  // `cloud`'s own router test asserts its server emits exactly the second one.
  assert.deepEqual(parseBootstrap({ mode: 'local' }), { mode: 'local' });
  assert.deepEqual(parseBootstrap({ mode: 'cloud', user: null }), { mode: 'cloud', user: null });
});

test('an unrecognised body is rejected rather than half-believed', () => {
  // A future server mode this build has never heard of must not be coerced
  // into one of the two it knows -- guessing "cloud" would sign someone out of
  // a local session, and guessing "local" would dial a daemon that is not there.
  for (const bad of [null, undefined, 42, 'local', {}, { mode: 'enterprise' }, { mode: 1 }]) {
    assert.equal(parseBootstrap(bad), null, `${JSON.stringify(bad)} should not parse`);
  }
});

test('a server that answers is believed', async () => {
  const got = await fetchBootstrap(serving({ mode: 'cloud', user: null }));
  assert.deepEqual(got, { mode: 'cloud', user: null } satisfies Bootstrap);
});

test('boot never throws, whatever the network does', async () => {
  // A boot path that can reject is a boot path that renders a blank page. The
  // three ways this realistically fails, all of which must land on FALLBACK:
  const rejecting = (async () => {
    throw new Error('offline');
  }) as unknown as typeof fetch;
  const notJson = (async () => new Response('<!doctype html>')) as unknown as typeof fetch;

  assert.deepEqual(await fetchBootstrap(rejecting), FALLBACK, 'network error');
  assert.deepEqual(await fetchBootstrap(serving({}, 500)), FALLBACK, 'server error');
  assert.deepEqual(await fetchBootstrap(notJson), FALLBACK, 'SPA fallback served HTML');
});

test('a server that accepts and never answers cannot hang boot', async () => {
  // The captive-portal case: the connection is accepted, the request never
  // resolves. Nothing mounts until this returns, so without the timeout the
  // page stays blank forever -- the one duration a splash cannot cover.
  const stalling: typeof fetch = ((_url: string, init?: RequestInit) =>
    new Promise((_resolve, reject) => {
      init?.signal?.addEventListener('abort', () => reject(new Error('aborted')));
    })) as unknown as typeof fetch;

  const started = Date.now();
  const got = await fetchBootstrap(stalling, '/api/bootstrap', 60);
  assert.deepEqual(got, FALLBACK);
  assert.ok(
    Date.now() - started < 2_000,
    'fetchBootstrap must give up on its own rather than wait on the network',
  );
});

test('the request carries an abort signal, so nothing is left running', async () => {
  // `AbortSignal.timeout` rather than racing a timer: a race leaves the request
  // alive and its later rejection unhandled, surfacing as an unrelated console
  // error nobody can place.
  let seen: AbortSignal | null | undefined;
  const capturing: typeof fetch = ((_url: string, init?: RequestInit) => {
    seen = init?.signal;
    return Promise.resolve(new Response(JSON.stringify({ mode: 'local' })));
  }) as unknown as typeof fetch;

  await fetchBootstrap(capturing);
  assert.ok(seen instanceof AbortSignal, 'no signal was passed to fetch');
});

test('the fallback is local, because that is the path that still works', () => {
  // Assuming cloud would show a signed-out screen to someone on loopback,
  // where there is nothing to sign in to.
  assert.equal(FALLBACK.mode, 'local');
});

// --- the signed-in user ------------------------------------------------------

const USER = {
  id: 'u-1',
  displayName: 'Andy',
  email: 'andy@example.com',
  avatarUrl: 'https://example.com/a.png',
};

test('a signed-in user parses through the bootstrap envelope', () => {
  assert.deepEqual(parseBootstrap({ mode: 'cloud', user: USER }), {
    mode: 'cloud',
    user: USER,
  } satisfies Bootstrap);
});

test('the optional fields are allowed to be absent', () => {
  // GitHub accounts with a private email have no address, and not everyone has
  // an avatar. Neither is a reason to refuse the user.
  assert.deepEqual(parseUser({ id: 'u-1', displayName: 'Andy' }), {
    id: 'u-1',
    displayName: 'Andy',
    email: null,
    avatarUrl: null,
  });
  assert.deepEqual(parseUser({ ...USER, email: null, avatarUrl: null }), {
    ...USER,
    email: null,
    avatarUrl: null,
  });
});

test('a user missing its identifying fields is nobody, not a half-rendered one', () => {
  // The consequence is the point: `null` gates to /login. Accepting a partial
  // user would render an account menu with an undefined name and let the app
  // behave as though someone were signed in when the server did not say so.
  for (const bad of [
    null,
    undefined,
    'andy',
    42,
    {},
    { id: 'u-1' },
    { displayName: 'Andy' },
    { id: 1, displayName: 'Andy' },
    { id: 'u-1', displayName: 42 },
  ]) {
    assert.equal(parseUser(bad), null, `${JSON.stringify(bad)} should not be a user`);
  }
});

test('a cloud envelope carrying a malformed user is signed out, not rejected', () => {
  // The envelope is still understood -- this is the edge, and the app must
  // render /login rather than fall back to `local` and dial a daemon that is
  // not there.
  const got = parseBootstrap({ mode: 'cloud', user: { id: 'u-1' } });
  assert.deepEqual(got, { mode: 'cloud', user: null });
});

test('wrong-typed fields do not leak through as undefined', () => {
  // `email: 42` must become null rather than reaching the UI as a number that
  // renders as "42" next to the account name.
  assert.deepEqual(parseUser({ id: 'u-1', displayName: 'Andy', email: 42, avatarUrl: {} }), {
    id: 'u-1',
    displayName: 'Andy',
    email: null,
    avatarUrl: null,
  });
});

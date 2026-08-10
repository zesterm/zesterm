import { test } from 'node:test';
import assert from 'node:assert/strict';

import { fetchBootstrap, parseBootstrap, FALLBACK, type Bootstrap } from '../src/bootstrap.ts';

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

test('the fallback is local, because that is the path that still works', () => {
  // Assuming cloud would show a signed-out screen to someone on loopback,
  // where there is nothing to sign in to.
  assert.equal(FALLBACK.mode, 'local');
});

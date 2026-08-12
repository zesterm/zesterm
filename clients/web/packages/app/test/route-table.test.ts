import { test } from 'node:test';
import assert from 'node:assert/strict';

import { createMemoryHistory, createRouter } from '@sigx/router';

import { SHELL_CHILD_PATHS, SHELL_PATH } from '../src/route-table.ts';

// The exact record shape routes.tsx builds, minus the components — the
// RouterView key is derived from paths alone, so this is the whole mechanism.
const router = createRouter({
  history: createMemoryHistory(),
  routes: [{ path: SHELL_PATH, children: SHELL_CHILD_PATHS.map((path) => ({ path })) }],
});

test('every shell URL resolves through the same top-level record', () => {
  const hosts = router.resolve('/hosts');
  const host = router.resolve('/h/abc');
  const session = router.resolve('/h/abc/s/3');
  assert.equal(hosts.matched[0]?.path, SHELL_PATH);
  assert.equal(
    host.matched[0]?.path,
    SHELL_PATH,
    'RouterView keys the routed component by matched[0].path — a different key here remounts the Shell',
  );
  assert.equal(
    session.matched[0]?.path,
    SHELL_PATH,
    'crossing /hosts ↔ /h/…/s/… must not remount the Shell, or every open tab is discarded',
  );
});

test('the nested shape still hands the leaf params to useParams', () => {
  const session = router.resolve('/h/abc/s/3');
  assert.deepEqual(
    session.params,
    { hostId: 'abc', sessionId: '3' },
    'the route watcher activates tabs from these — an empty record would strand the URL',
  );
});

test('the child paths are absolute', () => {
  for (const path of SHELL_CHILD_PATHS) {
    assert.ok(
      path.startsWith('/h/'),
      `'${path}' must start with '/h/' — a relative child joins under ${SHELL_PATH} and never matches a real /h/… URL`,
    );
  }
});

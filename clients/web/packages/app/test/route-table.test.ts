import { test } from 'node:test';
import assert from 'node:assert/strict';

import { createMemoryHistory, createRouter } from '@sigx/router';

import { PROFILES_PATH, SHELL_CHILD_PATHS, SHELL_PATH, safeNextPath } from '../src/route-table.ts';

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

test('the params record is replaced on navigation, so it can never be captured', async () => {
  // The whole of #196 in four lines. `useParams()` is `useRoute().params`, and
  // the router swaps that record out on every navigation — so `const params =
  // useParams()` at setup pins the record the component was born with, and
  // reads from it are frozen for ever *and* register no dependency on the
  // route. `Shell.tsx` did exactly that: the URL said a session, the pane went
  // on rendering the session list, and only a full reload showed the terminal.
  //
  // Pinned here rather than at the seam because nothing in this workspace
  // renders a component: this is the router behaviour the Shell must respect,
  // and if a future version ever mutated the record in place instead, whoever
  // sees this fail should go and simplify Shell.tsx.
  const nav = createRouter({
    history: createMemoryHistory(),
    routes: [{ path: SHELL_PATH, children: SHELL_CHILD_PATHS.map((path) => ({ path })) }],
  });

  await nav.push('/h/abc/s/3');
  const captured = nav.currentRoute.params;
  await nav.push('/h/def/s/9');

  assert.deepEqual(
    captured,
    { hostId: 'abc', sessionId: '3' },
    'a captured record still names the route it was taken from',
  );
  assert.deepEqual(
    nav.currentRoute.params,
    { hostId: 'def', sessionId: '9' },
    'and only the route itself knows where we are — read through it, at each use',
  );
});

test('the child paths are absolute', () => {
  // The invariant is the leading slash, not the `/h/` prefix: the matcher
  // takes a '/'-prefixed child as-is and joins anything else under
  // `SHELL_PATH`, so a relative `profiles` would match `/hosts/profiles` and
  // never the URL that is actually navigated to. Asserting the prefix instead
  // read the same while there was only one family of children, and broke the
  // moment a screen that is not a machine joined them (#352).
  for (const path of SHELL_CHILD_PATHS) {
    assert.ok(
      path.startsWith('/'),
      `'${path}' must be absolute — a relative child joins under ${SHELL_PATH} and never matches the URL navigated to`,
    );
  }
  assert.ok(
    SHELL_CHILD_PATHS.includes(PROFILES_PATH),
    'the profiles screen is a CHILD of the shell record: as a sibling it carries a different RouterView key, so opening it would remount the Shell and discard every open tab',
  );
});

test('safeNextPath mirrors the Worker safeNext, plus the loop the server never sees', () => {
  // The same cases the Worker's own tests use, so the two vetting layers
  // cannot silently disagree about what a safe destination is.
  const cases: Array<[string | undefined, string, string]> = [
    [undefined, '/hosts', 'absent means the ordinary landing'],
    ['', '/hosts', 'empty is absent'],
    ['/link?grant=abc', '/link?grant=abc', 'the hand-off URL this exists to carry'],
    ['/themes', '/themes', 'any ordinary path passes through'],
    ['https://evil.example', '/hosts', 'an absolute URL is an open redirect'],
    ['//evil.example', '/hosts', 'protocol-relative is the case startsWith("/") misses'],
    ['/\\evil.example', '/hosts', 'the backslash spelling some browsers normalise to //'],
    ['/login', '/hosts', 'next pointing back at the gate would bounce forever'],
    ['/login?next=%2Fhosts', '/hosts', 'and so would the parameterised spelling'],
  ];
  for (const [raw, want, why] of cases) {
    assert.equal(safeNextPath(raw), want, why);
  }
});

test('the profiles screen shares the shell record key, so tabs survive opening it', () => {
  // The same mechanism the session URLs rely on, asserted through the real
  // router rather than by reading the table: RouterView keys the routed
  // component by `matched[0].path`, so two URLs whose first matched record
  // differs are two different mounts — and a remount here throws away every
  // open terminal, its socket and its scrollback.
  assert.equal(
    router.resolve(PROFILES_PATH).matched[0]?.path,
    SHELL_PATH,
    'opening the profiles screen must not remount the Shell',
  );
  assert.equal(
    router.resolve(PROFILES_PATH).matched.at(-1)?.path,
    PROFILES_PATH,
    'and it must still be the matched leaf, or the pane would never know it is there',
  );
});

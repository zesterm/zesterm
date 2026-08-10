/**
 * The directory actor on a real in-memory host — the same runtime the
 * sidecar runs, minus the sockets.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { createHost, memoryStorage, type Host } from '@sigx/actors/host';

import { LOCAL_DIRECTORY_KEY, SessionDirectory, type SessionEntry } from '../src/index.ts';

/** Timers quiet enough that a test process exits when it is done. */
const quiet = { sweepIntervalMs: 60_000, reminderTickMs: 60_000, callTimeoutMs: 0 };

async function directoryHost(): Promise<Host> {
  const host = createHost({
    actors: [SessionDirectory],
    storage: memoryStorage(),
    defaults: quiet,
  });
  await host.start();
  return host;
}

function entry(session: string, title: string): SessionEntry {
  return {
    host: '2e'.repeat(32),
    session,
    title,
    cwd: '/home',
    cols: 120,
    rows: 30,
    altScreen: false,
    attached: true,
  };
}

test('the feed replaces the list and readers see the daemon truth', async () => {
  const host = await directoryHost();
  try {
    const dir = host.actor(SessionDirectory, LOCAL_DIRECTORY_KEY);
    await dir.setLink(true, { id: '2e'.repeat(32), label: 'andy-mac' }, { kind: 'ws', host: '127.0.0.1', port: 7718 });
    await dir.replaceAll([entry('1', 'zsh'), entry('2', 'vim')], null);

    const view = await dir.list();
    assert.equal(view.connected, true);
    assert.equal(view.sessions.length, 2);
    assert.deepEqual(
      view.dataPlane,
      { kind: 'ws', host: '127.0.0.1', port: 7718 },
      'the app learns the data plane from the directory, discriminant and all',
    );

    // The daemon closed one; the push replaces rather than merges — a merge
    // would resurrect it.
    await dir.replaceAll([entry('2', 'vim')], null);
    const after = await dir.list();
    assert.deepEqual(after.sessions.map((s) => s.session), ['2']);
  } finally {
    await host.stop();
  }
});

test('a created session is remembered for select-on-create', async () => {
  const host = await directoryHost();
  try {
    const dir = host.actor(SessionDirectory, LOCAL_DIRECTORY_KEY);
    await dir.replaceAll([entry('7', 'new shell')], '7');
    assert.equal((await dir.list()).lastCreated, '7');
    // A later push without a create keeps the marker; only a newer create
    // moves it.
    await dir.replaceAll([entry('7', 'new shell')], null);
    assert.equal((await dir.list()).lastCreated, '7');
  } finally {
    await host.stop();
  }
});

test('a dropped daemon link empties the list rather than serving it stale', async () => {
  const host = await directoryHost();
  try {
    const dir = host.actor(SessionDirectory, LOCAL_DIRECTORY_KEY);
    await dir.setLink(true, { id: '2e'.repeat(32), label: 'andy-mac' }, { kind: 'ws', host: '127.0.0.1', port: 7718 });
    await dir.replaceAll([entry('1', 'zsh')], null);

    await dir.setLink(false, null, null);
    const view = await dir.list();
    assert.equal(view.connected, false);
    assert.equal(view.sessions.length, 0, 'a list nobody can vouch for is not a list');
    assert.deepEqual(
      view.dataPlane,
      { kind: 'ws', host: '127.0.0.1', port: 7718 },
      'the last known data plane survives for the banner UI',
    );
  } finally {
    await host.stop();
  }
});

/**
 * The launcher's answer to "which machines, and how do I reach them".
 *
 * `Shell` asked its *directory* — one machine's session list — so it could
 * only ever name that one machine. The seam is what makes the shell
 * host-plural by construction rather than by a list that happens to hold one,
 * and `localHostSource` has to keep answering exactly as the inlined lookups
 * did or the loopback client changes behaviour for the benefit of the hosted
 * one (#332).
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import type { DirectoryView } from '@zesterm/control';

import type { DirectoryStatus } from '../src/directory-source.ts';
import { localHostSource } from '../src/host-source.ts';

const HOST = 'ab'.repeat(32);
const OTHER = 'cd'.repeat(32);

const VIEW: DirectoryView = {
  connected: true,
  host: { id: HOST, label: 'mac' },
  sessions: [],
  dataPlane: { kind: 'ws', host: '127.0.0.1', port: 7718 },
  lastCreated: null,
};

/** A reader stuck on one status, which is all the seam reads. */
const reading = (status: DirectoryStatus) => () => status;

test('a ready directory offers its own machine, and how to reach it', () => {
  const source = localHostSource(reading({ kind: 'ready', view: VIEW }));
  assert.deepEqual(source.hosts(), [{ id: HOST, label: 'mac' }]);
  assert.notEqual(source.dialFor(HOST), null, 'and it is dialable');
});

test('a directory that is not ready offers nothing at all', () => {
  // No placeholder row, and no host id that resolves to a dial: the directory
  // is still connecting, and a launcher offering a machine nobody has heard
  // from is offering a click that cannot work.
  for (const status of [
    { kind: 'pending' } as const,
    { kind: 'offline' } as const,
    { kind: 'pairing', code: '123456' } as const,
    { kind: 'error', message: 'nope' } as const,
  ]) {
    const source = localHostSource(reading(status));
    assert.deepEqual(source.hosts(), [], `${status.kind} lists nothing`);
    assert.equal(source.dialFor(HOST), null, `${status.kind} dials nothing`);
  }
});

test('a ready directory with no host yet is the same as not ready', () => {
  // `DirectoryView.host` is nullable: the actor exists and the daemon has not
  // said who it is. Reachable on loopback at startup.
  const source = localHostSource(reading({ kind: 'ready', view: { ...VIEW, host: null } }));
  assert.deepEqual(source.hosts(), []);
  assert.equal(source.dialFor(HOST), null);
});

test('an id the directory does not hold gets no dial', () => {
  // **The failure this exists to prevent**, and it is invisible on loopback:
  // a launcher row a frame behind a directory change would dial *this*
  // machine while naming another. There is only one machine here, so the
  // mistake would show up first on the hosted path — as a session opened on
  // the wrong computer.
  const source = localHostSource(reading({ kind: 'ready', view: VIEW }));
  assert.equal(source.dialFor(OTHER), null, 'a stale id names nothing rather than the wrong thing');
  assert.equal(source.dialFor(''), null);
});

test('a machine with no dialable plane is listed and not dialable', () => {
  // Two different questions, and the seam answers them separately: "is this
  // one of my machines" and "can I reach it right now". A relay plane with no
  // relay access is exactly that case — the row exists, the click does not.
  const source = localHostSource(
    reading({
      kind: 'ready',
      view: { ...VIEW, dataPlane: { kind: 'relay', hostId: HOST } },
    }),
    null,
  );
  assert.deepEqual(source.hosts(), [{ id: HOST, label: 'mac' }], 'still your machine');
  assert.equal(source.dialFor(HOST), null, 'but nothing here can reach it');
});

test('the seam re-reads the directory rather than caching it', () => {
  // Built once in setup and read in the render fn — so a directory that
  // becomes ready later must change the answer without the source being
  // rebuilt, or the launcher stays empty for the life of the shell.
  let status: DirectoryStatus = { kind: 'pending' };
  const source = localHostSource(() => status);
  assert.deepEqual(source.hosts(), []);
  status = { kind: 'ready', view: VIEW };
  assert.deepEqual(source.hosts(), [{ id: HOST, label: 'mac' }], 'the next read sees it');
});

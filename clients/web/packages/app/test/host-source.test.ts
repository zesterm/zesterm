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

import type { DirectoryView, SessionEntry } from '@zesterm/control';

import type { DirectoryStatus } from '../src/directory-source.ts';
import { localHostSource } from '../src/host-source.ts';

const HOST = 'ab'.repeat(32);
const OTHER = 'cd'.repeat(32);

const entry = (host: string, session: string): SessionEntry => ({
  host,
  session,
  title: 'zsh',
  cwd: '/src',
  cols: 80,
  rows: 24,
  altScreen: false,
  attached: false,
});

const VIEW: DirectoryView = {
  connected: true,
  host: { id: HOST, label: 'mac' },
  sessions: [],
  dataPlane: { kind: 'ws', host: '127.0.0.1', port: 7718 },
  lastCreated: null,
};

const WITH_SESSIONS: DirectoryView = {
  ...VIEW,
  sessions: [entry(HOST, '1'), entry(HOST, '2')],
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

test('sessions() is every session the shell knows about', () => {
  // The palette searches this, so it is "the fleet's sessions" rather than
  // "a machine's". On loopback that is one machine's; the seam is what lets
  // the hosted path answer with several without `Shell` changing.
  const source = localHostSource(reading({ kind: 'ready', view: WITH_SESSIONS }));
  assert.deepEqual(
    source.sessions().map((e) => e.session),
    ['1', '2'],
  );
});

test('sessions() is empty rather than absent while nothing is ready', () => {
  // The palette maps over this every keystroke; an `undefined` here would be a
  // crash in a search box.
  const source = localHostSource(reading({ kind: 'pending' }));
  assert.deepEqual(source.sessions(), []);
});

test('the empty list is the same list every time, and cannot be mutated', () => {
  // `sessions()` is read by the palette on every keystroke *and* by the route
  // watch. A fresh `[]` per call is an allocation on a hot path and, where a
  // watch compares dependencies by reference, a value never equal to itself —
  // a watch that re-fires forever while a machine is still connecting.
  const source = localHostSource(reading({ kind: 'pending' }));
  assert.equal(source.sessions(), source.sessions(), 'same reference');

  // And shared, so it has to be immutable: one caller pushing into it would
  // hand every other caller a list that is not empty.
  assert.throws(() => {
    (source.sessions() as SessionEntry[]).push(entry(HOST, '1'));
  });
});

test('find() takes both halves of the pair, and the host half is load-bearing', () => {
  // **A session id is unique to its machine, not across the fleet.** Matching
  // on the id alone opens whichever host answered first — invisible on
  // loopback, where there is one host and it always matches, and on the hosted
  // path it is a URL opening a session on the wrong computer. So the check
  // lives here rather than in whichever caller remembers it.
  const source = localHostSource(reading({ kind: 'ready', view: WITH_SESSIONS }));
  assert.equal(source.find(HOST, '1')?.session, '1');
  assert.equal(source.find(OTHER, '1'), null, 'right session id, wrong machine');
  assert.equal(source.find(HOST, '9'), null, 'right machine, no such session');
});

test('find() answers null rather than throwing while nothing is ready', () => {
  const source = localHostSource(reading({ kind: 'pending' }));
  assert.equal(source.find(HOST, '1'), null);
});

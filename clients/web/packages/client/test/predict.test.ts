/**
 * The predictor, owned by the client: a typed key guesses before the host
 * answers, the delta that echoes it takes the guess back, and a guess nothing
 * answers is taken back by the clock — each of which must repaint the row it
 * stood on, or the view shows a guess the engine has already dropped.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { SessionClient, type DirtyRows } from '../src/index.ts';
import { ADDR, FakeClock, FakeDaemon, keyframe, testSigner, update } from './harness.ts';

function guessing(daemon: FakeDaemon, clock: FakeClock, changes: DirtyRows[]) {
  const c = new SessionClient({
    dial: daemon.dial,
    signer: testSigner(),
    label: 'test',
    session: ADDR,
    cols: 20,
    rows: 2,
    clock,
    expectedHost: daemon.host.clientId,
    events: { onChange: (d) => changes.push(d) },
  });
  c.connect();
  return c;
}

const rows = (d: DirtyRows): number[] => (d === 'all' ? [-1] : [...d].sort());

test('a typed key is guessed on its row, and the echo takes the guess back', async () => {
  const daemon = new FakeDaemon();
  const clock = new FakeClock();
  const changes: DirtyRows[] = [];
  const c = guessing(daemon, clock, changes);
  await daemon.completeHandshake();
  const link = daemon.current;
  link.deliver(keyframe(1, ['']));
  changes.length = 0;

  c.input(Uint8Array.of(0x61), { key: 'printable', ch: 'a' });
  assert.deepEqual(
    c.predictor.overlay().map((p) => [p.row, p.col, p.ch]),
    [[0, 0, 'a']],
    'the guess stands at the cursor before the host has said anything',
  );
  assert.deepEqual(rows(changes.at(-1) ?? 'all'), [0], 'the view is told to repaint the row it landed on');
  assert.equal(link.ofType('input').length, 1, 'and the bytes still went out');

  clock.advance(60);
  link.deliver(update(1, 2, 'a'));
  // `update` writes the row but moves no cursor; the host's cursor is what
  // confirms, so this second delta carries it.
  link.deliver({
    t: 'update',
    session: { host: ADDR.host, session: 1 },
    base: 2,
    seq: 3,
    delta: { attrs: [], ops: [{ op: 'cursor', cursor: { row: 0, col: 1, visible: true, shape: 0 } }] },
  });
  assert.deepEqual(c.predictor.overlay(), [], 'the echo landed: nothing is guessed any more');
  assert.deepEqual(rows(changes.at(-1) ?? 'all'), [0], 'a cursor-only confirmation still repaints the row the guess left');
  assert.ok((c.predictor.echoLatencyMs() ?? 0) >= 60, 'a confirmed guess measured the link');
});

test('a guess nothing answers is taken back by the clock, and the row repaints', async () => {
  const daemon = new FakeDaemon();
  const clock = new FakeClock();
  const changes: DirtyRows[] = [];
  const c = guessing(daemon, clock, changes);
  await daemon.completeHandshake();
  daemon.current.deliver(keyframe(1, ['']));

  c.input(Uint8Array.of(0x61), { key: 'printable', ch: 'a' });
  changes.length = 0;
  clock.advance(500);
  assert.equal(c.predictor.pending().length, 1, 'before any measurement a guess may wait a second');
  assert.equal(changes.length, 0, 'nothing changed, nothing repainted');
  clock.advance(600);
  assert.equal(c.predictor.pending().length, 0, 'past a second with no answer the guess is dropped');
  assert.deepEqual(rows(changes.at(-1) ?? 'all'), [0], 'and the row it stood on is repainted to erase it');
});

test('a write that is not typing guesses nothing', async () => {
  const daemon = new FakeDaemon();
  const clock = new FakeClock();
  const c = guessing(daemon, clock, []);
  await daemon.completeHandshake();
  daemon.current.deliver(keyframe(1, ['']));
  c.input(Uint8Array.of(0x1b, 0x5b, 0x49));
  assert.equal(c.predictor.pending().length, 0, 'a focus report is not a keystroke');
});

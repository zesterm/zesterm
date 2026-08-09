/**
 * The four remote.rs behaviours, held at exact instants.
 *
 * Every test drives the real client through the real wire bytes — the fake
 * daemon signs a real challenge — with time advanced by hand, because the ack
 * cadence and the backoff ladder are *numbers*, not vibes.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  SessionClient,
  dirtyRowsOf,
  type ConnectionState,
  REDIAL_MAX_MS,
} from '../src/index.ts';
import { ADDR, FakeClock, FakeDaemon, keyframe, testIdentity, update } from './harness.ts';

function client(daemon: FakeDaemon, clock: FakeClock, states: ConnectionState[] = []) {
  const c = new SessionClient({
    dial: daemon.dial,
    identity: testIdentity(),
    label: 'test',
    session: ADDR,
    cols: 20,
    rows: 2,
    clock,
    expectedHost: daemon.host.clientId,
    events: {
      onConnection: (s) => states.push(s),
    },
  });
  c.connect();
  return c;
}

test('the handshake completes and attach names the client dimensions', () => {
  const daemon = new FakeDaemon();
  const clock = new FakeClock();
  const states: ConnectionState[] = [];
  client(daemon, clock, states);

  daemon.completeHandshake();

  const attach = daemon.current.lastOfType('attach');
  assert.ok(attach, 'a welcomed client attaches without being asked');
  assert.equal(attach['cols'], 20);
  assert.equal(attach['rows'], 2);
  assert.deepEqual(states.at(-1), { phase: 'connected' });
});

test('a keyframe applies and is acknowledged', () => {
  const daemon = new FakeDaemon();
  const clock = new FakeClock();
  const c = client(daemon, clock);
  daemon.completeHandshake();

  daemon.current.deliver(keyframe(5, ['hello world']));
  assert.equal(c.grid.rows[0]?.runs[0]?.text, 'hello world');
  const ack = daemon.current.lastOfType('ack');
  assert.equal(ack?.['seq'], 5, 'the keyframe sequence must be acknowledged');
});

test('acks coalesce on the 16ms cadence and carry the highest seq', () => {
  const daemon = new FakeDaemon();
  const clock = new FakeClock();
  client(daemon, clock);
  daemon.completeHandshake();
  const link = daemon.current;

  link.deliver(keyframe(1));
  assert.equal(link.ofType('ack').length, 1, 'the first ack goes straight out');

  // Three deltas inside one interval: one ack, at the interval's edge, naming
  // the newest sequence — remote.rs's exact rule.
  clock.advance(1);
  link.deliver(update(1, 2, 'a'));
  clock.advance(1);
  link.deliver(update(2, 3, 'b'));
  clock.advance(1);
  link.deliver(update(3, 4, 'c'));
  assert.equal(link.ofType('ack').length, 1, 'acks inside the interval must wait');

  clock.advance(16);
  assert.equal(link.ofType('ack').length, 2, 'the trailing timer flushes the latched ack');
  assert.equal(link.lastOfType('ack')?.['seq'], 4, 'the ack names the highest applied seq');
});

test('an update whose base is not held asks for a keyframe, once', () => {
  const daemon = new FakeDaemon();
  const clock = new FakeClock();
  const c = client(daemon, clock);
  daemon.completeHandshake();
  const link = daemon.current;

  link.deliver(keyframe(1, ['start']));
  // seq 3 building on 2, which this client never applied.
  link.deliver(update(2, 3, 'phantom'));
  assert.equal(c.grid.rows[0]?.runs[0]?.text, 'start', 'a stale update must not be applied');
  assert.equal(link.ofType('request_keyframe').length, 1);

  // More stale updates while the request is in flight: still one request —
  // a burst after a hiccup should heal once, not once per delta.
  link.deliver(update(3, 4, 'phantom2'));
  link.deliver(update(4, 5, 'phantom3'));
  assert.equal(link.ofType('request_keyframe').length, 1, 'one gap, one request');

  // The healing keyframe lands and the chain resumes from it.
  link.deliver(keyframe(6, ['healed']));
  link.deliver(update(6, 7, 'onwards'));
  assert.equal(c.grid.rows[0]?.runs[0]?.text, 'onwards');
});

test('input while disconnected is dropped, never queued', () => {
  const daemon = new FakeDaemon();
  const clock = new FakeClock();
  const c = client(daemon, clock);
  daemon.completeHandshake();
  daemon.current.deliver(keyframe(1));

  daemon.current.close();
  c.input(Uint8Array.of(0x6c, 0x73, 0x0d)); // "ls\n" into the void

  clock.advance(REDIAL_MAX_MS);
  daemon.completeHandshake();
  assert.equal(
    daemon.current.ofType('input').length,
    0,
    'replayed keystrokes are how a reconnect runs a command the user abandoned',
  );
});

test('only the newest resize survives a disconnect, via the re-attach', () => {
  const daemon = new FakeDaemon();
  const clock = new FakeClock();
  const c = client(daemon, clock);
  daemon.completeHandshake();

  daemon.current.close();
  c.resize(100, 30);
  c.resize(120, 40); // newer; the only one that may matter

  clock.advance(REDIAL_MAX_MS);
  daemon.completeHandshake();
  const attach = daemon.current.lastOfType('attach');
  assert.equal(attach?.['cols'], 120, 'the re-attach carries the newest size');
  assert.equal(attach?.['rows'], 40);
  assert.equal(daemon.current.ofType('resize').length, 0, 'no stale resizes are replayed');
});

test('the redial backoff doubles from 200ms to the 5s ceiling', () => {
  const daemon = new FakeDaemon();
  const clock = new FakeClock();
  client(daemon, clock);
  daemon.completeHandshake();

  // A daemon that stays down: every redial is refused on arrival, so the
  // ladder climbs. Five unlucky dials must not add up to minutes of waiting.
  const seen: number[] = [];
  daemon.current.close();
  for (let i = 0; i < 6; i++) {
    const delay = clock.nextTimerIn();
    assert.ok(delay !== undefined, `attempt ${i}: no redial was scheduled`);
    seen.push(delay);
    clock.advance(delay);
    daemon.current.close();
  }
  assert.deepEqual(seen, [200, 400, 800, 1600, 3200, 5000], 'doubling, then the ceiling');

  // The daemon comes back: one good handshake resets the ladder, so the
  // *next* outage starts patient again rather than five-seconds-sullen.
  const delay = clock.nextTimerIn();
  assert.ok(delay !== undefined);
  clock.advance(delay);
  daemon.completeHandshake();
  daemon.current.close();
  assert.equal(clock.nextTimerIn(), 200, 'a successful reconnect resets the backoff');
});

test('reconnect adopts the same session and the grid survives in place', () => {
  const daemon = new FakeDaemon();
  const clock = new FakeClock();
  const c = client(daemon, clock);
  daemon.completeHandshake();
  daemon.current.deliver(keyframe(1, ['before the drop']));
  const gridBefore = c.grid;

  daemon.current.close();
  clock.advance(200);
  daemon.completeHandshake();

  assert.equal(daemon.links.length, 2, 'a fresh dial, not a resurrected socket');
  const attach = daemon.current.lastOfType('attach');
  assert.equal(attach?.['session'] && (attach['session'] as { session: number }).session, 1,
    'the same session is re-attached — adopt, never recreate');
  assert.equal(c.grid, gridBefore, 'the GridView on screen is kept, not rebuilt');

  // The attach keyframe resyncs the same grid, whatever seq it lands at.
  daemon.current.deliver(keyframe(9, ['after the drop']));
  assert.equal(c.grid.rows[0]?.runs[0]?.text, 'after the drop');
  daemon.current.deliver(update(9, 10, 'and onwards'));
  assert.equal(c.grid.rows[0]?.runs[0]?.text, 'and onwards');
});

test('a denied device stops redialling', () => {
  const daemon = new FakeDaemon();
  const clock = new FakeClock();
  const states: ConnectionState[] = [];
  client(daemon, clock, states);

  daemon.current.open();
  daemon.current.deliver({
    t: 'auth_failed',
    reason: 'denied',
    message: 'the person at the machine said no',
  });

  assert.deepEqual(states.at(-1)?.phase, 'failed');
  clock.advance(60_000);
  assert.equal(daemon.links.length, 1, 'a device told no must not hammer the rate limiter');
});

test('an approval prompt surfaces the six-digit code and does not retry over it', () => {
  const daemon = new FakeDaemon();
  const clock = new FakeClock();
  const states: ConnectionState[] = [];
  client(daemon, clock, states);

  daemon.current.open();
  daemon.current.deliver({ t: 'auth_pending', code: '123456', expires_in_secs: 120 });
  assert.deepEqual(states.at(-1), { phase: 'awaiting-approval', code: '123456' });
  clock.advance(10_000);
  assert.equal(daemon.links.length, 1, 'waiting for a person is not a reason to redial');
});

test('dirtyRowsOf is conservative where it cannot be exact', () => {
  assert.deepEqual(
    dirtyRowsOf({ ops: [{ op: 'row', row: 3 }] }, 10),
    new Set([3]),
  );
  assert.deepEqual(
    dirtyRowsOf({ ops: [{ op: 'scroll', top: 1, bottom: 4, lines: 1 }] }, 10),
    new Set([1, 2, 3, 4]),
    'a scroll dirties its whole region — precision buys nothing, staleness paints wrong',
  );
  assert.equal(
    dirtyRowsOf({ ops: [{ op: 'alt_screen', active: true }] }, 10),
    'all',
    'a screen switch invalidates everything on it',
  );
});

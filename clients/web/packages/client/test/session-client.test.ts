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
import {
  ADDR,
  FakeClock,
  FakeDaemon,
  flush,
  gatedSigner,
  keyframe,
  testSigner,
  update,
} from './harness.ts';
import type { ClientSigner } from '@zesterm/auth';

function client(
  daemon: FakeDaemon,
  clock: FakeClock,
  states: ConnectionState[] = [],
  signer: ClientSigner = testSigner(),
) {
  const c = new SessionClient({
    dial: daemon.dial,
    signer,
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

test('the handshake completes and attach names the client dimensions', async () => {
  const daemon = new FakeDaemon();
  const clock = new FakeClock();
  const states: ConnectionState[] = [];
  client(daemon, clock, states);

  await daemon.completeHandshake();

  const attach = daemon.current.lastOfType('attach');
  assert.ok(attach, 'a welcomed client attaches without being asked');
  assert.equal(attach['cols'], 20);
  assert.equal(attach['rows'], 2);
  assert.deepEqual(states.at(-1), { phase: 'connected' });
});

test('a keyframe applies and is acknowledged', async () => {
  const daemon = new FakeDaemon();
  const clock = new FakeClock();
  const c = client(daemon, clock);
  await daemon.completeHandshake();

  daemon.current.deliver(keyframe(5, ['hello world']));
  assert.equal(c.grid.rows[0]?.runs[0]?.text, 'hello world');
  const ack = daemon.current.lastOfType('ack');
  assert.equal(ack?.['seq'], 5, 'the keyframe sequence must be acknowledged');
});

test('acks coalesce on the 16ms cadence and carry the highest seq', async () => {
  const daemon = new FakeDaemon();
  const clock = new FakeClock();
  client(daemon, clock);
  await daemon.completeHandshake();
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

test('an update whose base is not held asks for a keyframe, once', async () => {
  const daemon = new FakeDaemon();
  const clock = new FakeClock();
  const c = client(daemon, clock);
  await daemon.completeHandshake();
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

test('input while disconnected is dropped, never queued', async () => {
  const daemon = new FakeDaemon();
  const clock = new FakeClock();
  const c = client(daemon, clock);
  await daemon.completeHandshake();
  daemon.current.deliver(keyframe(1));

  daemon.current.close();
  c.input(Uint8Array.of(0x6c, 0x73, 0x0d)); // "ls\n" into the void

  clock.advance(REDIAL_MAX_MS);
  await daemon.completeHandshake();
  assert.equal(
    daemon.current.ofType('input').length,
    0,
    'replayed keystrokes are how a reconnect runs a command the user abandoned',
  );
});

test('only the newest resize survives a disconnect, via the re-attach', async () => {
  const daemon = new FakeDaemon();
  const clock = new FakeClock();
  const c = client(daemon, clock);
  await daemon.completeHandshake();

  daemon.current.close();
  c.resize(100, 30);
  c.resize(120, 40); // newer; the only one that may matter

  clock.advance(REDIAL_MAX_MS);
  await daemon.completeHandshake();
  const attach = daemon.current.lastOfType('attach');
  assert.equal(attach?.['cols'], 120, 'the re-attach carries the newest size');
  assert.equal(attach?.['rows'], 40);
  assert.equal(daemon.current.ofType('resize').length, 0, 'no stale resizes are replayed');
});

test('the redial backoff doubles from 200ms to the 5s ceiling', async () => {
  const daemon = new FakeDaemon();
  const clock = new FakeClock();
  client(daemon, clock);
  await daemon.completeHandshake();

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
  await daemon.completeHandshake();
  daemon.current.close();
  assert.equal(clock.nextTimerIn(), 200, 'a successful reconnect resets the backoff');
});

test('reconnect adopts the same session and the grid survives in place', async () => {
  const daemon = new FakeDaemon();
  const clock = new FakeClock();
  const c = client(daemon, clock);
  await daemon.completeHandshake();
  daemon.current.deliver(keyframe(1, ['before the drop']));
  const gridBefore = c.grid;

  daemon.current.close();
  clock.advance(200);
  await daemon.completeHandshake();

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

test('a denied device stops redialling', async () => {
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
  await flush();

  assert.deepEqual(states.at(-1)?.phase, 'failed');
  clock.advance(60_000);
  assert.equal(daemon.links.length, 1, 'a device told no must not hammer the rate limiter');
});

test('an approval prompt surfaces the six-digit code and does not retry over it', async () => {
  const daemon = new FakeDaemon();
  const clock = new FakeClock();
  const states: ConnectionState[] = [];
  client(daemon, clock, states);

  daemon.current.open();
  daemon.current.deliver({ t: 'auth_pending', code: '123456', expires_in_secs: 120 });
  await flush();
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

// ---------------------------------------------------------------------------
// The handshake stopped being synchronous when the device key stopped being
// readable. These three hold the consequences.

test('a host that pipelines two handshake messages is handled one at a time', async () => {
  // The concrete bug this prevents: with the challenge still signing, a
  // `welcome` in the same task moves the state machine, `attach` goes out
  // first, and the daemon sees a client attaching to a session before it ever
  // authenticated. Verified by removing the queue — the order flips and this
  // fails.
  const daemon = new FakeDaemon();
  const clock = new FakeClock();
  const signer = gatedSigner();
  client(daemon, clock, [], signer);

  const link = daemon.current;
  link.open();
  await flush();

  // Both host messages in one task, as one TCP segment would deliver them.
  link.deliver(daemon.challengeFor(link));
  link.deliver(daemon.welcome);
  await flush();

  assert.equal(signer.asked, 1, 'exactly one signature is outstanding');
  assert.deepEqual(
    link.sent.map((m) => m['t']),
    ['hello'],
    'nothing else may go out while the challenge is unanswered',
  );

  signer.release();
  await flush();

  assert.deepEqual(
    link.sent.map((m) => m['t']),
    ['hello', 'auth', 'attach'],
    'auth strictly before attach — a client must not attach before it has proved itself',
  );
});

test('a signature that outlives its connection is never replayed onto the next', async () => {
  // `crypto.subtle` settles on a later task, so a dropped socket can beat it.
  // The stale answer covers the *previous* challenge's nonce; sending it on a
  // fresh connection is a signature the host reads as a device that failed to
  // prove itself, and the redial ladder then punishes a working key.
  const daemon = new FakeDaemon();
  const clock = new FakeClock();
  const signer = gatedSigner();
  client(daemon, clock, [], signer);

  const first = daemon.current;
  first.open();
  await flush();
  first.deliver(daemon.challengeFor(first));
  await flush();
  first.close(); // the daemon goes away mid-signature

  clock.advance(200);
  await flush();
  const second = daemon.current;
  assert.notEqual(second, first, 'the client redialled');
  second.open();
  await flush();

  signer.release(); // the first connection's signature, arriving far too late
  await flush();

  assert.deepEqual(
    second.sent.map((m) => m['t']),
    ['hello'],
    'the second connection has said nothing but hello — its own challenge is still to come',
  );

  // And when that challenge does arrive, it is answered exactly once.
  second.deliver(daemon.challengeFor(second));
  await flush();
  signer.release();
  await flush();
  assert.equal(second.ofType('auth').length, 1, 'one connection, one answer');
});

test('a device that cannot sign says so, and does not blame the host', async () => {
  // A key the browser evicted, or a crypto.subtle that will not sign after
  // all. Reporting this as `host-unproven` sends someone to inspect a daemon
  // that did nothing wrong; and retrying reaches the same broken key.
  const daemon = new FakeDaemon();
  const clock = new FakeClock();
  const signer = gatedSigner();
  const states: ConnectionState[] = [];
  client(daemon, clock, states, signer);

  const link = daemon.current;
  link.open();
  await flush();
  link.deliver(daemon.challengeFor(link));
  await flush();
  signer.fail(new Error('the key is gone'));
  await flush();

  const last = states.at(-1);
  assert.equal(last?.phase, 'failed');
  assert.equal(
    last?.phase === 'failed' ? last.reason : '',
    'signer-failed',
    'this device is the one that failed, not the host',
  );
  assert.equal(link.ofType('auth').length, 0, 'nothing was sent that could not be signed');
  clock.advance(60_000);
  await flush();
  assert.equal(daemon.links.length, 1, 'a key that cannot sign will not sign on the next try');
});

/** A keyframe at a chosen width, with rows starting at a chosen absolute line. */
function keyframeAt(
  seq: number,
  cols: number,
  firstLine: number,
  rows: string[],
): Record<string, unknown> {
  return {
    t: 'keyframe',
    session: { host: ADDR.host, session: 1 },
    seq,
    cols,
    rows: rows.length,
    rows_data: rows.map((text, i) => ({
      line: firstLine + i,
      runs: [{ attr: 0, cells: text.length, text }],
      wrapped: false,
    })),
    attrs: [{ id: 0, fg: 'Default', bg: 'Default', flags: 0 }],
    cursor: { row: 0, col: 0, visible: true, shape: 0 },
    modes: 0,
  };
}

/** A delta that pushes one row into the client's own scrollback. */
function sbPush(base: number, seq: number, line: number, text: string): Record<string, unknown> {
  return {
    t: 'update',
    session: { host: ADDR.host, session: 1 },
    base,
    seq,
    delta: {
      attrs: [],
      ops: [
        {
          op: 'sb_push',
          payload: { line, runs: [{ attr: 0, cells: text.length, text }], wrapped: false },
        },
      ],
    },
  };
}

test('a width change asks for the scrollback it had to discard', async () => {
  // A reflow renumbers every line id, so the rows this client kept cannot be
  // re-anchored and are dropped (`GridView.applyKeyframe`). Dropping them is
  // right; leaving it there is not, because nothing ever fetched them again —
  // so every block above the viewport rendered empty or half-missing for the
  // rest of the attachment, and on Windows ConPTY repaints on every resize.
  //
  // The wire already answers this: `request_scrollback` returns rows under the
  // CURRENT numbering, which is exactly what the client now lacks. (#209)
  const daemon = new FakeDaemon();
  const clock = new FakeClock();
  const c = client(daemon, clock);
  await daemon.completeHandshake();
  const link = daemon.current;

  link.deliver(keyframe(1, ['live row']));
  for (let i = 0; i < 4; i++) link.deliver(sbPush(i + 1, i + 2, 100 + i, `history ${i}`));
  assert.equal(c.grid.scrollback.length, 4, 'four rows of history before the resize');

  // The resize: a keyframe at a new width, whose rows start well above zero
  // because the host holds history of its own below them.
  link.deliver(keyframeAt(9, 40, 70, ['live row, rewrapped']));
  assert.equal(c.grid.scrollback.length, 0, 'the stale rows go, as they must');

  const asked = link.ofType('request_scrollback');
  assert.equal(asked.length, 1, 'and the client asks for them back under the new numbering');
  assert.equal(
    asked[0]?.['from_line'],
    66,
    'the rows immediately above the viewport — asking from 0 would fetch the oldest ' +
      'history and leave a gap under the screen',
  );
  assert.equal(asked[0]?.['count'], 4, 'as much as it had, no more');
});

test('an unchanged width asks for nothing', async () => {
  // A reconnect or a height-only resize keeps every row, so a refetch would be
  // a round trip for rows the client is already holding.
  const daemon = new FakeDaemon();
  const clock = new FakeClock();
  const c = client(daemon, clock);
  await daemon.completeHandshake();
  const link = daemon.current;

  link.deliver(keyframe(1, ['live row']));
  link.deliver(sbPush(1, 2, 100, 'history'));
  link.deliver(keyframe(3, ['live row again']));

  assert.equal(c.grid.scrollback.length, 1, 'the history survives an unchanged width');
  assert.equal(link.ofType('request_scrollback').length, 0, 'so nothing is asked for');
});

test('a width change with no history to lose asks for nothing', async () => {
  const daemon = new FakeDaemon();
  const clock = new FakeClock();
  client(daemon, clock);
  await daemon.completeHandshake();
  const link = daemon.current;

  link.deliver(keyframe(1, ['live row']));
  link.deliver(keyframeAt(2, 40, 0, ['rewrapped']));

  assert.equal(
    link.ofType('request_scrollback').length,
    0,
    'nothing was discarded, so there is nothing to fetch',
  );
});

test('narrowing asks for more rows than it held, because the same text needs more', async () => {
  // Rewrapping at half the width roughly doubles the rows the same history
  // occupies, so a refetch of exactly what was held comes back a fraction of
  // it: 29 rows kept at 80 columns returned 5 of the 47 the oldest block
  // spanned at 40. Both widths are known, so the ask is scaled by their ratio
  // — and rounded up, because asking for too many is free while asking for too
  // few is the bug being fixed. (#209)
  const daemon = new FakeDaemon();
  const clock = new FakeClock();
  client(daemon, clock);
  await daemon.completeHandshake();
  const link = daemon.current;

  link.deliver(keyframe(1, ['live row'])); // 20 columns
  for (let i = 0; i < 10; i++) link.deliver(sbPush(i + 1, i + 2, 100 + i, `history ${i}`));

  link.deliver(keyframeAt(20, 5, 200, ['rewrapped'])); // a quarter of the width

  const asked = link.ofType('request_scrollback');
  assert.equal(asked[0]?.['count'], 40, 'ten rows at 20 columns need about forty at 5');
  assert.equal(asked[0]?.['from_line'], 160, 'and they sit immediately above the viewport');
});

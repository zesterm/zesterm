/**
 * The replay set: an attach ticket admits exactly one attach.
 *
 * Run twice like the other room suites — once against one live object, once
 * against an object thrown away before every handler call — and here that is
 * not ceremony. A spent-id set held in an instance field passes every test
 * written against one live instance and forgets everything the moment the
 * object is evicted — which here is *silent and in the attacker's favour*:
 * after a quiet minute every captured ticket looks fresh again. So the set is
 * asserted three ways, and the first of them is worth nothing on its own:
 *
 * - **A second attach is refused** — which a `Set` in an instance field passes
 *   for the whole of the live run, since nothing there throws the object away.
 * - **A second attach is refused after an explicit `evict()`**, in *both*
 *   modes. Checked by writing the bug: a spent-id `Set` on `RelayRoom` fails
 *   this one in both runs and the one above in neither.
 * - **The key is in storage**, which fails even for a set that is durable by
 *   accident — a module-level one, say, which survives an eviction in a test
 *   process and nothing at all in production.
 *
 * The sweep is here too, because a set that only grows is a leak and an alarm
 * that always re-arms gives away the "an idle host costs nothing" property one
 * wake-up at a time. Both directions are asserted: what a sweep deletes, and
 * what it must not.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { ATTACH_TICKET_TTL_MS, CONTROL_SEEN_REFRESH_MS, hex } from '@zesterm/cloud-shared';

import { CLOSE_HOST_ABSENT } from '../src/room/control.ts';
import { CLOSE_TICKET_REPLAYED } from '../src/room/pipe.ts';
import {
  claimAttachTicket,
  spentTicketKey,
  SPENT_RETENTION_MS,
  sweepSpentTickets,
} from '../src/room/replay.ts';
import { FakeRoomStorage } from './fake-platform.ts';
import { frames, freshJti, NOW, parked, RELAY_SEED, Relay } from './harness.ts';

/** What a spent ticket is closed with, in full: the code *and* which of the two it was. */
const REPLAYED = { code: CLOSE_TICKET_REPLAYED, reason: 'this attach ticket has been spent' };
const UNCLAIMABLE = { code: CLOSE_TICKET_REPLAYED, reason: 'this attach ticket cannot be spent' };

// ---------------------------------------------------------------------------

function suite(mode: string, evicting: boolean): void {
  const it = (name: string, body: () => Promise<void>): void => {
    test(`[${mode}] ${name}`, body);
  };
  const relay = (): Relay => new Relay(evicting, hex(RELAY_SEED));

  it('a ticket admits one attach, and the second is refused', async () => {
    const r = relay();
    const control = await parked(r);
    const jti = freshJti();

    // An ordinary first attach, all the way to a live pipe — `pipe` itself
    // fails if the browser was closed instead.
    await r.pipe(control, { jti });

    const opens = control.sent.length;
    const again = await r.attach('replay', { jti });

    assert.deepEqual(
      again.closed,
      REPLAYED,
      'a captured ticket is good for its whole thirty seconds otherwise, and since the pipe carries real bytes that is a pipe to someone else’s machine',
    );
    assert.equal(
      control.sent.length,
      opens,
      'and the host was never asked: the check is ahead of the `open` frame, the pipe id and the ten-second timer, so a replay costs the machine nothing at all',
    );
  });

  it('a ticket spent on an attach that failed is still spent', async () => {
    const r = relay();
    const jti = freshJti();

    // No control link, so this attach is refused — the ticket is burnt anyway.
    const first = await r.attach('nobody-home', { jti });
    assert.equal(first.closed?.code, CLOSE_HOST_ABSENT);

    const control = await parked(r);
    const second = await r.attach('retry', { jti });
    assert.deepEqual(
      second.closed,
      REPLAYED,
      'spending only on success would leave a captured ticket good for one more try every time the host happened to be asleep, which is exactly when nobody is watching',
    );
    assert.ok(
      !frames(control).some((f) => f['t'] === 'open'),
      'and the host that woke up in between is asked nothing: it gets its challenge and its `ready`, and no pipe to dial',
    );
  });

  it('the refusal survives the object being thrown away', async () => {
    const r = relay();
    const control = await parked(r);
    const jti = freshJti();

    await r.pipe(control, { jti });
    // In *both* modes, which is the point: the live mode is where a set in an
    // instance field gives every right answer until this line.
    r.evict();

    assert.deepEqual(
      (await r.attach('after-eviction', { jti })).closed,
      REPLAYED,
      'the object is evicted between messages, so a set in an instance field makes every captured ticket fresh again after one idle minute — silently, and in the attacker’s favour',
    );
    assert.ok(
      r.platform.storage.keys.includes(spentTicketKey(jti)),
      'and it survives because it is in `ctx.storage`, which is where ADR-009 puts per-object state that must outlive an eviction — never in D1, which the attach path does not have a round trip for',
    );
  });

  it('a refused replay writes nothing and does not extend what it hit', async () => {
    const r = relay();
    await parked(r);
    const jti = freshJti();

    await r.attach('first', { jti });
    const writes = r.platform.storage.writes;
    const armed = r.platform.storage.alarm;

    await r.attach('replay', { jti });
    assert.equal(
      r.platform.storage.writes,
      writes,
      'a refusal is a read and nothing more — re-recording the id would let a flood of replays keep an entry alive for as long as it kept arriving',
    );
    assert.equal(r.platform.storage.alarm, armed, 'and the sweep it is waiting for does not move either');
  });

  it('the first attach arms the sweep for its own expiry, and later ones leave it alone', async () => {
    const r = relay();
    await parked(r);

    await r.attach('first');
    assert.equal(
      r.platform.storage.alarm,
      NOW + SPENT_RETENTION_MS,
      'the sweep is due when the entry this attach wrote stops being able to refuse anything the edge would still admit',
    );

    await r.attach('second', { now: NOW + 5_000 });
    assert.equal(
      r.platform.storage.alarm,
      NOW + SPENT_RETENTION_MS,
      'there is one alarm per object, so re-arming per attach slides the sweep forward for as long as a host keeps receiving them — and a busy room would never sweep at all',
    );
  });

  it('the sweep drops what has expired, keeps what has not, and re-arms for the earliest left', async () => {
    const r = relay();
    await parked(r);
    const early = freshJti();
    const late = freshJti();

    await r.attach('early', { jti: early });
    await r.attach('late', { jti: late, now: NOW + 5_000 });

    const due = await r.sweep(NOW + SPENT_RETENTION_MS);
    assert.equal(due, NOW + SPENT_RETENTION_MS, 'the platform delivers the alarm that was armed');
    assert.deepEqual(
      r.platform.storage.keys.filter((k) => k.startsWith('jti:')),
      [spentTicketKey(late)],
      'a set that only grows is a leak; one that is emptied wholesale hands every ticket in flight a second attach',
    );
    assert.equal(
      r.platform.storage.alarm,
      NOW + 5_000 + SPENT_RETENTION_MS,
      'and the object is woken again for the one that is left, because an alarm the sweep forgot to re-arm is a set that grows for the rest of the object’s life',
    );

    await r.sweep(NOW + 5_000 + SPENT_RETENTION_MS);
    assert.deepEqual(
      r.platform.storage.keys.filter((k) => k.startsWith('jti:')),
      [],
      'the last entry goes the same way',
    );
    assert.equal(
      r.platform.storage.alarm,
      null,
      'and nothing is re-armed: an alarm that always reschedules itself is “an idle host costs nothing” given away one wake-up at a time',
    );
  });

  it('the sweep takes the replay set and leaves the room’s other storage alone', async () => {
    const r = relay();
    // `parked` runs the real enrolment lookup, so the host cache is written by
    // the code that owns it rather than by a key this test made up.
    await parked(r);
    await r.attach('browser');
    assert.ok(r.platform.storage.keys.includes('host:enrolled'), 'the cache under test is really there');

    await r.sweep(NOW + SPENT_RETENTION_MS);
    assert.deepEqual(
      r.platform.storage.keys,
      ['host:enrolled'],
      'the sweep is a prefixed `list`, and one without a prefix would drop the enrolment cache every thirty seconds — a D1 query per reconnect, restored',
    );
  });

  it('the alarm handler is the dispatcher: it sweeps, records presence, and re-arms for whichever is sooner', async () => {
    const r = relay();
    await parked(r);
    const spent = freshJti();
    await r.attach('browser', { jti: spent });

    // `NOW` is years in the past, so what the attach above recorded is expired
    // by the wall clock `RelayRoom.alarm` reads — which is the whole reason it
    // takes no `now` parameter: the platform calls it with an
    // `AlarmInvocationInfo` in that position, and arithmetic on it is `NaN`.
    const future = { exp: Date.now() + 600_000 };
    await r.platform.storage.put(spentTicketKey('still-live'), future);

    const before = Date.now();
    await r.alarm();
    const after = Date.now();

    assert.deepEqual(
      r.platform.storage.keys.filter((k) => k.startsWith('jti:')),
      [spentTicketKey('still-live')],
      'the handler has to reach the sweep, or the set is never swept in production however well the sweep itself is tested',
    );
    // The second job, added by #237. Both run on every wake whoever asked for
    // it, which is what makes an early wake harmless.
    assert.equal(
      r.db.controlRefreshes,
      1,
      'the handler has to reach the presence refresh too — it is the only thing that keeps a parked link readable as online, since its keepalives never wake the object',
    );

    const alarm = r.platform.storage.alarm;
    assert.ok(alarm !== null, 'a parked host keeps the object on a schedule');
    assert.ok(
      alarm >= before + CONTROL_SEEN_REFRESH_MS && alarm <= after + CONTROL_SEEN_REFRESH_MS,
      `the refresh is sooner than the surviving ticket's ${future.exp - before}ms, and the alarm ` +
        `always holds the earliest of the two needs; got ${alarm - before}ms out`,
    );
  });

  it('a ticket the storage cannot record admits nobody', async () => {
    const r = relay();
    const control = await parked(r);
    const opens = control.sent.length;
    r.platform.storage.failStorage = true;

    const ws = await r.attach('blip');
    assert.deepEqual(
      ws.closed,
      UNCLAIMABLE,
      'failing open on a storage error turns an outage into an open replay window; failing closed costs one browser one retry',
    );
    assert.equal(
      control.sent.length,
      opens,
      'and the host is not asked — an uncaught throw here would reject `openAttach` and hand the browser a 500, the one answer it cannot read',
    );
  });

  it('a sweep that cannot be scheduled does not cost the browser its attach', async () => {
    const r = relay();
    const control = await parked(r);
    r.platform.storage.failAlarms = true;

    const p = await r.pipe(control, { client: 'unlucky' });
    assert.equal(
      p.client.closed,
      null,
      'the id was recorded and this attach is honest — refusing it over the housekeeping is `touchHost`’s mistake, a machine taken offline by a blip on something cosmetic',
    );

    r.platform.storage.failAlarms = false;
    await r.attach('later');
    assert.notEqual(
      r.platform.storage.alarm,
      null,
      'and the next attach finds no pending alarm and arms one, so the entry the failed arming left behind is swept after all',
    );
  });
}

suite('one live instance', false);
suite('a fresh instance per call', true);

// --- the set, on its own ---------------------------------------------------

test('a claim is refused for a jti the caller did not supply', async () => {
  const storage = new FakeRoomStorage();
  assert.equal(
    await claimAttachTicket({ storage, jti: '', now: NOW }),
    'unclaimable',
    'only this Worker addresses the object and the id comes out of a decoder that already refuses an empty one — so an empty id is our own bug, and a refactor that stopped passing it must not disable the check with nothing to notice',
  );
  assert.equal(storage.writes, 0, 'and it costs no write, so it cannot be used to fill the set');
});

test('an entry the sweep cannot read is dropped rather than kept for ever', async () => {
  const storage = new FakeRoomStorage();
  await storage.put(spentTicketKey('shape-from-another-build'), 'not an entry');
  await storage.put(spentTicketKey('no-expiry'), { seenAt: NOW });

  await sweepSpentTickets(storage, NOW);
  assert.deepEqual(
    storage.keys,
    [],
    'an entry with no readable expiry is one no later sweep could expire either, so keeping it is a leak with no end — and dropping it costs at most one replay window for a ticket the edge is already bounding by `exp`',
  );
  assert.equal(storage.alarm, null, 'and there is nothing left to wake up for');
});

test('a spent id is remembered for longer than its ticket can live', async () => {
  const storage = new FakeRoomStorage();
  await claimAttachTicket({ storage, jti: 'once', now: NOW });

  // The tightest a ticket can be: minted at `NOW` and alive for the full TTL,
  // which is the most the edge will verify (`exp - iat > TTL` is refused).
  await sweepSpentTickets(storage, NOW + ATTACH_TICKET_TTL_MS);
  assert.deepEqual(
    storage.keys,
    [spentTicketKey('once')],
    'the edge and the object read two different machines’ clocks, so an entry that expired exactly with its ticket would leave the skew between them as a replay window',
  );
  assert.ok(
    SPENT_RETENTION_MS > ATTACH_TICKET_TTL_MS,
    'which is only true while the retention is the longer of the two',
  );
});

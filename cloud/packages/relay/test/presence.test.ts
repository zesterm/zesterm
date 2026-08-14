/**
 * Whether a parked control link is still there, and how the room could
 * possibly know (#237).
 *
 * The bug this suite exists for is a machine that answers an attach instantly
 * while the fleet screen calls it asleep. The fix rests on one awkward fact,
 * and that is what most of these tests are about: **the room cannot see the
 * keepalives**. The daemon pings every thirty seconds, but `room.ts` registers
 * those as a `WebSocketRequestResponsePair`, so workerd answers them beneath
 * the object and `webSocketMessage` is never called. A refresh driven by the
 * link's own traffic — the obvious design, and the one the issue proposed —
 * cannot be built at all.
 *
 * So the evidence is `getWebSocketAutoResponseTimestamp`, which the platform
 * records without waking anything, read on an alarm that exists only while a
 * link is parked. Every test below pins one half of that: what is written,
 * what stops being written, and — the property that keeps ADR-009's cost model
 * honest — when the object stops waking altogether.
 *
 * Run twice like the other room suites. Presence lives in D1 and in the
 * attachment rather than in an instance field, and the evicting run is what
 * says so.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { CONTROL_SEEN_REFRESH_MS, controlLinkIsLive, hex } from '@zesterm/cloud-shared';

import { KEEPALIVE_STALE_MS } from '../src/room/presence.ts';
import { HOST, NOW, parked, RELAY_SEED, Relay } from './harness.ts';

function suite(mode: string, evicting: boolean): void {
  const it = (name: string, body: () => Promise<void>): void => {
    test(`[${mode}] ${name}`, body);
  };
  const relay = (): Relay => new Relay(evicting, hex(RELAY_SEED));

  it('parking records the machine as reachable right now', async () => {
    const r = relay();
    await parked(r);

    assert.equal(
      r.db.controlSeen.get(HOST),
      NOW,
      'the whole bug in one line: a machine whose link is parked must be readable as online, and until #237 the only column written here was `last_seen_at`, which never expires and so cannot answer it',
    );
    assert.equal(
      r.db.lastSeen.get(HOST),
      NOW,
      'and arrival is still recorded, in the same statement — two facts, two columns',
    );
  });

  it('the refresh keeps a parked link online without ever seeing a keepalive', async () => {
    const r = relay();
    const control = await parked(r);

    // What the platform recorded on its own: the daemon pinged, workerd
    // answered, and the object was never woken for it. This map IS the
    // mechanism — there is no message the room could have observed instead.
    const later = NOW + CONTROL_SEEN_REFRESH_MS;
    r.platform.autoResponseAt.set(control, new Date(later - 1_000));

    await r.room().refreshPresence(later);

    assert.equal(
      r.db.controlSeen.get(HOST),
      later,
      'the alarm is the only thing that can keep this column moving, because the traffic that proves the link is alive is answered beneath the object',
    );
    assert.equal(
      r.db.lastSeen.get(HOST),
      NOW,
      'and it must not drag `last_seen_at` along: that column means "when did this machine last dial in", and a refresh every five minutes would turn it into "now" for ever',
    );
    assert.ok(
      controlLinkIsLive(r.db.controlSeen.get(HOST) ?? null, later),
      'which is what /api/hosts will read back as online',
    );
  });

  it('a link that stopped answering keepalives is not online, though its socket is still held', async () => {
    // The case a socket count cannot see, and the reason the auto-response
    // timestamp is consulted at all: the peer vanished without a close, so the
    // platform is still holding a socket for a machine that is gone.
    const r = relay();
    const control = await parked(r);

    const later = NOW + CONTROL_SEEN_REFRESH_MS;
    r.platform.autoResponseAt.set(control, new Date(later - KEEPALIVE_STALE_MS - 1));

    await r.room().refreshPresence(later);

    assert.equal(
      r.db.controlSeen.get(HOST),
      null,
      'four missed keepalives is a daemon that is not there, and saying otherwise is exactly the lie #237 is about — pointing the other way',
    );
    assert.equal(
      r.db.controlClears,
      1,
      'and it is cleared rather than left to the bound, so the fleet screen is right in seconds',
    );
  });

  it('a link that went quiet can come back without waiting to be re-parked', async () => {
    // The recovery path, and the reason the two "not online" cases re-arm
    // differently. A daemon whose pings were merely late — a stalled network,
    // a laptop resuming — must be able to return to online on its own. Giving
    // up here would latch a machine into "asleep" until its next attach, which
    // is the failure this whole change is about.
    const r = relay();
    const control = await parked(r);

    const quiet = NOW + CONTROL_SEEN_REFRESH_MS;
    r.platform.autoResponseAt.set(control, new Date(quiet - KEEPALIVE_STALE_MS - 1));
    r.platform.storage.alarm = null;
    await r.room().refreshPresence(quiet);

    assert.equal(r.db.controlSeen.get(HOST), null, 'gone quiet, so not online');
    assert.equal(
      r.platform.storage.alarm,
      quiet + CONTROL_SEEN_REFRESH_MS,
      'but the socket is still held, so the object keeps looking — unlike the case where the link has actually gone',
    );

    // The pings resume.
    const back = quiet + CONTROL_SEEN_REFRESH_MS;
    r.platform.autoResponseAt.set(control, new Date(back - 1_000));
    await r.room().refreshPresence(back);

    assert.equal(
      r.db.controlSeen.get(HOST),
      back,
      'and it is online again on the next refresh, with nothing having had to re-park it',
    );
  });

  it('a link that has never been pinged yet still counts as parked', async () => {
    // `getWebSocketAutoResponseTimestamp` is `null` until the platform has
    // answered one, which is the ordinary state of a link that parked seconds
    // ago. Treating that as death would take every freshly-parked machine
    // offline for a whole refresh interval.
    const r = relay();
    await parked(r);
    assert.equal(r.platform.autoResponseAt.size, 0, 'nothing has been recorded for this socket');

    await r.room().refreshPresence(NOW + 1_000);

    assert.equal(
      r.db.controlSeen.get(HOST),
      NOW + 1_000,
      'the socket being held is the same evidence `openAttach` already dials on, so falling back to it is no weaker than what shipped',
    );
  });

  it('an attach repairs a link that parked before this code existed', async () => {
    // #241. `control_seen_at` and the refresh alarm are both written when a
    // link *parks*, so a connection older than the deploy has neither and
    // reads asleep for ever — nothing retroactively observes it. The room
    // wakes on attach, and a ready control link at that moment is live proof,
    // so that is where it gets fixed.
    const r = relay();
    const control = await parked(r);

    // Exactly the state a pre-deploy park leaves behind: the link is up, the
    // column is empty, and nothing is scheduled to fill it.
    r.db.controlSeen.set(HOST, null);
    r.platform.storage.alarm = null;
    r.db.controlRefreshes = 0;

    const later = NOW + 60_000;
    r.platform.autoResponseAt.set(control, new Date(later - 1_000));
    await r.attach('browser', { now: later });

    assert.equal(
      r.db.controlSeen.get(HOST),
      later,
      'the first attach is enough: the card stops saying asleep about a machine that just served one',
    );
    assert.ok(
      r.platform.storage.alarm !== null,
      'and the refresh is scheduled from here on, so it stays online without needing another attach',
    );
  });

  it('an attach on a healthy room writes nothing extra', async () => {
    // The bound. Parking already armed the refresh, so presence is being
    // maintained and an attach has nothing to add — the heal must not become
    // a D1 write per attach for every room in the fleet.
    const r = relay();
    await parked(r);
    assert.ok(r.platform.storage.alarm !== null, 'parking schedules the refresh');
    const refreshes = r.db.controlRefreshes;

    await r.attach('browser', { now: NOW + 1_000 });

    assert.equal(
      r.db.controlRefreshes,
      refreshes,
      'steady state pays nothing: the alarm is already keeping this column fresh',
    );
  });

  it('a closed link is cleared at once, rather than waiting out the bound', async () => {
    const r = relay();
    const control = await parked(r);
    assert.equal(r.db.controlSeen.get(HOST), NOW);

    r.room().webSocketClose(control);
    // The clear is deliberately not awaited inside the handler — the
    // platform's is synchronous — so let the microtask it started settle.
    await Promise.resolve();
    await Promise.resolve();

    assert.equal(
      r.db.controlSeen.get(HOST),
      null,
      'a closed lid should show up on the fleet screen in seconds; the bound exists for the room that dies without ever reaching this handler',
    );
    assert.ok(
      !controlLinkIsLive(r.db.controlSeen.get(HOST) ?? null, NOW),
      'and `null` reads as offline, which is what a machine with no link is',
    );
  });

  it('the object stops waking once nothing is parked, which is what keeps an idle account free', async () => {
    const r = relay();
    const control = await parked(r);
    assert.equal(
      r.platform.storage.alarm,
      NOW + CONTROL_SEEN_REFRESH_MS,
      'a parked host puts the object on a schedule, because its keepalives cannot',
    );

    r.room().webSocketClose(control);
    await Promise.resolve();
    r.platform.storage.alarm = null;

    // The alarm the platform would have delivered, with the link now gone.
    await r.room().refreshPresence(NOW + CONTROL_SEEN_REFRESH_MS);

    assert.equal(
      r.platform.storage.alarm,
      null,
      'ADR-009 says an idle host costs nothing, and this is what keeps that true: the refresh re-arms only while there is a link to report, so a room whose daemon went away goes quiet for good rather than waking every five minutes for ever',
    );
  });

  it('a D1 blip on the refresh leaves the link parked and lets the bound do the work', async () => {
    // The `touchHost` argument, one column over: this runs on an alarm with no
    // peer waiting, so a failure must not throw — and the column decaying to
    // offline on its own is the safe direction to be wrong in.
    const r = relay();
    const control = await parked(r);
    r.db.failUpdates = true;

    const later = NOW + CONTROL_SEEN_REFRESH_MS;
    r.platform.autoResponseAt.set(control, new Date(later - 1_000));
    await r.room().refreshPresence(later);

    assert.equal(
      r.db.controlSeen.get(HOST),
      NOW,
      'the write failed and nothing was rewritten, so the column simply ages towards the bound',
    );
    assert.equal(
      r.platform.storage.alarm,
      NOW + CONTROL_SEEN_REFRESH_MS,
      'and the schedule survives, so the next refresh can succeed',
    );
  });
}

suite('one live instance', false);
suite('a fresh instance per call', true);

// --- the bound itself ------------------------------------------------------

test('the bound decays a room that died without a close handler', () => {
  // The failure the whole design is bounded against: a room evicted or killed
  // outright never runs `webSocketClose`, so the column is left saying a
  // machine is reachable. Nothing can clear it — so it has to expire.
  const parkedAt = NOW;
  assert.ok(controlLinkIsLive(parkedAt, NOW + CONTROL_SEEN_REFRESH_MS), 'one missed refresh is not death');
  assert.ok(
    controlLinkIsLive(parkedAt, NOW + 2 * CONTROL_SEEN_REFRESH_MS),
    'nor two: an alarm is retried rather than punctual, and a bound equal to the cadence would flap a machine that is sitting there perfectly reachable',
  );
  assert.ok(
    !controlLinkIsLive(parkedAt, NOW + 3 * CONTROL_SEEN_REFRESH_MS),
    'three intervals with no refresh is a room that is not coming back, and the screen must stop claiming otherwise',
  );
});

test('never connected and a clock that jumped are both offline', () => {
  assert.equal(controlLinkIsLive(null, NOW), false, 'a machine that has never dialled a relay');
  assert.equal(
    controlLinkIsLive(NOW + 60_000, NOW),
    false,
    'a timestamp from the future is not evidence — treating it as live would pin a dead machine online until the clock caught up',
  );
});

/**
 * The control link, run twice: once against one live object, once against an
 * object that is thrown away between every single call.
 *
 * The second run is the whole point. ADR-009's rule is that nothing may live in
 * instance fields, because the runtime evicts the object between messages — and
 * the bug it prevents passes every test written against a single instance. The
 * hazard here is not the pairing table people expect; it is the **challenge
 * nonce**, which is generated in one call and answered in the next with a
 * network round trip in between. A nonce in a field authenticates the daemon
 * that answers instantly and refuses every one that does not, which is a bug
 * that looks like flaky wifi.
 *
 * Both halves were checked by writing the bugs. Keeping the nonce in a field
 * fails nine of the sixteen tests below in the evicting run and none in the
 * live one. Answering "is a host present" from a field set at `hello` time is
 * the opposite shape: it gives the right answer in the live run, where only the
 * `getWebSocketsCalls` assertion catches it. Neither guard subsumes the other,
 * which is why both are here.
 *
 * `storage.writes` not moving is the third: ADR-009's cost model made
 * assertable rather than aspirational.
 *
 * Everything here drives `RelayRoom`'s methods directly. `fetch` is not tested
 * and cannot be — `WebSocketPair` and `WebSocketRequestResponsePair` are
 * workerd globals — which is exactly why the class is shaped so that `fetch`
 * holds nothing but their construction.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { fromHex } from '@zesterm/cloud-shared';

import {
  CLOSE_CONTROL_PROTOCOL,
  CLOSE_CONTROL_REFUSED,
  CLOSE_CONTROL_REPLACED,
  CLOSE_CONTROL_UNCONFIGURED,
  CLOSE_HOST_ABSENT,
  CONTROL_HANDSHAKE_TTL_MS,
  KEEPALIVE_REQUEST,
  KEEPALIVE_RESPONSE,
  MAX_CONTROL_MESSAGE_CHARS,
  MAX_LABEL_CHARS,
  NONCE_BYTES,
  readControlAttachment,
} from '../src/room/control.ts';
import { HOST_LOOKUP_TTL_MS } from '../src/room/hosts.ts';
import { CLOSE_PIPE_DIAL_TIMEOUT } from '../src/room/pipe.ts';
import { FakeSock } from './fake-platform.ts';
import {
  frames,
  hello,
  HOST,
  lastFrame,
  nonceOf,
  NOW,
  parked,
  RELAY_KEY,
  RELAY_SEED,
  Relay,
  STRANGER,
  STRANGER_SEED,
} from './harness.ts';
import { hex } from '@zesterm/cloud-shared';

// ---------------------------------------------------------------------------

function suite(mode: string, evicting: boolean): void {
  const it = (name: string, body: () => Promise<void>): void => {
    test(`[${mode}] ${name}`, body);
  };
  const relay = (): Relay => new Relay(evicting, hex(RELAY_SEED));
  /** A deployment whose `RELAY_SIGNING_KEY` secret has never been set. */
  const unconfigured = (): Relay => new Relay(evicting, undefined);

  it('a daemon is challenged, proves it holds its key, and its link is parked', async () => {
    const r = relay();
    const ws = await r.dial();

    const challenge = frames(ws)[0]!;
    assert.equal(challenge['t'], 'challenge');
    assert.equal(challenge['v'], 1);
    assert.ok(
      fromHex(String(challenge['nonce']), NONCE_BYTES),
      'a challenge that is not 32 bytes of hex is not a challenge the Rust can sign',
    );
    assert.equal(
      challenge['relay_key'],
      RELAY_KEY,
      'this is the key a daemon is meant to pin across reconnects, so it must be the public half of the configured seed and not something minted per dial',
    );

    await r.say(ws, await hello(nonceOf(ws)));
    assert.deepEqual(lastFrame(ws), { t: 'ready', v: 1 });
    assert.deepEqual(
      readControlAttachment(ws),
      { s: 'ready', host: HOST, label: 'andy-mac', at: NOW },
      'the parked state is in the attachment and nowhere else, because nothing else survives eviction',
    );
    assert.equal(r.db.lastSeen.get(HOST), NOW, 'last_seen_at is written on connect');
  });

  it('a D1 blip on last_seen_at does not take an enrolled host offline', async () => {
    const r = relay();
    // `last_seen_at` is cosmetic -- the fleet screen's "last seen". Its write
    // was awaited unguarded, so a transient D1 error rejected the handler and
    // an authenticated, enrolled daemon got no `ready`, no error frame and no
    // close code: taken offline by a blip on something nobody makes a decision
    // from. `touchHost`'s own doc comment forbade exactly that while the code
    // did it.
    r.db.failUpdates = true;
    const ws = await r.dial();
    await r.say(ws, await hello(nonceOf(ws)));

    assert.deepEqual(
      lastFrame(ws),
      { t: 'ready', v: 1 },
      'the host proved itself and is enrolled, so it parks -- whatever the timestamp column did',
    );
    assert.equal(
      r.db.lastSeen.get(HOST),
      undefined,
      'and the write really did fail, so this is about the failure rather than a happy path',
    );
  });

  it('the challenge is answered from the attachment, not from anything in memory', async () => {
    const r = relay();
    const ws = await r.dial();
    const nonce = nonceOf(ws);

    // The object thrown away between the challenge and the answer, in both
    // modes — which is what a network round trip is to a Durable Object that
    // has nothing else to do. A nonce in an instance field is gone by now; the
    // attachment is not.
    r.evict();

    await r.say(ws, await hello(nonce));
    assert.equal(
      lastFrame(ws)['t'],
      'ready',
      'the gap between challenge and hello is a network round trip, so a nonce that does not survive it refuses every real daemon after the first idle moment',
    );
  });

  it('every dial gets its own nonce', async () => {
    const r = relay();
    const first = await r.dial('first');
    const second = await r.dial('second');
    assert.notEqual(
      nonceOf(first),
      nonceOf(second),
      'a challenge that repeats is not a challenge: an answer to the last one answers this one',
    );
  });

  it('an attach is told which of the two things is wrong', async () => {
    const r = relay();

    const before = r.platform.getWebSocketsCalls;
    const alone = await r.attach('early');
    assert.deepEqual(alone.closed, {
      code: CLOSE_HOST_ABSENT,
      reason: 'no control link for this host',
    });
    assert.ok(
      r.platform.getWebSocketsCalls > before,
      'the answer must come from the platform every time — a room that remembered whether a host was present is the hibernation bug',
    );

    await parked(r);

    const after = await r.attach('late');
    assert.deepEqual(
      after.closed,
      {
        code: CLOSE_PIPE_DIAL_TIMEOUT,
        reason: 'the host did not dial back',
      },
      '"your Mac is asleep" and "your Mac is here and did not answer" are different problems and only one of them is the user’s. This host is parked but nothing plays its half, so the dial-back deadline is what answers',
    );
  });

  it('a control link that has not authenticated is not a host being present', async () => {
    const r = relay();
    await r.dial();
    assert.equal(
      (await r.attach()).closed?.code,
      CLOSE_HOST_ABSENT,
      'anyone can open a socket; only a signature over the nonce makes it a host',
    );
  });

  it('a challenge goes stale, and says so rather than blaming the key', async () => {
    const r = relay();
    const ws = await r.dial();
    const answer = await hello(nonceOf(ws));

    await r.say(ws, answer, NOW + CONTROL_HANDSHAKE_TTL_MS + 1);
    assert.deepEqual(lastFrame(ws), { t: 'error', v: 1, code: 'stale' });
    assert.equal(
      ws.closed?.code,
      CLOSE_CONTROL_PROTOCOL,
      'a slow link is not a refusal — telling a daemon its key is bad sends it to re-enrol when it should simply redial',
    );

    const room = relay();
    const inTime = await room.dial('in-time');
    await room.say(inTime, await hello(nonceOf(inTime)), NOW + CONTROL_HANDSHAKE_TTL_MS);
    assert.equal(
      lastFrame(inTime)['t'],
      'ready',
      'the last instant inside the window is inside it',
    );
  });

  it('a signature by another key, or over another nonce, is refused', async () => {
    const r = relay();

    const impostor = await r.dial('impostor');
    await r.say(impostor, await hello(nonceOf(impostor), { seed: STRANGER_SEED }));
    assert.deepEqual(lastFrame(impostor), { t: 'error', v: 1, code: 'bad-signature' });
    assert.equal(impostor.closed?.code, CLOSE_CONTROL_REFUSED);

    // The replay: a perfectly good answer to a *previous* challenge.
    const first = await r.dial('first');
    const stolen = await hello(nonceOf(first));
    const second = await r.dial('second');
    await r.say(second, stolen);
    assert.deepEqual(
      lastFrame(second),
      { t: 'error', v: 1, code: 'bad-signature' },
      'a captured hello must not park a link on the next dial, or the nonce buys nothing',
    );
  });

  it('a hello for another machine is a wrong room, not a bad key', async () => {
    const r = relay();
    const ws = await r.dial();
    // Genuinely signed, by the key it names — the object is simply not that
    // machine's. Reporting `bad-signature` here sends whoever is debugging it
    // into the crypto.
    await r.say(ws, await hello(nonceOf(ws), { seed: STRANGER_SEED, host: STRANGER }));
    assert.deepEqual(lastFrame(ws), { t: 'error', v: 1, code: 'wrong-room' });
    assert.equal(ws.closed?.code, CLOSE_CONTROL_REFUSED);
  });

  it('a key nobody enrolled proves possession and gets nothing', async () => {
    const r = relay();
    r.db.enrolled.delete(HOST);

    const ws = await r.dial();
    await r.say(ws, await hello(nonceOf(ws)));
    assert.deepEqual(
      lastFrame(ws),
      { t: 'error', v: 1, code: 'unknown-host' },
      'possession of a key says which machine this is, not that the machine is still ours',
    );
    assert.equal(r.db.updates, 0, 'a refused link must not touch the row it was refused over');
    assert.equal(r.platform.storage.writes, 0, 'and must not seed the positive cache');
  });

  it('the enrolment lookup is cached and the timestamp is not', async () => {
    const r = relay();

    await parked(r, 'first');
    assert.equal(r.db.selects, 1);
    assert.equal(r.db.updates, 1);
    assert.equal(r.platform.storage.writes, 1, 'one write, seeding the cache');

    // Inside the minute. The dial and its answer share a clock, or the
    // challenge would go stale long before the cache does.
    const withinMinute = NOW + HOST_LOOKUP_TTL_MS - 1;
    const again = await r.dial('second', { now: withinMinute });
    await r.say(again, await hello(nonceOf(again)), withinMinute);
    assert.equal(lastFrame(again)['t'], 'ready');
    assert.equal(r.db.selects, 1, 'the lookup is cached for a minute in the object’s own storage');
    assert.equal(r.db.updates, 2, 'last_seen_at is not cached — it is the thing being recorded');
    assert.equal(r.platform.storage.writes, 1, 'a cache hit writes nothing');

    const afterMinute = NOW + HOST_LOOKUP_TTL_MS;
    const later = await r.dial('third', { now: afterMinute });
    await r.say(later, await hello(nonceOf(later)), afterMinute);
    assert.equal(
      r.db.selects,
      2,
      'the cache is in the object’s storage and outlives every eviction, so without an expiry a revoked machine would go on parking links until someone deleted the room',
    );
  });

  it('a newer control link replaces the one before it', async () => {
    const r = relay();
    const first = await parked(r, 'first');
    const second = await parked(r, 'second');

    assert.deepEqual(
      first.closed,
      { code: CLOSE_CONTROL_REPLACED, reason: 'replaced by a newer control link' },
      'a daemon that changed networks leaves a socket behind, and two parked links make "dial back through which" unanswerable',
    );
    assert.equal(
      readControlAttachment(first),
      null,
      'the replaced link stops counting as ready without waiting on a close handshake with a peer that may be gone',
    );
    assert.equal(second.closed, null);
    assert.equal(
      (await r.attach()).closed?.code,
      CLOSE_PIPE_DIAL_TIMEOUT,
      'exactly one host is present afterwards',
    );
  });

  it('a parked link says nothing more, and is hung up on if it does', async () => {
    const r = relay();
    const ws = await parked(r);
    await r.say(ws, JSON.stringify({ t: 'open', v: 1 }));
    assert.deepEqual(lastFrame(ws), { t: 'error', v: 1, code: 'malformed' });
    assert.equal(ws.closed?.code, CLOSE_CONTROL_PROTOCOL);
  });

  it('every malformed hello is a refusal, never a throw', async () => {
    const r = relay();
    const cases: Array<[string | ArrayBuffer, string, string]> = [
      ['not json at all', 'malformed', 'a daemon mid-upgrade, or a probe'],
      ['[]', 'malformed', 'JSON that is not an object'],
      [JSON.stringify({ t: 'goodbye', v: 1 }), 'malformed', 'a message this build has no name for'],
      [
        JSON.stringify({ t: 'hello', v: 1, host: HOST, label: 'x' }),
        'malformed',
        'no signature at all',
      ],
      [
        JSON.stringify({ t: 'hello', v: 1, host: HOST.toUpperCase(), label: 'x', sig: 'ab'.repeat(64) }),
        'malformed',
        'an uppercase id is a different primary key, not the same machine',
      ],
      [
        JSON.stringify({ t: 'hello', v: 1, host: HOST, label: 'x', sig: 'ab'.repeat(63) }),
        'malformed',
        'a short signature is an error, never a pad',
      ],
      [
        JSON.stringify({ t: 'hello', v: 1, host: HOST, label: 'y'.repeat(MAX_LABEL_CHARS + 1), sig: 'ab'.repeat(64) }),
        'malformed',
        'an unbounded label is an attachment that throws once someone adds a field beside it',
      ],
      ['x'.repeat(MAX_CONTROL_MESSAGE_CHARS + 1), 'malformed', 'refused unread, before JSON.parse'],
      [new ArrayBuffer(8), 'malformed', 'the control link is text, because the free keepalive is'],
      [JSON.stringify({ t: 'hello', v: 2 }), 'version', 'a daemon that speaks a later protocol'],
    ];

    for (const [message, code, why] of cases) {
      const ws = await r.dial('malformed');
      await r.say(ws, message);
      assert.deepEqual(lastFrame(ws), { t: 'error', v: 1, code }, why);
    }
  });

  it('a relay with no key of its own refuses every control link', async () => {
    const r = unconfigured();
    const ws = await r.dial();
    assert.deepEqual(
      lastFrame(ws),
      { t: 'error', v: 1, code: 'unconfigured' },
      'there is nothing for a daemon to pin, and the honest answer is to say so rather than to publish a key that is not backed by a secret',
    );
    assert.equal(ws.closed?.code, CLOSE_CONTROL_UNCONFIGURED);
    assert.equal(
      ws.attachmentWrites,
      0,
      'nothing was challenged, so nothing is remembered about it',
    );
  });

  it('the keepalive is the free one, and it is set even for a refused dial', async () => {
    const r = unconfigured();
    await r.dial();
    assert.deepEqual(
      r.platform.autoResponse,
      { request: KEEPALIVE_REQUEST, response: KEEPALIVE_RESPONSE },
      'setWebSocketAutoResponse answers without waking the object, which is the difference between an idle host costing nothing and costing a request every thirty seconds forever',
    );
  });

  it('an attach costs no storage write and no query', async () => {
    const r = relay();
    await parked(r);
    const writes = r.platform.storage.writes;
    const selects = r.db.selects;

    await r.attach();
    await r.attach();

    assert.equal(r.platform.storage.writes, writes, 'ADR-009: never write storage on the data path');
    assert.equal(r.db.selects, selects, 'and the attach path makes no query of its own');
  });
}

suite('one live instance', false);
suite('a fresh instance per call', true);

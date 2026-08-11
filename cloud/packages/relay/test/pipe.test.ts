/**
 * The pipe: a browser attaches, the host dials back, and after that the object
 * is a byte pump and nothing else. Run twice, like `control.test.ts` — once
 * against one live object, once against an object thrown away before every
 * handler call.
 *
 * The second run is what this file is for. ADR-009 names the hibernation bug by
 * name and it is a *pipe* bug: a `Map<WebSocket, WebSocket>` pairing two
 * sockets passes every test written against a single live instance and drops
 * sessions in production after the first idle gap. So the pairing is asserted
 * twice over, in two different ways, because neither catches the other's bug:
 *
 * - **By value**, in the evicting run and again with an explicit `evict()` in
 *   the live one: bytes still cross after the object is gone.
 * - **By structure**, with `getWebSocketsCalls`: exactly one lookup per
 *   forwarded message. A peer memoised *within* one instance answers correctly
 *   for the whole of the live run, and this counter is the only thing that
 *   fails it there.
 *
 * `storage.writes` and `attachmentWrites` not moving under a few hundred
 * messages is the third, and it is ADR-009's cost model rather than tidiness:
 * a write on the data path costs a request per message and keeps the object
 * awake, which turns the dominant cost term from zero into continuous.
 *
 * Every one of these was checked by breaking the thing it covers — the mutation
 * list is in the PR, and each assertion below that died to one says so.
 *
 * **One thing here is deliberately not asserted, and the gap is real.** The
 * browser leg is tagged *before* the `open` goes out, because the daemon's dial
 * can land before `openAttach`'s next line runs and a host leg that arrived
 * first would find a pipe with one end. Swapping those two lines passes every
 * test below: it is a race between two `fetch`es, and this harness is one thread
 * with nothing to interleave. `room.ts`'s comment is what holds that line.
 *
 * As in `control.test.ts`, `fetch` is not driven: `WebSocketPair` is a workerd
 * global. The two refusal shapes it routes are therefore asserted at the seam
 * it routes on — a close code on the browser's socket, and `openPipeLeg`
 * returning `false`, which `fetch` turns into the daemon's 404.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { hex } from '@zesterm/cloud-shared';

import { CLOSE_HOST_ABSENT } from '../src/room/control.ts';
import {
  MAX_FRAME_BYTES,
  MAX_MESSAGE_BYTES,
  PIPE_BYTE_BURST_BYTES,
  PIPE_BYTE_RATE_PER_SECOND,
  PIPE_MESSAGE_BURST,
  SEAL_TAG_BYTES,
} from '../src/room/limits.ts';
import {
  CLOSE_PIPE_DIAL_TIMEOUT,
  CLOSE_PIPE_FLOOD,
  CLOSE_PIPE_PEER_GONE,
  messageSize,
  newPipeId,
  PIPE_ID_CHARS,
  pipeIdIsWellFormed,
  pipeMembership,
  pipeTag,
  PipeBudget,
  PipeBudgets,
  ROLE_CLIENT,
  ROLE_HOST,
} from '../src/room/pipe.ts';
import { FakePlatform, FakeSock } from './fake-platform.ts';
import { NOW, parked, pipeOf, RELAY_SEED, Relay } from './harness.ts';

/** A sealed `MAX_FRAME`: the largest message this protocol can actually produce. */
const SEALED_MAX_FRAME = MAX_FRAME_BYTES + SEAL_TAG_BYTES;

/**
 * One macrotask turn.
 *
 * Long enough that a promise which is *not* genuinely awaiting something has
 * settled, which is the whole of the "the 101 waits for the dial-back" test.
 */
function turn(): Promise<void> {
  return new Promise((resolve) => {
    setTimeout(resolve, 0);
  });
}

/** A payload with a distinct byte at every position, so a copy is provably one. */
function payload(size: number): Uint8Array {
  const bytes = new Uint8Array(size);
  for (let i = 0; i < size; i += 1) bytes[i] = (i * 31 + (i >> 13)) & 0xff;
  return bytes;
}

// ---------------------------------------------------------------------------

function suite(mode: string, evicting: boolean): void {
  const it = (name: string, body: () => Promise<void>): void => {
    test(`[${mode}] ${name}`, body);
  };
  const relay = (): Relay => new Relay(evicting, hex(RELAY_SEED));

  it('a control link that is closing but still listed refuses the attach with a code', async () => {
    const r = relay();
    const control = await parked(r);
    // `getWebSockets` can hand back a socket the platform has not finished
    // closing -- `control.ts` refuses to rest "is a host present" on a close
    // handshake with a peer that may be gone, and the fake models it by never
    // removing. An unguarded `send` throws there, `openAttach` rejects, and the
    // browser gets a 500: the one answer it cannot read, where every other
    // refusal on this path is a close code it can.
    control.close(1001, 'going away');

    const ws = await r.attach('browser', 20);
    assert.deepEqual(
      ws.closed,
      { code: CLOSE_HOST_ABSENT, reason: 'no control link for this host' },
      'a host whose link is on its way out is a host that is not there, and the browser is told so',
    );
  });

  it('a peer that is closing but still listed ends the pipe rather than the request', async () => {
    const r = relay();
    const control = await parked(r);
    const p = await r.pipe(control);
    p.host.close(1001, 'going away');

    // The throw lands on the *data* path, so an uncaught one rejects the
    // message handler and strands this leg holding a pipe with nothing at the
    // far end -- worse than the close it is standing in for.
    await r.say(p.client, 'into the void');
    assert.equal(
      p.client.closed?.code,
      CLOSE_PIPE_PEER_GONE,
      'the surviving leg must be told its pipe is over, not left waiting on a peer that cannot receive',
    );
  });

  it('a browser attaches, the host dials back, and bytes cross both ways', async () => {
    const r = relay();
    const control = await parked(r);
    const p = await r.pipe(control);

    await r.say(p.client, 'up');
    await r.say(p.host, 'down');

    assert.deepEqual(
      p.host.sent,
      ['up'],
      'the browser’s bytes reach the daemon and nothing else does — this is the headline, and nothing else in this file matters if it is wrong',
    );
    assert.deepEqual(
      p.client.sent,
      ['down'],
      'and back the other way: a pipe that only carries keystrokes carries no terminal',
    );

    const tags = r.platform.state();
    assert.deepEqual(
      tags.getTags(p.client).slice().sort(),
      [pipeTag(p.id), ROLE_CLIENT].sort(),
      'the pairing is the tags and there is nowhere else it is written down',
    );
    assert.deepEqual(tags.getTags(p.host).slice().sort(), [pipeTag(p.id), ROLE_HOST].sort());
    assert.deepEqual(
      pipeMembership(tags, p.client),
      { tag: pipeTag(p.id), role: ROLE_CLIENT },
      'and a leg knows which end it is from those tags alone, which cannot be rewritten after the accept',
    );
  });

  it('the peer is looked up on the platform once per message, never remembered', async () => {
    const r = relay();
    const control = await parked(r);
    const p = await r.pipe(control);

    // Deliberately no `evict()` here: in the live mode this really is one
    // long-lived instance, which is the only place a peer memoised in a field
    // gives the right answer. The counter is what fails it there.
    const before = r.platform.getWebSocketsCalls;
    for (let i = 0; i < 8; i += 1) await r.say(p.client, `m${i}`);

    assert.equal(
      r.platform.getWebSocketsCalls,
      before + 8,
      'exactly one `getWebSockets` per forwarded message. A peer cached in an instance field answers correctly for a whole live run and drops every session after the first idle gap, and this count is the only assertion that sees it',
    );
    assert.equal(p.host.sent.length, 8, 'and every one of them was actually forwarded');
  });

  it('a pipe evicted mid-session goes on pumping', async () => {
    const r = relay();
    const control = await parked(r);
    const p = await r.pipe(control);

    // `evict()` throws the instance away in *both* modes, so this holds even in
    // the run that otherwise keeps one object alive. An idle gap in a shell
    // session is the ordinary case, not the exotic one.
    await r.say(p.client, 'before');
    r.evict();
    await r.say(p.client, 'after');
    r.evict();
    await r.say(p.host, 'back');

    assert.deepEqual(
      p.host.sent,
      ['before', 'after'],
      'the object is evicted between messages, so a pairing that did not survive the gap would deliver the first and lose the second',
    );
    assert.deepEqual(p.client.sent, ['back'], 'and the return direction survives it too');
  });

  it('the data path writes no storage, no attachment and no query', async () => {
    const r = relay();
    const control = await parked(r);
    const p = await r.pipe(control);

    const writes = r.platform.storage.writes;
    const attachments = r.platform.attachmentWrites;
    const selects = r.db.selects;
    const updates = r.db.updates;

    // Comfortably inside `PIPE_MESSAGE_BURST`, so this measures the cost of
    // pumping rather than the cost of being refused.
    for (let i = 0; i < 300; i += 1) await r.say(p.client, `frame ${i}`);
    for (let i = 0; i < 300; i += 1) await r.say(p.host, `frame ${i}`);

    assert.equal(
      r.platform.storage.writes,
      writes,
      'ADR-009: never write storage on the data path. A write per message costs a request and, worse, keeps the object awake — the dominant cost term goes from zero to continuous',
    );
    assert.equal(
      r.platform.attachmentWrites,
      attachments,
      'and no attachment either, which is the same write wearing a different name',
    );
    assert.equal(r.db.selects + r.db.updates, selects + updates, 'and D1 is not on this path at all');
    assert.equal(p.host.sent.length + p.client.sent.length, 600, 'and all 600 crossed');
  });

  it('an 8 MiB frame crosses whole, byte for byte', async () => {
    const r = relay();
    const control = await parked(r);
    const p = await r.pipe(control);

    const sent = payload(SEALED_MAX_FRAME);
    // Compared against a copy taken before the send, not against the object
    // that was sent: identity would prove nothing about the bytes.
    const expected = Uint8Array.from(sent);
    await r.say(p.client, sent.buffer as ArrayBuffer);

    const got = p.host.sent[0];
    assert.ok(got instanceof ArrayBuffer, 'a binary frame must arrive binary');
    assert.equal(got.byteLength, SEALED_MAX_FRAME);
    assert.equal(
      Buffer.compare(Buffer.from(got), Buffer.from(expected)),
      0,
      'ADR-009 settled that frames cross whole: billing is per message, so splitting an 8 MiB scrollback response into 32 would multiply its cost by 32 on exactly the responses a split was meant to protect',
    );
    assert.ok(
      SEALED_MAX_FRAME < MAX_MESSAGE_BYTES,
      'and the reason it can cross whole is headroom against the edge ceiling, which is the comparison the ADR turns on',
    );
  });

  it('the open frame names a fresh 128-bit id with an absolute deadline', async () => {
    const r = relay();
    const control = await parked(r);
    const first = await r.pipe(control, { client: 'tab-one', host: 'leg-one', timeoutMs: 400 });

    const open = JSON.parse(String(control.sent[control.sent.length - 1])) as Record<string, unknown>;
    assert.equal(open['t'], 'open');
    assert.equal(open['v'], 1);
    assert.equal(
      open['exp'],
      NOW + 400,
      'absolute epoch milliseconds, because a daemon deciding whether a dial is still worth making after a suspend would otherwise have to remember when the frame arrived',
    );
    assert.ok(
      pipeIdIsWellFormed(String(open['pipe'])),
      'the id is the host leg’s only credential, so anything the edge would refuse as malformed must never be issued in the first place',
    );

    const second = await r.pipe(control, { client: 'tab-two', host: 'leg-two' });
    assert.notEqual(
      first.id,
      second.id,
      'an id that repeats is an id a previous browser’s daemon could still dial',
    );

    await r.say(second.client, 'two');
    assert.deepEqual(second.host.sent, ['two']);
    assert.deepEqual(
      first.host.sent,
      [],
      'and two pipes on one host do not cross — the peer is found by the pipe’s tag, not by role alone',
    );
  });

  it('the attach is not resolved until the host has dialled back', async () => {
    const r = relay();
    const control = await parked(r);
    const room = r.room();

    const client = new FakeSock('browser');
    let settled = false;
    const attaching = room.openAttach(client, NOW, 500).then(() => {
      settled = true;
    });

    await turn();
    assert.equal(
      settled,
      false,
      'the browser’s `open` fires when the 101 arrives and `SessionClient` sends its `Hello` immediately after, so a 101 written before the host leg exists produces bytes with nowhere to live but an instance field or a storage write',
    );

    assert.ok(room.openPipeLeg(new FakeSock('host-leg'), pipeOf(control)));
    await attaching;
    assert.equal(settled, true, 'and the dial-back is what releases it');
  });

  it('no control link is one refusal, and a host that never answers is another', async () => {
    const r = relay();

    const orphan = await r.attach('early');
    assert.deepEqual(
      orphan.closed,
      { code: CLOSE_HOST_ABSENT, reason: 'no control link for this host' },
      'nobody is home, and a browser cannot read an HTTP status — the close code is the whole channel',
    );

    const control = await parked(r);
    const before = control.sent.length;
    const silent = await r.attach('late', 10);
    assert.deepEqual(
      silent.closed,
      { code: CLOSE_PIPE_DIAL_TIMEOUT, reason: 'the host did not dial back' },
      '"your Mac is asleep" and "your Mac is here and did not answer" are different problems and only one of them is the user’s',
    );
    assert.equal(
      control.sent.length,
      before + 1,
      'the host really was asked — a control link the platform still holds is only as fresh as its own detection of a peer that vanished',
    );
  });

  it('a dial for an id that names no pipe is refused without a socket', async () => {
    const r = relay();
    const control = await parked(r);
    const p = await r.pipe(control);

    const guess = new FakeSock('guess');
    assert.equal(
      p.room.openPipeLeg(guess, newPipeId()),
      false,
      'the id is the credential; an id nobody issued buys nothing',
    );
    assert.equal(
      p.room.openPipeLeg(new FakeSock('again'), p.id),
      false,
      'and an id that has already been claimed is spent — a second dial cannot displace a live leg',
    );
    assert.equal(
      guess.closed,
      null,
      'no close code, because the far end here is `zest-daemon`, which reads the 404 `fetch` turns this `false` into. A browser is the end that cannot',
    );
    assert.ok(
      !r.platform.sockets.includes(guess),
      'and it was never accepted, so a refused dial costs no hibernatable socket',
    );

    // Late: the deadline passed, the entry deleted itself, and the dial that
    // finally arrives must not claim a pipe whose browser has already been told
    // the host is absent.
    const room = r.room();
    const late = new FakeSock('browser-late');
    await room.openAttach(late, NOW, 5);
    assert.equal(late.closed?.code, CLOSE_PIPE_DIAL_TIMEOUT);
    assert.equal(
      room.openPipeLeg(new FakeSock('too-late'), pipeOf(control)),
      false,
      'a pipe that timed out cannot be claimed afterwards by a dial that finally arrives',
    );
  });

  it('a pipe waiting for its host is claimed by its own id and by no other', async () => {
    const r = relay();
    const control = await parked(r);
    const room = r.room();

    // Two attaches left pending at once, which is the state the refusals above
    // cannot reach: they dial a stranger's id when `#dialling` is empty, so a
    // lookup that ignored the id entirely and took whichever pipe happened to
    // be waiting would pass every one of them. This is where 128 bits either
    // is the credential or is decoration. (Written because exactly that
    // mutation survived the first draft of this file.)
    const first = new FakeSock('tab-one');
    const attachingFirst = room.openAttach(first, NOW, 500);
    const firstId = pipeOf(control);
    const second = new FakeSock('tab-two');
    const attachingSecond = room.openAttach(second, NOW, 500);
    const secondId = pipeOf(control);

    const guess = new FakeSock('guess');
    assert.equal(
      room.openPipeLeg(guess, newPipeId()),
      false,
      'two browsers are waiting and a dial for neither of their ids arrives: a guess must not be handed one of them',
    );
    assert.ok(!r.platform.sockets.includes(guess), 'and it gets no socket, only the 404');

    const secondHost = new FakeSock('leg-two');
    assert.ok(room.openPipeLeg(secondHost, secondId));
    await attachingSecond;
    const firstHost = new FakeSock('leg-one');
    assert.ok(
      room.openPipeLeg(firstHost, firstId),
      'the dials may arrive in either order — the id says which pipe, not the order of the queue',
    );
    await attachingFirst;

    await r.say(first, 'one');
    assert.deepEqual(firstHost.sent, ['one'], 'and each browser reached its own daemon leg');
    assert.deepEqual(secondHost.sent, []);
  });

  it('either end going away closes the other', async () => {
    const r = relay();
    const control = await parked(r);

    // No `forget` before the handler: the platform reports a close on a socket
    // it is still holding — the close handshake is what eventually reaps it —
    // so `webSocketClose` can read the departing leg's own tags, and does.
    const browserFirst = await r.pipe(control, { client: 'tab', host: 'daemon-leg' });
    r.close(browserFirst.client);
    assert.deepEqual(
      browserFirst.host.closed,
      { code: CLOSE_PIPE_PEER_GONE, reason: 'the other end of this pipe is gone' },
      'otherwise a daemon holds a socket and a `serve()` thread for a browser tab that closed hours ago',
    );

    const hostFirst = await r.pipe(control, { client: 'tab-2', host: 'daemon-leg-2' });
    r.close(hostFirst.host);
    assert.equal(
      hostFirst.client.closed?.code,
      CLOSE_PIPE_PEER_GONE,
      'and the same the other way: a browser waiting on a machine that has gone is a spinner that never stops',
    );

    const failed = await r.pipe(control, { client: 'tab-3', host: 'daemon-leg-3' });
    r.fail(failed.host);
    assert.equal(
      failed.client.closed?.code,
      CLOSE_PIPE_PEER_GONE,
      'an error need not be followed by a close, and the half-pipe that would leave behind is silent',
    );
  });

  it('a message for a peer that is already gone closes the sender', async () => {
    const r = relay();
    const control = await parked(r);
    const p = await r.pipe(control);

    // The platform reaping the socket, which `close()` alone does not model: a
    // closed socket keeps appearing in `getWebSockets` until a close handshake
    // finishes with a peer that may already be gone.
    r.platform.forget(p.host);
    await r.say(p.client, 'anyone there');

    assert.deepEqual(
      p.client.closed,
      { code: CLOSE_PIPE_PEER_GONE, reason: 'the other end of this pipe is gone' },
      'a pipe with one end is not a pipe, and the survivor has to be told rather than left writing into nothing',
    );
  });

  it('too many messages, too fast, closes both ends with 1009', async () => {
    const r = relay();
    const control = await parked(r);
    const p = await r.pipe(control);

    // Pinned to the one instance on purpose. The buckets are instance memory
    // *by design* — a bucket lost to an eviction is a bucket rebuilt full, and
    // the idle gap that caused the eviction would have refilled it to full
    // anyway — so a run that evicted between messages is a run in which no
    // flood is ever over its cap. Pinning is the statement of that.
    for (let i = 0; i <= PIPE_MESSAGE_BURST; i += 1) {
      await p.room.webSocketMessage(p.client, 'x', NOW);
    }

    assert.equal(
      p.host.sent.length,
      PIPE_MESSAGE_BURST,
      'the burst is spent exactly, not approximately — a cap that refuses the message before its last is a cap that cuts honest sessions',
    );
    assert.equal(
      p.client.closed?.code,
      CLOSE_PIPE_FLOOD,
      'the flooder is told which it was, and 1009 rather than a 4xxx because it is a code a browser’s `event.code` reports with a meaning anyone can look up',
    );
    assert.equal(CLOSE_PIPE_FLOOD, 1009, 'and that is the number, not merely a constant');
    assert.equal(
      p.host.closed?.code,
      CLOSE_PIPE_PEER_GONE,
      'the pipe is over either way, and leaving the innocent end open would leave it waiting on a peer that has been hung up on',
    );
  });

  it('too many bytes, too fast, closes both ends — and three max frames are not too many', async () => {
    const r = relay();
    const control = await parked(r);
    const p = await r.pipe(control);

    const frame = payload(SEALED_MAX_FRAME).buffer as ArrayBuffer;
    for (let i = 0; i < 4; i += 1) await p.room.webSocketMessage(p.client, frame, NOW);

    assert.equal(
      p.host.sent.length,
      3,
      'the byte burst is three whole sealed `MAX_FRAME`s and cannot be a round number: a bucket that could not admit one largest frame in a single gulp would refuse the very case ADR-009 argues frames must cross whole for',
    );
    assert.equal(p.client.closed?.code, CLOSE_PIPE_FLOOD);
    assert.equal(p.host.closed?.code, CLOSE_PIPE_PEER_GONE);
    assert.equal(
      PIPE_BYTE_BURST_BYTES,
      3 * SEALED_MAX_FRAME,
      'stated here as well as in `limits.ts`, because the assertion above is only meaningful if the two agree',
    );
  });
}

suite('one live instance', false);
suite('a fresh instance per call', true);

// --- the pieces, on their own ----------------------------------------------
//
// Pure functions, so no room and no mode. They are here rather than folded into
// the suites above because a bucket driven through its refill by hand says
// something the pump cannot: the pump only ever sees "admitted" or "refused".

test('a pipe id is 32 lowercase hex, and anything else is refused before a room is woken', () => {
  const id = newPipeId();
  assert.equal(id.length, PIPE_ID_CHARS, '128 bits — short enough to guess is the whole failure mode');
  assert.ok(pipeIdIsWellFormed(id));
  assert.notEqual(newPipeId(), newPipeId(), 'from `crypto.getRandomValues`, not a counter');

  for (const bad of ['', 'z'.repeat(PIPE_ID_CHARS), 'a'.repeat(PIPE_ID_CHARS - 1), 'a'.repeat(PIPE_ID_CHARS + 1), 'A'.repeat(PIPE_ID_CHARS), ` ${'a'.repeat(PIPE_ID_CHARS - 1)}`]) {
    assert.equal(
      pipeIdIsWellFormed(bad),
      false,
      `"${bad}" can never match a pipe this relay issued, so admitting it is one more object wake-up an unauthenticated caller can bill the account for`,
    );
  }
});

test('a pipe tag with no role is not a member, in either direction', () => {
  const platform = new FakePlatform();
  const state = platform.state();
  const orphan = new FakeSock('orphan');
  state.acceptWebSocket(orphan, [pipeTag(newPipeId())]);

  assert.equal(
    pipeMembership(state, orphan),
    null,
    'a socket this build did not accept cannot be pumped in either direction, so it is not half a pipe — it is not a pipe at all',
  );
});

test('a leg’s cost is its bytes, and a string’s is its UTF-8 length', () => {
  assert.equal(messageSize(new ArrayBuffer(1234)), 1234);
  assert.equal(
    messageSize('é😀'),
    6,
    'a code-unit count would charge 3 for these 6 bytes. The pipes carry binary, so this only runs for a peer speaking something unexpected — and undercounting there is the wrong way to be wrong',
  );
});

test('a bucket refills at its rate, and a clock that jumps back hands out nothing twice', () => {
  const budget = new PipeBudget(NOW);
  assert.ok(
    budget.admit(SEALED_MAX_FRAME, NOW),
    'one largest frame in a single gulp, or the relay refuses the case ADR-009 argues for',
  );
  assert.ok(budget.admit(SEALED_MAX_FRAME, NOW));
  assert.ok(budget.admit(SEALED_MAX_FRAME, NOW));
  assert.equal(budget.admit(SEALED_MAX_FRAME, NOW), false, 'the burst is three of them');

  const halfSecond = PIPE_BYTE_RATE_PER_SECOND / 2;
  assert.equal(
    budget.admit(halfSecond + 1, NOW + 500),
    false,
    'refusing spends nothing, so a leg over its cap does not dig itself deeper — and half a second buys exactly half a second’s bytes',
  );
  assert.ok(budget.admit(halfSecond, NOW + 500), 'and not one byte less than that either');

  // The clock going backwards is not hypothetical on a platform that hands the
  // handler a wall clock: a backwards jump that withdrew tokens, or that moved
  // the mark back, would hand the interval it jumped over out a second time.
  const jumpy = new PipeBudget(NOW);
  assert.ok(jumpy.admit(SEALED_MAX_FRAME, NOW + 1000));
  assert.ok(jumpy.admit(SEALED_MAX_FRAME, NOW - 1_000_000));
  assert.ok(jumpy.admit(SEALED_MAX_FRAME, NOW + 1000));
  assert.equal(
    jumpy.admit(SEALED_MAX_FRAME, NOW + 1000),
    false,
    'the mark never moves backwards, so the second `NOW + 1000` refills from the first and not from the jump',
  );
});

test('a pipe that has ended takes both its buckets with it', () => {
  const budgets = new PipeBudgets();
  const tag = pipeTag(newPipeId());
  const other = pipeTag(newPipeId());

  budgets.admit({ tag, role: ROLE_CLIENT }, 1, NOW);
  budgets.admit({ tag, role: ROLE_HOST }, 1, NOW);
  budgets.admit({ tag: other, role: ROLE_CLIENT }, 1, NOW);
  assert.equal(budgets.size, 3, 'keyed by tag and role, never by socket identity — the platform hands back a different object for the same connection after a gap, and a map that missed would hand out a full bucket per message');

  budgets.forget(tag);
  assert.equal(
    budgets.size,
    1,
    'both legs of the ended pipe, and only those: without this a long-lived busy object accumulates one bucket per pipe it ever carried, which is small and unbounded, and unbounded is the property that matters in a process meant to run for days',
  );
});

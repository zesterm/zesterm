import { test } from 'node:test';
import assert from 'node:assert/strict';

import type { DataPlane } from '@zesterm/control';

import { dialFor, type RelayAccess } from '../src/dial-for.ts';
import { installFakeWebSocket, flush } from './fake-websocket.ts';

const HOST = 'cd'.repeat(32);

const handlers = { onOpen: () => {}, onMessage: () => {}, onClose: () => {} };

test('a ws plane dials the daemon socket, at the address dataPlaneUrl builds', async (t) => {
  const sockets = installFakeWebSocket();
  t.after(() => sockets.restore());

  const dial = dialFor({ kind: 'ws', host: '127.0.0.1', port: 7718 }, null);
  assert.ok(dial, 'a ws plane is dialable with no relay configured — the relay is the fallback, not the path');
  dial(handlers);

  assert.equal(
    sockets.latest().url,
    'ws://127.0.0.1:7718',
    'the daemon’s own --listen-ws address, never the sidecar’s port',
  );
});

test('a relay plane dials the relay with a ticket minted for that host', async (t) => {
  const sockets = installFakeWebSocket();
  t.after(() => sockets.restore());
  const asked: string[] = [];
  const relay: RelayAccess = {
    origin: 'https://relay.example.com',
    mintTicket: async (hostId) => {
      asked.push(hostId);
      return 'tkt-9';
    },
  };

  const dial = dialFor({ kind: 'relay', hostId: HOST }, relay);
  assert.ok(dial, 'a relay plane with a known relay is dialable');
  dial(handlers);
  await flush();

  assert.deepEqual(asked, [HOST], 'a ticket names the host it is for, so the mint has to be told which');
  assert.deepEqual(
    sockets.latest().protocols,
    ['zesterm.relay.v1', 'ticket.tkt-9'],
    'the relay leg carries its ticket on the subprotocol, which is what distinguishes it from wsDial',
  );
});

test('the non-dialable states all collapse to null', () => {
  assert.equal(
    dialFor({ kind: 'relay', hostId: HOST }, null),
    null,
    'a relay plane with no relay origin is exactly as dialable as no plane at all, so callers keep their single null check',
  );
  assert.equal(dialFor(null, null), null, 'no plane, no dial');
});

test('a kind this build has never heard of is not dialable', () => {
  // A stale bundle can meet a newer sidecar, and `SessionDirectory`'s write
  // methods are wire-callable — so the impossible arm is reachable at runtime
  // however exhaustive the switch is at compile time.
  const future = { kind: 'quic', host: 'nope' } as unknown as DataPlane;
  assert.equal(
    dialFor(future, null),
    null,
    'returning the never-bound plane instead would pass every caller’s `=== null` check and then be called as a Dial',
  );
});

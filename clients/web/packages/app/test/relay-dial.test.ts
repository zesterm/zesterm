import { test } from 'node:test';
import assert from 'node:assert/strict';

import type { ByteLinkHandlers } from '@zesterm/client';

import { relayDial } from '../src/relay-dial.ts';
import { installFakeWebSocket, flush } from './fake-websocket.ts';

const HOST = 'ab'.repeat(32);

/** Counts what the layer above would see, in the order it would see it. */
function recorder(): ByteLinkHandlers & { readonly log: string[] } {
  const log: string[] = [];
  return {
    log,
    onOpen: () => void log.push('open'),
    onMessage: (bytes) => void log.push(`message:${bytes.length}`),
    onClose: () => void log.push('close'),
  };
}

test('the ticket becomes the second subprotocol, and the URL carries no secret', async (t) => {
  const sockets = installFakeWebSocket();
  t.after(() => sockets.restore());
  const handlers = recorder();

  relayDial('https://relay.example.com', HOST, async () => 'tkt-123')(handlers);
  await flush();

  const ws = sockets.latest();
  assert.deepEqual(
    ws.protocols,
    ['zesterm.relay.v1', 'ticket.tkt-123'],
    'the ticket rides on Sec-WebSocket-Protocol and nowhere else — a secret in a URL lands in referrers, edge logs and history',
  );
  assert.equal(
    ws.url,
    `wss://relay.example.com/v1/attach?host=${HOST}`,
    'an https origin dials wss, and the room is selected by host id alone',
  );
  assert.equal(
    ws.binaryType,
    'arraybuffer',
    'a blob would cost an async read per message, and a delta exists to be applied the moment it lands',
  );
});

test('an http origin dials ws, so a local relay is reachable', async (t) => {
  const sockets = installFakeWebSocket();
  t.after(() => sockets.restore());

  relayDial('http://localhost:8787', HOST, async () => 't')(recorder());
  await flush();

  assert.equal(
    sockets.latest().url,
    `ws://localhost:8787/v1/attach?host=${HOST}`,
    'wrangler dev serves plain http, and a dial that forced wss could never reach it',
  );
});

test('the shell exists before the socket does', async (t) => {
  const sockets = installFakeWebSocket();
  t.after(() => sockets.restore());
  let minted = false;

  const link = relayDial('https://relay.example.com', HOST, async () => {
    minted = true;
    return 't';
  })(recorder());

  assert.equal(
    sockets.created.length,
    0,
    'the mint is a round trip, so a synchronous Dial cannot have opened anything yet — that is the whole reason the seam does not need to be async',
  );
  assert.ok(link, 'the caller gets its ByteLink immediately or the seam would have to change shape');
  await flush();
  assert.ok(minted, 'the dial kicked the mint off rather than waiting to be asked');
  assert.equal(sockets.created.length, 1, 'the socket follows the ticket');
});

test('open, binary message and close reach the handlers; a non-binary message does not', async (t) => {
  const sockets = installFakeWebSocket();
  t.after(() => sockets.restore());
  const handlers = recorder();

  relayDial('https://relay.example.com', HOST, async () => 't')(handlers);
  await flush();
  const ws = sockets.latest();

  ws.onopen?.();
  ws.onmessage?.({ data: new Uint8Array([1, 2, 3]).buffer });
  ws.onmessage?.({ data: 'a text frame' });
  ws.onerror?.();
  ws.onclose?.();

  assert.deepEqual(
    handlers.log,
    ['open', 'message:3', 'close'],
    'text frames are not this protocol, and `error` is not routed because `close` always follows it — routing both would double the redial',
  );
});

test('a mint that fails is a dropped dial, which is the whole point', async (t) => {
  const sockets = installFakeWebSocket();
  t.after(() => sockets.restore());
  const handlers = recorder();

  relayDial('https://relay.example.com', HOST, async () => {
    throw new Error('the ticket endpoint said no');
  })(handlers);
  await flush();

  assert.deepEqual(
    handlers.log,
    ['close'],
    'SessionClient treats a failed dial and a dropped one as one code path, so a failed mint reports the link gone and its backoff ladder retries it',
  );
  assert.equal(sockets.created.length, 0, 'nothing was opened, so there is nothing to leak');
});

test('a mint that resolves into a throwing constructor is still a dropped dial', async (t) => {
  const sockets = installFakeWebSocket();
  t.after(() => sockets.restore());
  const handlers = recorder();

  // The real `WebSocket` constructor throws `SyntaxError` when a subprotocol
  // value is not an RFC 7230 token, and a ticket is a string chosen by an
  // endpoint that does not exist yet — standard base64 (`=`, `/`) would do it.
  // `new URL` throws on a malformed origin the same way.
  //
  // The hazard is not the throw, it is where it lands: a `.then(onOk, onErr)`
  // second argument does not catch `onOk` throwing, so this used to reject
  // unhandled with neither callback firing. `SessionClient` then never
  // schedules a redial and the tab hangs for ever — a silent, terminal wedge
  // rather than the reconnect this file exists to guarantee.
  sockets.throwOnConstruct(new Error('the subprotocol is invalid'));

  relayDial('https://relay.example.com', HOST, async () => 'a-ticket')(handlers);
  await flush();

  assert.deepEqual(
    handlers.log,
    ['close'],
    'a dial that cannot open its socket must report the link gone, or nothing above it ever retries',
  );
});

test('closing before the ticket lands opens no socket, and still reports the link gone', async (t) => {
  const sockets = installFakeWebSocket();
  t.after(() => sockets.restore());
  const handlers = recorder();
  let release: (ticket: string) => void = () => {};

  const link = relayDial(
    'https://relay.example.com',
    HOST,
    () => new Promise<string>((resolve) => (release = resolve)),
  )(handlers);

  link.close();
  assert.deepEqual(
    handlers.log,
    ['close'],
    'onClose is terminal per dial and must fire exactly once — with no socket, nothing else ever would',
  );

  release('tkt-late');
  await flush();

  assert.equal(
    sockets.created.length,
    0,
    'a ticket arriving after the hang-up must not leak a socket: opening one to close it again still costs a connection at the edge and a room wake-up',
  );
  assert.deepEqual(handlers.log, ['close'], 'and it must not report the link gone a second time');
});

test('closing an open link closes its socket, and reports gone once', async (t) => {
  const sockets = installFakeWebSocket();
  t.after(() => sockets.restore());
  const handlers = recorder();

  const link = relayDial('https://relay.example.com', HOST, async () => 't')(handlers);
  await flush();
  const ws = sockets.latest();
  ws.onopen?.();

  link.close();
  link.close();

  assert.equal(ws.closeCalls, 1, 'the second close is a no-op rather than a second hang-up');
  assert.deepEqual(
    handlers.log,
    ['open', 'close'],
    'the socket’s own onclose reports it — reporting again here would double the redial',
  );
});

test('send reaches the socket once it exists, and is dropped rather than queued before', async (t) => {
  const sockets = installFakeWebSocket();
  t.after(() => sockets.restore());

  const link = relayDial('https://relay.example.com', HOST, async () => 't')(recorder());
  // Nothing above sends before `onOpen()`, so this cannot happen — but if it
  // ever does, dropping is what keeps the shell a pass-through: a queue here
  // would be the state machine the synchronous seam exists to avoid.
  link.send(new Uint8Array([1]));

  await flush();
  sockets.latest().onopen?.();
  link.send(new Uint8Array([7, 7]));

  assert.deepEqual(
    sockets.latest().sent,
    [new Uint8Array([7, 7])],
    'bytes go out verbatim, and the pre-open byte was neither replayed nor buffered',
  );
});

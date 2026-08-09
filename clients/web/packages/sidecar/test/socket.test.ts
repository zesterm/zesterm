/**
 * The whole control plane over a real socket: the sidecar's HTTP server, the
 * actors WebSocket, and `socketTransport` dialling in exactly as the browser
 * will — including a live subscription seeing a feed write.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { createHost, memoryStorage } from '@sigx/actors/host';
import { socketTransport } from '@sigx/actors-ws/client';
import { WebSocket as WsClient } from 'ws';

import { LOCAL_DIRECTORY_KEY, SessionDirectory } from '@zesterm/control';
import { startServer } from '../src/index.ts';

const quiet = { sweepIntervalMs: 60_000, reminderTickMs: 60_000, callTimeoutMs: 0 };

test('a socketTransport client reads the directory a feed write updated', async () => {
  const host = createHost({
    actors: [SessionDirectory],
    storage: memoryStorage(),
    defaults: quiet,
  });
  await host.start();
  const server = await startServer({ host, port: 0, allowOrigins: [] });
  const port = (server.address() as { port: number }).port;

  // Node's `ws` through the same `connect` seam Lynx will use — the browser
  // is the only runtime that gets the `url` sugar's global WebSocket. The
  // Origin header is set the way a browser would, because the point is to
  // pass the same `same-origin` posture production runs under, not to turn
  // it off for the test.
  const transport = socketTransport({
    connect: (handlers) => {
      const ws = new WsClient(`ws://127.0.0.1:${port}/_sigx/socket`, {
        origin: `http://127.0.0.1:${port}`,
      });
      ws.on('open', () => handlers.onOpen());
      ws.on('message', (data: Buffer) => handlers.onMessage(String(data)));
      ws.on('close', () => handlers.onClose());
      ws.on('error', () => {});
      return { send: (m: string) => ws.send(m), close: () => ws.close() };
    },
  });

  try {
    const view = (await transport.call(`SessionDirectory#list`, [LOCAL_DIRECTORY_KEY])) as {
      connected: boolean;
      sessions: unknown[];
    };
    assert.equal(view.connected, false, 'a fresh directory reports no daemon link');

    // The live subscription is the session list UI's whole mechanism.
    const seen: Array<{ sessions: Array<{ title: string }> }> = [];
    const live = transport.live?.();
    assert.ok(live, 'the socket transport must carry live subscriptions');
    const off = live.subscribe(
      { type: 'SessionDirectory', key: LOCAL_DIRECTORY_KEY, method: 'list' },
      (value) => seen.push(value as { sessions: Array<{ title: string }> }),
    );

    await waitFor(() => seen.length >= 1, 'the initial live value never arrived');

    // The feed writes in-process, as the sidecar does; the subscriber must
    // hear about it over the socket without asking.
    await host.actor(SessionDirectory, LOCAL_DIRECTORY_KEY).replaceAll(
      [
        {
          host: '2e'.repeat(32),
          session: '1',
          title: 'pushed-live',
          cwd: '/',
          cols: 80,
          rows: 24,
          altScreen: false,
          attached: false,
        },
      ],
      null,
    );
    await waitFor(
      () => seen.some((v) => v.sessions.some((s) => s.title === 'pushed-live')),
      'the live subscription never saw the feed write',
    );
    off();
  } finally {
    void transport.close?.();
    await new Promise<void>((resolve) => server.close(() => resolve()));
    await host.stop();
  }
});

async function waitFor(check: () => boolean, why: string): Promise<void> {
  const deadline = Date.now() + 5000;
  while (!check()) {
    if (Date.now() > deadline) assert.fail(why);
    await new Promise((r) => setTimeout(r, 20));
  }
}

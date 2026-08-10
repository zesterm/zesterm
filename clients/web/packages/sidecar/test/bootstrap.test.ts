/**
 * `/api/bootstrap` over the sidecar's real HTTP server.
 *
 * The app's own suite covers parsing and the fallback; this covers the half
 * that only a socket can: that the sidecar actually answers, at that path, with
 * that body, and that it does so *before* the SPA fallback gets a look in.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { createHost, memoryStorage } from '@sigx/actors/host';
import { SessionDirectory } from '@zesterm/control';
import { startServer } from '../src/index.ts';

const quiet = { sweepIntervalMs: 60_000, reminderTickMs: 60_000, callTimeoutMs: 0 };

async function withServer(
  staticDir: string | undefined,
  body: (base: string) => Promise<void>,
): Promise<void> {
  const host = createHost({ actors: [SessionDirectory], storage: memoryStorage(), defaults: quiet });
  await host.start();
  const server = await startServer({
    host,
    port: 0,
    ...(staticDir === undefined ? {} : { staticDir }),
  });
  const port = (server.address() as { port: number }).port;
  try {
    await body(`http://127.0.0.1:${port}`);
  } finally {
    await new Promise<void>((r) => server.close(() => r()));
    await host.stop?.();
  }
}

test('the sidecar answers bootstrap with local', async () => {
  await withServer(undefined, async (base) => {
    const res = await fetch(`${base}/api/bootstrap`);
    assert.equal(res.status, 200);
    assert.match(res.headers.get('content-type') ?? '', /application\/json/);
    assert.deepEqual(await res.json(), { mode: 'local' });
  });
});

test('bootstrap is answered in dev mode too, with no static dir', async () => {
  // The vite dev server proxies /api here. If this 404'd, the client's
  // fallback would quietly cover it -- and a fallback exercised every day is
  // one nobody notices has stopped agreeing with the server.
  await withServer(undefined, async (base) => {
    assert.equal((await fetch(`${base}/api/bootstrap`)).status, 200);
    // ...while everything else in dev mode is still the sidecar's 404.
    assert.equal((await fetch(`${base}/index.html`)).status, 404);
  });
});

test('bootstrap is not swallowed by the SPA fallback when static files are served', async () => {
  // With a staticDir, an unknown path returns index.html. `/api/bootstrap`
  // must not: handing a fetch expecting JSON a page of markup surfaces as a
  // parse error naming a line of HTML, which points nowhere near the cause.
  await withServer(new URL('./fixtures', import.meta.url).pathname, async (base) => {
    const res = await fetch(`${base}/api/bootstrap`);
    assert.match(res.headers.get('content-type') ?? '', /application\/json/);
    assert.deepEqual(await res.json(), { mode: 'local' });

    // The fallback itself still works for a client-side route.
    const spa = await fetch(`${base}/hosts`);
    assert.equal(spa.status, 200);
    assert.match(await spa.text(), /<!doctype html>/i);
  });
});

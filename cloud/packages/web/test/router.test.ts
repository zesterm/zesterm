import { test } from 'node:test';
import assert from 'node:assert/strict';

import { routeApi } from '../src/router.ts';
import type { Bootstrap } from '../src/bootstrap.ts';

const get = (path: string) => new Request(`https://zesterm-web.example.workers.dev${path}`);

test('bootstrap says cloud, and says the visitor is signed out', async () => {
  const res = routeApi(get('/api/bootstrap'));
  assert.ok(res, '/api/bootstrap must be handled by the Worker, not the assets');
  assert.equal(res.status, 200);

  const body = (await res.json()) as Bootstrap;
  assert.equal(body.mode, 'cloud', 'the sidecar answers "local"; this is the edge');
  assert.equal(body.mode === 'cloud' ? body.user : undefined, null);
});

test('api answers are never cached', async () => {
  // Every /api/* answer is per-user the moment accounts land, and a response
  // cached at the edge before that is one served to the wrong person after it.
  const res = routeApi(get('/api/bootstrap'));
  assert.equal(res?.headers.get('cache-control'), 'no-store');
  assert.match(res?.headers.get('content-type') ?? '', /^application\/json/);
});

test('a non-api path is declined so the assets can serve the app', () => {
  // `null` is load-bearing: it is what makes index.html reach the browser.
  for (const path of ['/', '/hosts', '/h/abc/s/1', '/assets/index-abc123.js']) {
    assert.equal(routeApi(get(path)), null, `${path} must fall through to ASSETS`);
  }
});

test('an unknown /api path is JSON 404, never the SPA fallback', async () => {
  // Falling through would hand JavaScript a 200 full of HTML, which surfaces
  // as a JSON parse error naming a line of markup and tells you nothing.
  const res = routeApi(get('/api/nope'));
  assert.ok(res, 'an unknown /api path must not reach the asset fallback');
  assert.equal(res.status, 404);
  assert.match(res.headers.get('content-type') ?? '', /^application\/json/);
});

test('bootstrap refuses a method it does not implement', async () => {
  const res = routeApi(
    new Request('https://zesterm-web.example.workers.dev/api/bootstrap', { method: 'POST' }),
  );
  assert.equal(res?.status, 405);
});

test('/api is not a prefix match on the app own routes', () => {
  // `/apiary` starts with "/api" but is not an API path. `startsWith('/api/')`
  // rather than `startsWith('/api')` is the whole difference, and it is the
  // kind of thing that only breaks once someone names a route badly.
  assert.equal(routeApi(get('/apiary')), null);
});

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { dataPlaneUrl } from '../src/data-plane-url.ts';

test('a ws plane is the daemon socket, verbatim', () => {
  assert.equal(
    dataPlaneUrl({ kind: 'ws', host: '127.0.0.1', port: 7718 }),
    'ws://127.0.0.1:7718',
    'the URL is the daemon’s own --listen-ws address, never the sidecar’s port',
  );
});

test('a relay plane is not a URL, and says so the same way "no plane" does', () => {
  assert.equal(
    dataPlaneUrl({ kind: 'relay', hostId: 'ab'.repeat(32) }),
    null,
    'a relay pipe needs a minted ticket before a socket exists, so the row stays disabled rather than dialling a URL that cannot work',
  );
  assert.equal(
    dataPlaneUrl(null),
    null,
    'both non-dialable states collapse to one, so every caller keeps its single null check',
  );
});

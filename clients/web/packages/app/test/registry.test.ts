import { test } from 'node:test';
import assert from 'node:assert/strict';

import { fetchRegistry, mintEnrollCode, parseDevice, parseHost, revoke } from '../src/registry.ts';

const HOST = { id: 'a'.repeat(64), label: 'andy-mac', platform: 'macos', enrolledAt: 1, lastSeenAt: 2 };
const DEVICE = {
  id: 'b'.repeat(64),
  label: 'this browser',
  kind: 'browser',
  extractable: false,
  enrolledAt: 1,
  lastSeenAt: null,
};

/** A `fetch` that answers each path from a table. */
function serving(table: Record<string, unknown>, status = 200): typeof fetch {
  return (async (url: string) => {
    const body = table[url];
    if (body === undefined) throw new Error(`unexpected fetch: ${url}`);
    return new Response(JSON.stringify(body), {
      status,
      headers: { 'content-type': 'application/json' },
    });
  }) as unknown as typeof fetch;
}

test('both lists are read, and the envelopes are the Worker\'s', async () => {
  const got = await fetchRegistry(
    serving({ '/api/hosts': { hosts: [HOST] }, '/api/devices': { devices: [DEVICE] } }),
  );
  assert.equal(got.hosts.length, 1);
  assert.equal(got.hosts[0]?.label, 'andy-mac');
  assert.equal(got.devices.length, 1);
  assert.equal(got.devices[0]?.kind, 'browser');
});

test('a row missing what a row needs is dropped, not rendered as undefined', async () => {
  // The screen shows `label` in bold. A row without one would read as a blank
  // entry someone could revoke, which is worse than not listing it.
  const got = await fetchRegistry(
    serving({
      '/api/hosts': { hosts: [HOST, { id: 'x'.repeat(64) }, null, 'nonsense'] },
      '/api/devices': { devices: [DEVICE, { ...DEVICE, kind: 'toaster' }] },
    }),
  );
  assert.equal(got.hosts.length, 1, 'only the complete host survives');
  assert.equal(got.devices.length, 1, 'a kind nobody renders is not a device');
});

test('a key of unknown safety is assumed readable, not assumed safe', async () => {
  // The cautious direction: an absent `extractable` makes the screen over-warn
  // rather than quietly promise a key cannot be read by script on the origin.
  const { extractable, ...withoutFlag } = DEVICE;
  void extractable;
  assert.equal(parseDevice(withoutFlag)?.extractable, true);
  assert.equal(parseDevice({ ...DEVICE, extractable: false })?.extractable, false);
});

test('a host with no platform still lists', () => {
  // `platform` is optional on the daemon's side, so an absent one is ordinary
  // and must not drop the machine off the screen.
  const { platform, ...withoutPlatform } = HOST;
  void platform;
  assert.equal(parseHost(withoutPlatform)?.platform, '');
});

test('a failed list is an error the screen can show, not an empty account', async () => {
  // Rendering "no machines yet" because the request 500'd would tell someone
  // their fleet is gone.
  await assert.rejects(
    fetchRegistry(serving({ '/api/hosts': { hosts: [] }, '/api/devices': { devices: [] } }, 500)),
  );
});

test('revoke is a JSON POST, because anything else is refused 403', async () => {
  // The Worker's CSRF rule requires the Origin *and* a JSON content-type; a
  // link or a form cannot satisfy it, and that refusal is the rule working.
  let seen: { method?: string | undefined; ct?: string | undefined; credentials?: string | undefined } = {};
  const capturing: typeof fetch = ((url: string, init?: RequestInit) => {
    seen = {
      method: init?.method,
      ct: (init?.headers as Record<string, string> | undefined)?.['content-type'],
      credentials: init?.credentials,
    };
    assert.match(url, /^\/api\/hosts\/[0-9a-f]{64}\/revoke$/);
    return Promise.resolve(new Response(null, { status: 204 }));
  }) as unknown as typeof fetch;

  await revoke('hosts', HOST.id, capturing);
  assert.equal(seen.method, 'POST');
  assert.equal(seen.ct, 'application/json');
  assert.equal(seen.credentials, 'same-origin', 'the session cookie has to be sent');
});

test('an id is escaped into the path rather than concatenated', async () => {
  // Ids come from the server today, but the moment one is typed or pasted this
  // is the difference between a 404 and a request to another route.
  let path = '';
  const capturing: typeof fetch = ((url: string) => {
    path = url;
    return Promise.resolve(new Response(null, { status: 204 }));
  }) as unknown as typeof fetch;

  await revoke('devices', 'a/../hosts/b', capturing);
  assert.ok(!path.includes('/../'), `path escaped the route: ${path}`);
  assert.ok(path.startsWith('/api/devices/'), path);
});

test('a refused revoke is reported rather than swallowed', async () => {
  const refusing: typeof fetch = (() =>
    Promise.resolve(new Response(null, { status: 403 }))) as unknown as typeof fetch;
  await assert.rejects(revoke('hosts', HOST.id, refusing), /403/);
});

test('minting is a JSON POST carrying the kind, because anything else is refused 403', async () => {
  // Same CSRF posture as revoke: Origin plus a JSON content-type, with the
  // session cookie — a link or a form cannot mint a code, by design.
  let seen: {
    url?: string;
    method?: string | undefined;
    ct?: string | undefined;
    credentials?: string | undefined;
    body?: unknown;
  } = {};
  const capturing: typeof fetch = ((url: string, init?: RequestInit) => {
    seen = {
      url,
      method: init?.method,
      ct: (init?.headers as Record<string, string> | undefined)?.['content-type'],
      credentials: init?.credentials,
      body: init?.body,
    };
    return Promise.resolve(
      new Response(JSON.stringify({ code: 'ZK7M2Q9T', expiresAt: 1_000 }), {
        status: 200,
        headers: { 'content-type': 'application/json' },
      }),
    );
  }) as unknown as typeof fetch;

  const got = await mintEnrollCode('host', capturing);
  assert.equal(seen.url, '/api/enroll/code');
  assert.equal(seen.method, 'POST');
  assert.equal(seen.ct, 'application/json');
  assert.equal(seen.credentials, 'same-origin', 'the session cookie has to be sent');
  assert.deepEqual(
    JSON.parse(seen.body as string),
    { kind: 'host' },
    'the kind travels in the body — host and device codes enrol different tables',
  );
  assert.deepEqual(got, { code: 'ZK7M2Q9T', expiresAt: 1_000 });
});

test('a wrong-shaped mint answer is an error, not a panel showing undefined', async () => {
  // The code is about to be typed into another machine verbatim; a panel
  // rendering `undefined` teaches someone to type `undefined`.
  const bodies: unknown[] = [
    {},
    { code: 'ZK7M2Q9T' },
    { expiresAt: 1_000 },
    { code: '', expiresAt: 1_000 },
    { code: 'ZK7M2Q9T', expiresAt: 'soon' },
    null,
    'nonsense',
  ];
  for (const body of bodies) {
    const serving: typeof fetch = (() =>
      Promise.resolve(
        new Response(JSON.stringify(body), {
          status: 200,
          headers: { 'content-type': 'application/json' },
        }),
      )) as unknown as typeof fetch;
    await assert.rejects(
      mintEnrollCode('device', serving),
      /wrong shape/,
      `shape ${JSON.stringify(body)} must be refused`,
    );
  }
});

test('a refused mint is reported rather than swallowed', async () => {
  const refusing: typeof fetch = (() =>
    Promise.resolve(new Response(null, { status: 403 }))) as unknown as typeof fetch;
  await assert.rejects(mintEnrollCode('host', refusing), /403/);
});

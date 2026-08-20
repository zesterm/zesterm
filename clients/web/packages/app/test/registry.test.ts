import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  ATTESTATION_TTL_MS,
  attestDevice,
  generateIdentity,
  registerRequest,
  seedSigner,
  verifyClientSignature,
} from '@zesterm/auth';

import {
  approveDevice,
  fetchRegistry,
  mintEnrollCode,
  parseDevice,
  fetchEvents,
  parseEvent,
  parseHost,
  registerDevice,
  restore,
  revoke,
} from '../src/registry.ts';

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

// What `fetchRegistry` actually requests: the owner's recovery view, so a
// revoked machine has somewhere to be seen and restored. The Worker ignores
// the parameter for bearer callers and older deployments never read it, so
// asking is always safe.
const HOSTS_URL = '/api/hosts?include=revoked';
const DEVICES_URL = '/api/devices?include=revoked';

test('both lists are read, and the envelopes are the Worker\'s', async () => {
  const got = await fetchRegistry(
    serving({ [HOSTS_URL]: { hosts: [HOST] }, [DEVICES_URL]: { devices: [DEVICE] } }),
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
      [HOSTS_URL]: { hosts: [HOST, { id: 'x'.repeat(64) }, null, 'nonsense'] },
      [DEVICES_URL]: { devices: [DEVICE, { ...DEVICE, kind: 'toaster' }] },
    }),
  );
  assert.equal(got.hosts.length, 1, 'only the complete host survives');
  assert.equal(got.devices.length, 1, 'a kind nobody renders is not a device');
});

test('revokedAt is read when present and null when the Worker predates it', () => {
  // Absent must read as live: an older Worker lists only live rows and says
  // nothing about revocation, and inventing "revoked" for every row on it
  // would render the whole account into the recovery section.
  assert.equal(parseHost(HOST)?.revokedAt, null);
  assert.equal(parseHost({ ...HOST, revokedAt: 7 })?.revokedAt, 7);
  assert.equal(parseDevice(DEVICE)?.revokedAt, null);
  assert.equal(parseDevice({ ...DEVICE, revokedAt: 7 })?.revokedAt, 7);
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
    fetchRegistry(serving({ [HOSTS_URL]: { hosts: [] }, [DEVICES_URL]: { devices: [] } }, 500)),
  );
});

test('an event row missing what a line needs is dropped, not rendered as undefined', () => {
  const good = {
    action: 'revoke',
    actor: 'owner',
    subjectKind: 'host',
    subjectLabel: 'andy-mac',
    at: 5,
  };
  assert.equal(parseEvent(good)?.subjectLabel, 'andy-mac');
  assert.equal(parseEvent({ ...good, action: 'exploded' }), null, 'a verb nobody renders');
  assert.equal(parseEvent({ ...good, actor: 'ghost' }), null, 'an authority nobody defined');
  assert.equal(parseEvent({ ...good, subjectLabel: '' }), null, 'a line about nothing named');
  assert.equal(parseEvent({ ...good, at: 'yesterday' }), null);
  assert.equal(parseEvent(null), null);
});

test('fetchEvents reads the envelope and drops what does not parse', async () => {
  const got = await fetchEvents(
    serving({
      '/api/registry/events': {
        events: [
          { action: 'restore', actor: 'owner', subjectKind: 'device', subjectLabel: 'Edge', at: 9 },
          'nonsense',
        ],
      },
    }),
  );
  assert.equal(got.length, 1);
  assert.equal(got[0]?.action, 'restore');
});

test('restore is a JSON POST, the same posture as revoke', async () => {
  // The way back in for a machine revoked by mistake: same CSRF posture,
  // because the Worker refuses anything else 403.
  let seen: { method?: string | undefined; ct?: string | undefined; credentials?: string | undefined } = {};
  const capturing: typeof fetch = ((url: string, init?: RequestInit) => {
    seen = {
      method: init?.method,
      ct: (init?.headers as Record<string, string> | undefined)?.['content-type'],
      credentials: init?.credentials,
    };
    assert.match(url, /^\/api\/devices\/[0-9a-f]{64}\/restore$/);
    return Promise.resolve(new Response(null, { status: 200 }));
  }) as unknown as typeof fetch;

  await restore('devices', DEVICE.id, capturing);
  assert.equal(seen.method, 'POST');
  assert.equal(seen.ct, 'application/json');
  assert.equal(seen.credentials, 'same-origin', 'the session cookie has to be sent');
});

test('a refused restore is reported rather than swallowed', async () => {
  const refusing: typeof fetch = (() =>
    Promise.resolve(new Response(null, { status: 404 }))) as unknown as typeof fetch;
  await assert.rejects(restore('hosts', HOST.id, refusing), /404/);
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
    accept?: string | undefined;
    credentials?: string | undefined;
    body?: unknown;
  } = {};
  const capturing: typeof fetch = ((url: string, init?: RequestInit) => {
    seen = {
      url,
      method: init?.method,
      ct: (init?.headers as Record<string, string> | undefined)?.['content-type'],
      accept: (init?.headers as Record<string, string> | undefined)?.['accept'],
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
  assert.equal(seen.accept, 'application/json', 'a JSON answer is read back, so the request says it takes one');
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

test('a device row absent a status is read as approved', () => {
  // Backward tolerance for a Worker deployed before the column existed —
  // whose rows are all grandfathered approved by the migration anyway. The
  // cautious direction is inverted from `extractable` on purpose: a false
  // "pending" banner tells someone their working browser is locked out.
  assert.equal(parseDevice(DEVICE)?.status, 'approved');
  assert.equal(parseDevice({ ...DEVICE, status: 'pending' })?.status, 'pending');
  assert.equal(parseDevice({ ...DEVICE, status: 'approved' })?.status, 'approved');
});

test('registerDevice signs its own key and POSTs the standard CSRF posture', async () => {
  const signer = seedSigner(generateIdentity('07'.repeat(32)));
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
      body: JSON.parse(init?.body as string),
    };
    return Promise.resolve(
      new Response(
        JSON.stringify({
          device: {
            id: signer.clientId,
            label: 'Firefox',
            kind: 'browser',
            extractable: true,
            status: 'pending',
            enrolledAt: 5,
            lastSeenAt: null,
          },
        }),
        { status: 200, headers: { 'content-type': 'application/json' } },
      ),
    );
  }) as unknown as typeof fetch;

  const got = await registerDevice(
    { signer, account: 'user-a', label: 'Firefox', extractable: true },
    capturing,
  );

  assert.equal(seen.url, '/api/devices/register');
  assert.equal(seen.method, 'POST');
  assert.equal(seen.ct, 'application/json', 'the Worker CSRF rule refuses anything else 403');
  assert.equal(seen.credentials, 'same-origin', 'the session cookie has to be sent');
  const body = seen.body as { deviceId: string; label: string; kind: string; extractable: boolean; sig: string };
  assert.equal(body.deviceId, signer.clientId, 'the key registered is the key that signed');
  assert.equal(body.kind, 'browser');
  assert.equal(body.extractable, true);
  assert.ok(
    verifyClientSignature(
      signer.clientId,
      'enrollment',
      registerRequest('user-a', signer.clientId, 'Firefox'),
      body.sig,
    ),
    'the sig must be exactly what the Worker will verify: the register request under the client enrollment domain',
  );
  assert.equal(got.status, 'pending', 'the parsed row is what the caller renders');
});

test('a wrong-shaped register answer is an error, not a device made of undefined', async () => {
  const signer = seedSigner(generateIdentity('07'.repeat(32)));
  const serving: typeof fetch = (() =>
    Promise.resolve(
      new Response(JSON.stringify({ device: { id: signer.clientId } }), {
        status: 200,
        headers: { 'content-type': 'application/json' },
      }),
    )) as unknown as typeof fetch;
  await assert.rejects(
    registerDevice({ signer, account: 'user-a', label: 'x', extractable: true }, serving),
    /wrong shape/,
  );
});

test('a refused registration is reported rather than swallowed', async () => {
  const signer = seedSigner(generateIdentity('07'.repeat(32)));
  const refusing: typeof fetch = (() =>
    Promise.resolve(new Response(null, { status: 429 }))) as unknown as typeof fetch;
  await assert.rejects(
    registerDevice({ signer, account: 'user-a', label: 'x', extractable: true }, refusing),
    /429/,
    'the caller decides whether silence is right — the fleet auto-register swallows, a future button must not',
  );
});

test('approveDevice signs an attestation as this browser and POSTs the CSRF posture', async () => {
  const signer = seedSigner(generateIdentity('07'.repeat(32)));
  const target = { id: 'b'.repeat(64), label: 'new browser' };
  const NOW = 1_700_000_000_000;
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
      body: JSON.parse(init?.body as string),
    };
    return Promise.resolve(
      new Response(
        JSON.stringify({
          device: {
            id: target.id,
            label: target.label,
            kind: 'browser',
            extractable: true,
            status: 'approved',
            enrolledAt: 5,
            lastSeenAt: null,
          },
        }),
        { status: 200, headers: { 'content-type': 'application/json' } },
      ),
    );
  }) as unknown as typeof fetch;

  const got = await approveDevice(
    { signer, account: 'user-a', device: target, now: NOW },
    capturing,
  );

  assert.equal(seen.url, `/api/devices/${target.id}/approve`);
  assert.equal(seen.method, 'POST');
  assert.equal(seen.ct, 'application/json', 'the Worker CSRF rule refuses anything else 403');
  assert.equal(seen.credentials, 'same-origin', 'the session cookie has to be sent');

  // Ed25519 is deterministic, so the strongest check is byte equality: the
  // posted blob must be exactly what the auth helper builds for these fields
  // — same window, same label, signed by this browser as `by`.
  const expected = await attestDevice(signer, {
    account: 'user-a',
    device: target.id,
    label: target.label,
    iat: NOW,
    exp: NOW + ATTESTATION_TTL_MS,
  });
  assert.deepEqual(seen.body, { attestation: expected });
  assert.equal(got.status, 'approved', 'the parsed row is what the caller refetches around');
});

test('a refused approval is reported rather than swallowed', async () => {
  const signer = seedSigner(generateIdentity('07'.repeat(32)));
  const refusing: typeof fetch = (() =>
    Promise.resolve(new Response(null, { status: 403 }))) as unknown as typeof fetch;
  await assert.rejects(
    approveDevice(
      { signer, account: 'user-a', device: { id: 'b'.repeat(64), label: 'x' }, now: 1 },
      refusing,
    ),
    /403/,
  );
});

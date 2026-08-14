import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  approveLinkGrant,
  denyLinkGrant,
  fetchLinkGrant,
  fingerprintGroups,
  grantFromQuery,
  parseLinkGrant,
} from '../src/link.ts';

const GRANT = {
  label: 'andy-desktop',
  kind: 'desktop',
  platform: 'macos',
  fingerprint: 'ab12cd34',
  approved: false,
  expiresAt: 1_000,
};

test('a grant answer missing what the page renders is dropped, not shown as undefined', () => {
  // This page's whole job is to be read carefully before a click; a card
  // saying `undefined` teaches people to approve without reading.
  assert.deepEqual(parseLinkGrant(GRANT), GRANT, 'the complete answer survives');
  assert.equal(parseLinkGrant({ ...GRANT, label: '' }), null, 'an empty label');
  assert.equal(parseLinkGrant({ ...GRANT, kind: 'toaster' }), null, 'a kind nobody renders');
  assert.equal(
    parseLinkGrant({ ...GRANT, fingerprint: 'AB12CD34' }),
    null,
    'the fingerprint is compared character-for-character against the app, so casing must be canonical',
  );
  assert.equal(parseLinkGrant({ ...GRANT, fingerprint: 'ab12' }), null, 'a short fingerprint');
  assert.equal(parseLinkGrant(null), null);
  const { platform, ...noPlatform } = GRANT;
  void platform;
  assert.equal(parseLinkGrant(noPlatform)?.platform, '', 'platform is optional, like everywhere');
});

test('an absent approved flag reads as not approved', () => {
  // The cautious direction: the page then shows a button whose press is
  // idempotent, rather than telling someone an unapproved device is in.
  const { approved, ...withoutFlag } = GRANT;
  void approved;
  assert.equal(parseLinkGrant(withoutFlag)?.approved, false);
  assert.equal(parseLinkGrant({ ...GRANT, approved: true })?.approved, true);
});

test('the fingerprint is spaced for comparing, four characters at a time', () => {
  assert.equal(fingerprintGroups('ab12cd34'), 'ab12 cd34');
  assert.equal(fingerprintGroups('ab12'), 'ab12', 'no trailing space on a single group');
});

test('only a well-shaped grant id is read out of the query', () => {
  const id = 'A'.repeat(43);
  assert.equal(grantFromQuery(id), id);
  assert.equal(grantFromQuery(undefined), null, 'no query at all');
  assert.equal(grantFromQuery(['a', 'b']), null, 'a repeated parameter is nobody’s grant');
  assert.equal(grantFromQuery('A'.repeat(42)), null, 'wrong length');
  assert.equal(grantFromQuery(`${'A'.repeat(42)}=`), null, 'padding is not in the alphabet');
});

test('the reads and writes carry the cookie posture, and the id is escaped into the path', async () => {
  const seen: Array<{ url: string; method: string | undefined; credentials: string | undefined; ct: string | undefined }> = [];
  const capturing: typeof fetch = ((url: string, init?: RequestInit) => {
    seen.push({
      url,
      method: init?.method,
      credentials: init?.credentials,
      ct: (init?.headers as Record<string, string> | undefined)?.['content-type'],
    });
    return Promise.resolve(
      new Response(JSON.stringify(GRANT), {
        status: 200,
        headers: { 'content-type': 'application/json' },
      }),
    );
  }) as unknown as typeof fetch;

  await fetchLinkGrant('a/../evil', capturing);
  await approveLinkGrant('g'.repeat(43), capturing);
  await denyLinkGrant('g'.repeat(43), capturing);

  assert.ok(!seen[0]!.url.includes('/../'), 'an id is escaped, never concatenated');
  assert.equal(seen[0]!.credentials, 'same-origin', 'the read is the approver’s session');
  assert.equal(seen[1]!.method, 'POST');
  assert.equal(seen[1]!.ct, 'application/json', 'the Worker CSRF rule refuses anything else 403');
  assert.equal(seen[1]!.url, `/api/link/${'g'.repeat(43)}/approve`);
  assert.equal(seen[2]!.url, `/api/link/${'g'.repeat(43)}/deny`);
});

test('a dead grant is null from the read, and a refused answer throws from the writes', async () => {
  const refusing: typeof fetch = (() =>
    Promise.resolve(new Response(JSON.stringify({ error: 'not_found' }), { status: 404 }))) as unknown as typeof fetch;
  assert.equal(
    await fetchLinkGrant('x'.repeat(43), refusing),
    null,
    'every dead grant is one answer to the page: "no longer valid"',
  );
  await assert.rejects(approveLinkGrant('x'.repeat(43), refusing), /404/);
  await assert.rejects(denyLinkGrant('x'.repeat(43), refusing), /404/);
});

test('a wrong-shaped 200 is null, not a card of undefineds', async () => {
  const serving: typeof fetch = (() =>
    Promise.resolve(
      new Response(JSON.stringify({ label: 'x' }), {
        status: 200,
        headers: { 'content-type': 'application/json' },
      }),
    )) as unknown as typeof fetch;
  assert.equal(await fetchLinkGrant('x'.repeat(43), serving), null);
});

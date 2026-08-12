import { test } from 'node:test';
import assert from 'node:assert/strict';

import { ago, fingerprintDisplay, hostCard } from '../src/fleet-model.ts';
import type { Host } from '../src/registry.ts';

test('fingerprintDisplay keeps head and tail of a long key', () => {
  const key = 'a'.repeat(30) + 'zzzz';
  const shown = fingerprintDisplay(key);
  assert.equal(shown, 'aaaa…zzzz', 'head+tail is what lets two keys be told apart at a glance');
  assert.ok(shown.length < key.length, 'a 64-hex key does not fit an 11.5px card row');
});

test('fingerprintDisplay handles even and odd key lengths without dropping ends', () => {
  for (const key of ['0123456789ab', '0123456789abc']) {
    const shown = fingerprintDisplay(key);
    assert.ok(shown.startsWith(key.slice(0, 4)), `head survives for length ${key.length}`);
    assert.ok(shown.endsWith(key.slice(-4)), `tail survives for length ${key.length}`);
    assert.ok(shown.includes('…'), 'the elision is visible, never silent');
  }
});

test('a key short enough to show whole is never truncated', () => {
  // The ellipsis costs a character itself: hiding one char behind one
  // ellipsis shortens nothing and only obscures.
  for (const key of ['', 'ab', 'a'.repeat(8), 'a'.repeat(9)]) {
    assert.equal(fingerprintDisplay(key), key, `length ${key.length} fits as-is`);
  }
  assert.notEqual(fingerprintDisplay('a'.repeat(10)), 'a'.repeat(10), 'ten is past the budget');
});

const HOST: Host = {
  id: 'f'.repeat(64),
  label: 'studio',
  platform: 'macos',
  enrolledAt: 1_000,
  lastSeenAt: null,
};

test('a host card carries os, key and last seen from the record', () => {
  const card = hostCard(HOST, { localHostId: null, now: 2_000 });
  assert.equal(card.name, 'studio');
  assert.deepEqual(
    card.rows.map((r) => r.label),
    ['os', 'key', 'last seen'],
    'the §7 row order, minus what the registry does not carry',
  );
  const key = card.rows.find((r) => r.label === 'key');
  assert.equal(key?.value, fingerprintDisplay(HOST.id), 'the key row IS the enrolled public key');
  assert.equal(key?.mono, true, 'fingerprints render in the mono face');
  assert.equal(
    card.rows.find((r) => r.label === 'last seen')?.value,
    'never',
    'a never-seen host says so — the honest value, not a fabricated age',
  );
});

test('absent fields are omitted, never faked', () => {
  const bare = hostCard({ ...HOST, platform: '' }, { localHostId: null, now: 0 });
  assert.ok(
    !bare.rows.some((r) => r.label === 'os'),
    'a record without a platform gets no os row — not an empty one',
  );
  const card = hostCard(HOST, { localHostId: null, now: 0 });
  assert.ok(
    !card.rows.some((r) => r.label === 'sessions'),
    'no session count on the wire means no sessions row — 0 would claim knowledge',
  );
});

test('a session count appears only when a caller supplies one', () => {
  const card = hostCard(HOST, { localHostId: null, sessions: 3, now: 0 });
  assert.equal(card.rows.find((r) => r.label === 'sessions')?.value, '3');
});

test('only the identified local machine is marked local', () => {
  assert.equal(
    hostCard(HOST, { localHostId: HOST.id, now: 0 }).local,
    true,
    'the local card gets the accent border and the this-machine note',
  );
  assert.equal(hostCard(HOST, { localHostId: 'e'.repeat(64), now: 0 }).local, false);
  assert.equal(
    hostCard(HOST, { localHostId: null, now: 0 }).local,
    false,
    'unidentifiable (the hosted path today) marks nothing rather than guessing',
  );
});

test('ago is rough on purpose and pure over the given clock', () => {
  const now = 100 * 60_000;
  assert.equal(ago(null, now), 'never');
  assert.equal(ago(now - 30_000, now), 'just now');
  assert.equal(ago(now - 5 * 60_000, now), '5m ago');
  assert.equal(ago(now - 3 * 3_600_000, now), '3h ago');
  assert.equal(ago(now - 48 * 3_600_000, now), '2d ago');
});

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { ago, codeCountdown, fingerprintDisplay, hostCard, presenceOf } from '../src/fleet-model.ts';
import type { DirectoryStatus } from '../src/directory-source.ts';
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

test('codeCountdown walks the whole ten minutes and is pure over the given clock', () => {
  const minted = 5_000_000;
  const expiresAt = minted + 600_000; // the server's TTL
  assert.equal(codeCountdown(expiresAt, minted), '10:00', 'a just-minted code shows the full TTL');
  assert.equal(codeCountdown(expiresAt, minted + 13_000), '9:47', 'mid-life counts down');
  assert.equal(
    codeCountdown(expiresAt, minted + 599_000),
    '0:01',
    'seconds are zero-padded so the display never jitters between widths',
  );
});

test('codeCountdown says expired at the boundary and stays there', () => {
  // At `expiresAt` the server stops honouring the code, so the display must
  // not still be counting — and time past it is not a negative countdown.
  assert.equal(codeCountdown(1_000, 1_000), 'expired');
  assert.equal(codeCountdown(1_000, 1_001), 'expired');
  assert.equal(codeCountdown(1_000, 999_999), 'expired');
});

test('codeCountdown never shows 0:00 beside a code that still works', () => {
  // Rounds up, not down: `0:00` claims expiry, and a code with 400ms left is
  // still one the server accepts.
  assert.equal(codeCountdown(1_000, 600), '0:01');
  assert.equal(codeCountdown(61_000, 500), '1:01');
});

test('ago is rough on purpose and pure over the given clock', () => {
  const now = 100 * 60_000;
  assert.equal(ago(null, now), 'never');
  assert.equal(ago(now - 30_000, now), 'just now');
  assert.equal(ago(now - 5 * 60_000, now), '5m ago');
  assert.equal(ago(now - 3 * 3_600_000, now), '3h ago');
  assert.equal(ago(now - 48 * 3_600_000, now), '2d ago');
});

/** A `ready` status carrying `n` sessions, and nothing else a card reads. */
function ready(n: number): DirectoryStatus {
  return {
    kind: 'ready',
    view: {
      sessions: Array.from({ length: n }, (_, i) => ({ session: String(i) })),
    },
  } as unknown as DirectoryStatus;
}

test('a machine nobody asked about says so, rather than claiming to be asleep', () => {
  // The distinction the whole type exists for: "not asked" and "asked, no
  // answer" look identical on a boolean and mean opposite things. The local
  // path and a deployment with no relay both land here.
  const p = presenceOf(undefined);
  assert.equal(p.kind, 'unknown');
  assert.equal(p.reachable, false);
  assert.equal(p.text, '', 'an unknown machine has nothing to say, not something reassuring');
});

test('asleep is its own state and is not a fault', () => {
  const p = presenceOf({ kind: 'offline' });
  assert.equal(
    p.kind,
    'asleep',
    'over the relay most of a fleet is asleep most of the time; painting that as degraded is how a screen stops being read',
  );
  assert.notEqual(p.kind, 'degraded');
  assert.equal(p.reachable, false, 'a sleeping machine leads nowhere, so its card must not');
});

test('pairing shows the code, because the code IS the instruction', () => {
  const p = presenceOf({ kind: 'pairing', code: '481920' });
  assert.ok(
    p.text.includes('481920'),
    'the code has to be compared against what the machine is showing; without it this is a wait with no way out',
  );
  assert.equal(p.reachable, false);
});

test('an error says what went wrong rather than a fixed sentence', () => {
  const p = presenceOf({ kind: 'error', message: 'the relay refused this browser' });
  assert.ok(p.text.includes('refused'), 'the two worlds fail differently and the status knows how');
  assert.equal(p.kind, 'degraded');
});

test('only a machine that answered is reachable', () => {
  assert.equal(presenceOf(ready(0)).reachable, true);
  for (const s of [
    { kind: 'pending' } as const,
    { kind: 'offline' } as const,
    { kind: 'error', message: 'x' } as const,
    { kind: 'pairing', code: '1' } as const,
  ]) {
    assert.equal(presenceOf(s).reachable, false, `${s.kind} must not offer a way in`);
  }
});

test('the session count is pluralised and comes off the live view', () => {
  assert.equal(presenceOf(ready(1)).text, 'online · 1 session');
  assert.equal(presenceOf(ready(3)).text, 'online · 3 sessions');
  assert.equal(presenceOf(ready(0)).text, 'online · 0 sessions');
});

test('a live count outranks a supplied one, and the card cannot contradict its dot', () => {
  // Same fact from two places: the registry hint and the socket. A card
  // reading `online · 3 sessions` beside a row saying `sessions 7` is worse
  // than either number alone.
  const card = hostCard(HOST, { localHostId: null, sessions: 7, now: 0, status: ready(3) });
  assert.equal(card.rows.find((r) => r.label === 'sessions')?.value, '3');
  assert.ok(card.presence.text.includes('3 sessions'));
});

test('a deployment with no relay says nothing, rather than accusing every machine', () => {
  // The bug this catches, found in review: driving the live directory with no
  // relay configured puts every host into `failed` with a NO_RELAY message —
  // one deployment-level fact, repeated once per machine as though each had
  // been asked and each had gone wrong. The fleet passes `undefined` instead.
  const card = hostCard(HOST, { localHostId: null, now: 0, status: undefined });
  assert.equal(card.presence.kind, 'unknown');
  assert.equal(card.presence.text, '');
  assert.ok(
    !card.rows.some((r) => r.label === 'sessions'),
    'no relay means no session count either — 0 would claim knowledge',
  );
});

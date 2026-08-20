import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  ago,
  browserLabel,
  codeCountdown,
  copyOutcome,
  deviceRow,
  deviceVouchAction,
  eventLine,
  fingerprintDisplay,
  hostCard,
  mintPanelOnStart,
  ownDeviceAction,
  ownDeviceApproved,
  partitionRevoked,
  presenceOf,
} from '../src/fleet-model.ts';
import type { DirectoryStatus } from '../src/directory-source.ts';
import type { Device, Host } from '../src/registry.ts';

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
  revokedAt: null,
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

test('starting a mint for the other kind clears the panel at once', () => {
  // The stale panel is the bug, found in review: a host code left on screen
  // while a device mint is in flight is a code someone copies into the wrong
  // sign-in, under a panel claiming a mint it did not start.
  const host = { kind: 'host' as const, code: 'ZK7M2Q9T', expiresAt: 1_000 };
  assert.equal(mintPanelOnStart(host, 'device'), null);
});

test('a remint for the same kind keeps its panel up', () => {
  // The visible code is still the server's while the new one is minted, and a
  // panel that blinks away mid-remint reads as the mint having failed.
  const host = { kind: 'host' as const, code: 'ZK7M2Q9T', expiresAt: 1_000 };
  assert.equal(mintPanelOnStart(host, 'host'), host);
  assert.equal(mintPanelOnStart(null, 'host'), null, 'no panel stays no panel');
});

test('every way a clipboard write fails is one resolved copy failed', async () => {
  // On plain http `navigator.clipboard` is undefined — not a clipboard that
  // rejects — so the unguarded call throws SYNCHRONOUSLY and a `.catch` on
  // its result never runs. The absent API, a sync throw and an ordinary
  // rejection must all land in the same place, or the click handler errors
  // instead of the button saying the copy didn't take.
  assert.equal(await copyOutcome(undefined, 'x'), 'copy failed', 'insecure origin: no API at all');
  assert.equal(
    await copyOutcome(
      {
        writeText: () => {
          throw new Error('sync');
        },
      },
      'x',
    ),
    'copy failed',
    'a synchronous throw resolves rather than escaping the handler',
  );
  assert.equal(
    await copyOutcome({ writeText: () => Promise.reject(new Error('denied')) }, 'x'),
    'copy failed',
    'denied permission rejects, and is folded too',
  );
});

test('a copy that took says copied, with exactly the minted text', async () => {
  // Verbatim matters: the enrolment signature covers the code bytes, so what
  // lands on the clipboard must be what the server minted, separators and all.
  let wrote = '';
  const clipboard = {
    writeText: (t: string) => {
      wrote = t;
      return Promise.resolve();
    },
  };
  assert.equal(await copyOutcome(clipboard, 'zest-daemon --enroll ZK7M2Q9T'), 'copied');
  assert.equal(wrote, 'zest-daemon --enroll ZK7M2Q9T');
});

test('ownDeviceAction: an unlisted key registers, a pending own row banners, approved is quiet', () => {
  const own = 'c'.repeat(64);
  const device = (id: string, status: 'pending' | 'approved'): Device => ({
    id,
    label: 'x',
    kind: 'browser',
    extractable: true,
    status,
    enrolledAt: 1,
    lastSeenAt: null,
    revokedAt: null,
  });

  assert.equal(ownDeviceAction([], own, false), 'register', 'the account has never seen this key');
  assert.equal(
    ownDeviceAction([device('d'.repeat(64), 'approved')], own, false),
    'register',
    'somebody else’s row is not this browser’s',
  );
  assert.equal(
    ownDeviceAction([device(own, 'pending')], own, false),
    'awaiting-approval',
    'the owner learns their browser is waiting only if the screen says so',
  );
  assert.equal(
    ownDeviceAction([device(own, 'approved')], own, false),
    'nothing',
    'an approved browser has nothing to register and nothing to announce',
  );
});

test('an ephemeral key is never auto-registered', () => {
  // An ephemeral identity is minted when the key store cannot be read and is
  // deliberately not persisted — registering it would enrol a pending row per
  // visit, none of them this browser by the time anyone approves it.
  assert.equal(ownDeviceAction([], 'e'.repeat(64), true), 'nothing');
});

test('browserLabel names the product, in compatibility-archaeology order', () => {
  // Everything claims Chrome/ and almost everything claims Safari/, so the
  // more specific token must win first or every browser is called Chrome.
  const cases: Array<[string, string]> = [
    ['Mozilla/5.0 (X11; Linux) Gecko/20100101 Firefox/128.0', 'Firefox'],
    ['Mozilla/5.0 AppleWebKit/537.36 Chrome/126.0.0.0 Safari/537.36 Edg/126.0.2', 'Edge'],
    ['Mozilla/5.0 AppleWebKit/537.36 Chrome/126.0.0.0 Safari/537.36 OPR/112.0', 'Opera'],
    ['Mozilla/5.0 AppleWebKit/537.36 Chrome/126.0.0.0 Safari/537.36', 'Chrome'],
    ['Mozilla/5.0 AppleWebKit/605.1.15 Version/17.5 Safari/605.1.15', 'Safari'],
    ['curl/8.6.0', 'browser'],
    ['', 'browser'],
  ];
  for (const [ua, want] of cases) {
    assert.equal(browserLabel(ua), want, ua || '(empty)');
  }
});

test('deviceRow keeps the pending marker and the key warning out of the meta string', () => {
  // The component styles the two flags; folded into `meta` they would render
  // in one colour and stop reading as either.
  const row = deviceRow(
    {
      id: 'f'.repeat(64),
      label: 'Firefox',
      kind: 'browser',
      extractable: true,
      status: 'pending',
      enrolledAt: 1,
      lastSeenAt: 1_000,
      revokedAt: null,
    },
    61_000 + 1_000,
  );
  assert.equal(row.name, 'Firefox');
  assert.equal(row.meta, 'browser · last seen 1m ago');
  assert.equal(row.pending, true, 'a pending row must be distinguishable in the list');
  assert.equal(row.keyReadable, true, 'the seed warning survives the row shaping');

  const approved = deviceRow(
    {
      id: 'f'.repeat(64),
      label: 'Firefox',
      kind: 'browser',
      extractable: false,
      status: 'approved',
      enrolledAt: 1,
      lastSeenAt: null,
      revokedAt: null,
    },
    2,
  );
  assert.equal(approved.pending, false);
  assert.equal(approved.keyReadable, false);
  assert.equal(approved.meta, 'browser · last seen never');
});

test('only an approved, non-ephemeral own key may vouch', () => {
  const own = 'c'.repeat(64);
  const device = (id: string, status: 'pending' | 'approved'): Device => ({
    id,
    label: 'x',
    kind: 'browser',
    extractable: true,
    status,
    enrolledAt: 1,
    lastSeenAt: null,
    revokedAt: null,
  });

  assert.equal(ownDeviceApproved([device(own, 'approved')], own, false), true);
  assert.equal(
    ownDeviceApproved([device(own, 'pending')], own, false),
    false,
    'a pending key offering to vouch would offer a guaranteed server refusal',
  );
  assert.equal(ownDeviceApproved([], own, false), false, 'an unlisted key cannot vouch');
  assert.equal(
    ownDeviceApproved([device(own, 'approved')], own, true),
    false,
    'an ephemeral key must not vouch: whatever it signs names a key gone next load',
  );
});

test('deviceVouchAction offers approve on pending rows, vouch on approved ones, never on self', () => {
  const own = 'c'.repeat(64);
  const other = 'd'.repeat(64);
  const device = (id: string, status: 'pending' | 'approved'): Device => ({
    id,
    label: 'x',
    kind: 'browser',
    extractable: true,
    status,
    enrolledAt: 1,
    lastSeenAt: null,
    revokedAt: null,
  });

  assert.equal(deviceVouchAction(device(other, 'pending'), own, true), 'approve');
  assert.equal(
    deviceVouchAction(device(other, 'approved'), own, true),
    'vouch',
    'attestation, not status, is what spares pairing prompts — and the browser cannot ' +
      'see which approved devices already carry one, so every approved row gets the offer',
  );
  assert.equal(
    deviceVouchAction(device(own, 'pending'), own, true),
    null,
    'self-vouching is refused by the Worker, so the button would be a guaranteed error',
  );
  assert.equal(deviceVouchAction(device(own, 'approved'), own, true), null);
  assert.equal(
    deviceVouchAction(device(other, 'pending'), own, false),
    null,
    'a browser that may not sign gets no signing buttons at all',
  );
});

test('the remove button says deny on a pending row and revoke on an approved one', () => {
  // Both run the same revoke underneath; the word is for the person — a
  // pending request is *denied*, granted trust is *revoked*.
  const base: Device = {
    id: 'f'.repeat(64),
    label: 'x',
    kind: 'browser',
    extractable: false,
    status: 'pending',
    enrolledAt: 1,
    lastSeenAt: null,
    revokedAt: null,
  };
  assert.equal(deviceRow(base, 2).removeLabel, 'deny');
  assert.equal(deviceRow({ ...base, status: 'approved' }, 2).removeLabel, 'revoke');
});

test('an event line says what happened, to what, on whose authority, and when', () => {
  // The incident the Activity section exists for was a revoke nobody
  // remembered making — the authority is the load-bearing part of the line.
  const at = 1_000;
  const now = at + 3 * 24 * 60 * 60_000;
  const line = eventLine(
    { action: 'revoke', actor: 'owner', subjectLabel: 'ANDII-ALIEN01', at },
    now,
  );
  assert.equal(line, "revoked ANDII-ALIEN01 · from this account's browser · 3d ago");

  assert.ok(
    eventLine({ action: 'enroll', actor: 'machine', subjectLabel: 'andy-mac', at }, now).includes(
      'by the machine itself',
    ),
    'a claim is the key proving possession of itself, and the line says so',
  );
});

test('partitionRevoked keeps live rows launchable and revoked rows out of every live path', () => {
  // The recovery view rides the same listing, so the split is one function
  // both sections read — a revoked host that leaked into the live list would
  // be watched, dialled and offered for launch, all against a row the ticket
  // route refuses.
  const live = { id: 'a'.repeat(64), revokedAt: null };
  const gone = { id: 'b'.repeat(64), revokedAt: 5 };
  const split = partitionRevoked([live, gone]);
  assert.deepEqual(split.live, [live]);
  assert.deepEqual(split.revoked, [gone]);

  const none = partitionRevoked([live]);
  assert.deepEqual(none.revoked, [], 'no revoked rows is the ordinary case, not a special one');
});

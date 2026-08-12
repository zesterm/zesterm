/**
 * The relay's half of the attach ticket: real Ed25519, and every way in.
 *
 * The mint lives in the *other* Worker and this package does not import it —
 * two Workers, two deploy cadences, and a test that reached across would be the
 * first thing to make them one. What both sides do share is
 * `@zesterm/cloud-shared`'s byte layout, so the tickets here are signed over
 * `attachTicketPreimage` exactly as the mint signs them, with a real key.
 *
 * The refusals are the point. A verifier that admits everything passes a
 * happy-path test, so every case below is a ticket that is *almost* right:
 * one byte, one field, one key, one second.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { getPublicKeyAsync, signAsync } from '@noble/ed25519';
import {
  ATTACH_TICKET_TTL_MS,
  attachTicketPayload,
  attachTicketPreimage,
  encodeAttachTicket,
  hex,
  utf8,
  type AttachTicket,
} from '@zesterm/cloud-shared';

import { attachVerdict, roomRequest } from '../src/index.ts';
import type { Env } from '../src/env.ts';
import { ATTACH_JTI_PARAM, ATTACH_PATH, attachJti } from '../src/routes.ts';
import {
  CLOSE_TICKET_REFUSED,
  RELAY_SUBPROTOCOL,
  ticketFromSubprotocols,
  ticketPublicKeys,
  verifyAttachTicket,
} from '../src/ticket.ts';

const HOST = 'ab'.repeat(32);
const OTHER_HOST = 'cd'.repeat(32);
const IAT = 1_700_000_000_000;
const EXP = IAT + ATTACH_TICKET_TTL_MS;

/** A deterministic signing key, so a failure names the same public key every run. */
async function keyPair(seed: number): Promise<{ seed: Uint8Array; publicHex: string }> {
  const bytes = new Uint8Array(32).fill(seed);
  return { seed: bytes, publicHex: hex(await getPublicKeyAsync(bytes)) };
}

const ACCOUNT = await keyPair(7);
const STRANGER = await keyPair(9);

function claims(overrides: Partial<AttachTicket> = {}): AttachTicket {
  return {
    v: 1,
    jti: 'aWQtMDAwMDAwMDE',
    aud: 'relay',
    host: HOST,
    user: 'gh_42',
    dev: 'de'.repeat(32),
    iat: IAT,
    exp: EXP,
    ...overrides,
  };
}

/** What the mint produces: the canonical payload, signed. */
async function mint(overrides: Partial<AttachTicket> = {}, seed = ACCOUNT.seed): Promise<string> {
  const payload = attachTicketPayload(claims(overrides));
  return encodeAttachTicket(payload, await signAsync(attachTicketPreimage(payload), seed));
}

/**
 * A ticket over claims the canonical writer would refuse to spell — a wrong
 * `aud`, a version from the future. Written straight to JSON because the point
 * is to prove the *verifier* refuses them, and a typed mint cannot produce them.
 */
async function mintRaw(raw: Record<string, unknown>, seed = ACCOUNT.seed): Promise<string> {
  const payload = utf8(JSON.stringify(raw));
  return encodeAttachTicket(payload, await signAsync(attachTicketPreimage(payload), seed));
}

const verify = (text: string, keys: string | undefined, now = IAT, host = HOST) =>
  verifyAttachTicket({ text, host, keys: ticketPublicKeys(keys), now });

test('a ticket the account service signed admits its holder to that host, and nothing else does', async () => {
  const admitted = await verify(await mint(), ACCOUNT.publicHex);
  assert.deepEqual(
    admitted,
    claims(),
    'a real signature over the shared preimage must verify, or the two Workers agree about bytes and not about keys',
  );

  assert.equal(
    await verify(await mint({}, STRANGER.seed), ACCOUNT.publicHex),
    null,
    'a key the relay does not hold must not admit anyone — this is the whole authorization decision',
  );
});

test('a ticket is refused the instant it expires, and is good until then', async () => {
  const ticket = await mint();

  assert.ok(
    await verify(ticket, ACCOUNT.publicHex, EXP - 1),
    'one millisecond inside the window is inside it',
  );
  assert.equal(
    await verify(ticket, ACCOUNT.publicHex, EXP),
    null,
    '`exp` is the first instant the ticket is dead, not the last it is alive',
  );
  assert.equal(
    await verify(ticket, ACCOUNT.publicHex, EXP + 60_000),
    null,
    'a ticket captured from an edge log must be worthless by the time anyone reads it',
  );
});

test('a ticket minted for later cannot be spent now', async () => {
  // No clock-skew allowance, and none wanted: the mint and the relay run on
  // Cloudflare's clock. An `iat` tolerance is a window in which a ticket minted
  // for the future is already spendable.
  assert.equal(
    await verify(await mint(), ACCOUNT.publicHex, IAT - 1),
    null,
    'a ticket whose issue time has not arrived is not yet a ticket',
  );
});

test('the room in the URL and the room in the signature must be the same room', async () => {
  const ticket = await mint({ host: HOST });

  assert.equal(
    await verify(ticket, ACCOUNT.publicHex, IAT, OTHER_HOST),
    null,
    'a valid ticket for one machine must not open a pipe to another',
  );
  assert.equal(
    await verify(ticket, ACCOUNT.publicHex, IAT, ''),
    null,
    'a missing ?host= is not a wildcard',
  );
  assert.equal(
    await verify(ticket, ACCOUNT.publicHex, IAT, HOST.toUpperCase()),
    null,
    'host ids are stored as hex() writes them, so an uppercase spelling is a different machine — matching case-insensitively here would admit a holder to a room the mint never named',
  );
});

test('a ticket minted for some other audience is not an attach', async () => {
  // `aud` exists so a ticket the account service signs for a future service
  // — a directory, a metrics endpoint — cannot be spent as admission to a room.
  const ticket = await mintRaw({
    v: 1,
    jti: 'aWQtMDAwMDAwMDE',
    aud: 'directory',
    host: HOST,
    user: 'gh_42',
    dev: 'de'.repeat(32),
    iat: IAT,
    exp: EXP,
  });
  assert.equal(await verify(ticket, ACCOUNT.publicHex), null, 'the audience is checked, not assumed');
});

test('tampering with either half is refused, including tampering that keeps the JSON valid', async () => {
  const original = attachTicketPayload(claims());
  const signature = await signAsync(attachTicketPreimage(original), ACCOUNT.seed);

  // The interesting forgery: a payload that parses, narrows, and names a
  // different account, carried under the real signature.
  const swapped = attachTicketPayload(claims({ user: 'gh_intruder' }));
  assert.equal(
    await verify(encodeAttachTicket(swapped, signature), ACCOUNT.publicHex),
    null,
    'the signature covers the payload bytes, so a re-written claim must not travel under it',
  );

  const bitFlipped = Uint8Array.from(signature, (b, i) => (i === 0 ? b ^ 0x01 : b));
  assert.equal(
    await verify(encodeAttachTicket(original, bitFlipped), ACCOUNT.publicHex),
    null,
    'one bit of the signature is enough',
  );

  const truncated = signature.slice(0, 63);
  assert.equal(
    await verify(encodeAttachTicket(original, truncated), ACCOUNT.publicHex),
    null,
    'a short signature is an error, never a pad',
  );
});

test('a padded ticket is refused here too, so nothing can mint one and get away with it', async () => {
  // Unpadded base64url is load-bearing: `Sec-WebSocket-Protocol` values are
  // HTTP tokens, and `=` and `/` are not token characters. A padded ticket is a
  // malformed header that works under `wrangler dev` and is dropped by a CDN.
  const ticket = await mint();
  const bad: Array<[string, string]> = [
    [`${ticket}=`, 'one character of padding on the signature'],
    [`${ticket}==`, 'two'],
    [ticket.replace('.', '/.'), 'a standard-base64 `/` in the payload'],
    [ticket.replace('.', '+.'), 'a standard-base64 `+` in the payload'],
  ];
  for (const [text, why] of bad) {
    assert.equal(await verify(text, ACCOUNT.publicHex), null, why);
  }
});

test('a small-order key in the list admits nobody', async () => {
  // The all-zero encoding is a valid point and verifies almost anything under
  // ZIP-215 semantics, which is noble's default. `verifyAsync` is called with
  // `zip215: false` for exactly this: one such key reaching TICKET_PUBLIC_KEYS
  // by accident would otherwise open every room to everyone.
  const smallOrder = '00'.repeat(32);
  assert.equal(
    await verifyAttachTicket({
      text: encodeAttachTicket(attachTicketPayload(claims()), new Uint8Array(64)),
      host: HOST,
      keys: ticketPublicKeys(smallOrder),
      now: IAT,
    }),
    null,
    'a key that answers for signatures it never made must not be able to admit anyone',
  );
});

test('the key list is what makes rotation possible without an atomic deploy', async () => {
  const ticket = await mint({}, STRANGER.seed);

  // The new key added beside the old one, before the mint switches to it.
  assert.ok(
    await verify(ticket, `${ACCOUNT.publicHex},${STRANGER.publicHex}`, IAT),
    'a ticket signed by any key in the list is admitted, or rotation means deploying two Workers at the same instant',
  );

  // Whitespace, because a comma-separated list a person edits acquires it.
  assert.ok(
    await verify(ticket, ` ${ACCOUNT.publicHex} , ${STRANGER.publicHex} `, IAT),
    'a rotation list is hand-edited, and a space must not cost a key',
  );

  assert.deepEqual(
    ticketPublicKeys(`not-hex,${ACCOUNT.publicHex},${ACCOUNT.publicHex.toUpperCase()}`).length,
    1,
    'a typo in one entry costs that key rather than every attach in the fleet, and uppercase is a typo',
  );
  assert.deepEqual(ticketPublicKeys(undefined), [], 'an unconfigured relay holds no keys');
  assert.equal(
    await verify(await mint(), undefined),
    null,
    'a relay with no keys configured refuses every attach, which is the correct answer and an obvious one',
  );
});

test('the ticket is found by name in the offer list, not by position', async () => {
  const header = (...offers: string[]) => offers.join(', ');

  assert.equal(ticketFromSubprotocols(header(RELAY_SUBPROTOCOL, 'ticket.abc')), 'abc');
  assert.equal(
    ticketFromSubprotocols(header('ticket.abc', RELAY_SUBPROTOCOL)),
    'abc',
    'an offer list is the client’s preference order; a verifier that assumed position would break the day anything inserts one',
  );
  assert.equal(
    ticketFromSubprotocols(header(RELAY_SUBPROTOCOL, '  ticket.abc  ')),
    'abc',
    'the header is comma-space separated and the spaces are not part of the value',
  );
  assert.equal(ticketFromSubprotocols(header(RELAY_SUBPROTOCOL)), null, 'no ticket offered');
  assert.equal(ticketFromSubprotocols(null), null, 'no header at all');
});

/**
 * Bindings that throw.
 *
 * Not a convenience: ADR-009's cost model rests on a refused attach costing no
 * room wake-up and no query, so an implementation that reached for either would
 * fail here rather than merely be slower in production.
 */
function env(keys?: string): Env {
  const forbidden = (what: string) => (): never => {
    throw new Error(`the attach path must not ${what} — a refusal must cost no wake-up`);
  };
  return {
    RELAY_ROOM: { idFromName: forbidden('name a room'), get: forbidden('get a room stub') },
    DB: { prepare: forbidden('query D1') },
    ...(keys === undefined ? {} : { TICKET_PUBLIC_KEYS: keys }),
  };
}

const attach = (ticket: string | null, host = HOST, upgrade = true) =>
  new Request(`https://relay.example.com${ATTACH_PATH}?host=${host}`, {
    headers: {
      ...(upgrade ? { upgrade: 'websocket' } : {}),
      ...(ticket === null ? {} : { 'sec-websocket-protocol': `${RELAY_SUBPROTOCOL}, ticket.${ticket}` }),
    },
  });

test('a verified attach is the only thing that may name a room', async () => {
  assert.deepEqual(
    await attachVerdict(attach(await mint()), env(ACCOUNT.publicHex), IAT),
    { kind: 'room', host: HOST, jti: claims().jti },
    'the ticket is what the edge checks; whether the daemon is connected is the room’s to answer, and the room is not woken until the ticket has verified',
  );
});

test('the verified jti reaches the room, on a request that is still an upgrade', async () => {
  const original = attach(await mint());
  const forwarded = roomRequest(original, claims().jti);

  assert.equal(
    attachJti(new URL(forwarded.url)),
    claims().jti,
    'written by the edge and read by the room, and this is the only assertion that puts the two halves together: `fetch` is untestable on both sides, so two ends naming different parameters would refuse every attach in the fleet with a green suite',
  );
  assert.equal(
    new URL(forwarded.url).searchParams.get('host'),
    HOST,
    'and `?host=` survives, which is the only thing binding the object’s name to a machine',
  );
  assert.equal(
    forwarded.headers.get('sec-websocket-protocol'),
    null,
    'the ticket rides in that header, and every comment around `roomRequest` says the room never sees it — which was untrue while it rode along. The edge has already checked the signature, the audience, the expiry and the room, and forwards the one field it decided; a bearer credential good for thirty seconds against a live shell has no business reaching a component with no use for it, where something can log it',
  );
  assert.equal(
    forwarded.headers.get('upgrade'),
    'websocket',
    'the upgrade is the point of the request, and the room refuses anything that is not one. (Node’s `Request` keeps the header; workerd’s is not exercised here, and only a deploy proves that half)',
  );
  assert.equal(
    new URL(original.url).searchParams.get(ATTACH_JTI_PARAM),
    null,
    'and the caller’s own request is untouched, so nothing downstream reads a URL this Worker rewrote under it',
  );

  assert.equal(
    attachJti(new URL(roomRequest(attach(await mint(), `${HOST}&jti=mine`), 'ours').url)),
    'ours',
    '`set` replaces rather than appends: `searchParams.get` returns the first, so an appended id would let the caller choose which ticket the room spends',
  );
});

test('every refusal is a close code, because a browser cannot read an HTTP status', async () => {
  const refused = { kind: 'close', code: CLOSE_TICKET_REFUSED } as const;
  const cases: Array<[Request, string]> = [
    [attach(null), 'no ticket offered'],
    [attach('not-a-ticket'), 'a ticket that does not decode'],
    [attach(await mint({}, STRANGER.seed)), 'a key the relay does not hold'],
    [attach(await mint(), OTHER_HOST), 'a ticket for another room'],
  ];
  for (const [request, why] of cases) {
    const verdict = await attachVerdict(request, env(ACCOUNT.publicHex), IAT);
    assert.equal(verdict.kind, 'close', why);
    assert.equal(
      verdict.kind === 'close' ? verdict.code : 0,
      refused.code,
      `${why}: one code for every refusal, because telling them apart is worth more to whoever is guessing`,
    );
  }
});

test('a request that is not an upgrade gets a status, because whoever sent it can read one', async () => {
  assert.deepEqual(
    await attachVerdict(attach(await mint(), HOST, false), env(ACCOUNT.publicHex), IAT),
    { kind: 'http', status: 426, body: 'expected a WebSocket upgrade' },
    'a person or a probe is not a browser WebSocket, and 426 is the only thing either can read',
  );
});

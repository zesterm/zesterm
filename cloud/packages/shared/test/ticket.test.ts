/**
 * The attach ticket's encoding, pinned.
 *
 * Two Workers sign and verify these bytes and they share no code path at
 * runtime, so what is asserted here is the format itself: the exact preimage
 * for a fixed payload, the alphabet the encoding is allowed to use, and every
 * shape the decoder must refuse. A disagreement about any of them surfaces at
 * bring-up as "the ticket is invalid", which names nothing.
 *
 * Whether a *real* signature over these bytes verifies is proved a layer up,
 * where the Ed25519 is: `packages/web/test/relay.test.ts` checks that what the
 * mint produces verifies under the seed's public key, and
 * `packages/relay/test/ticket.test.ts` checks that a real signature over this
 * preimage is admitted and every tampering with it is not. Neither package
 * imports the other — the guarantee that the two agree is that the bytes are
 * built here, once.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  ATTACH_TICKET_SIGNATURE_LEN,
  ATTACH_TICKET_TTL_MS,
  attachTicketPayload,
  attachTicketPreimage,
  decodeAttachTicket,
  encodeAttachTicket,
  hex,
  MAX_ATTACH_TICKET_CHARS,
  utf8,
  type AttachTicket,
} from '../src/index.ts';

const HOST = 'ab'.repeat(32);
const DEV = 'cd'.repeat(32);

const TICKET: AttachTicket = {
  v: 1,
  jti: 'aWQtMDAwMDAwMDE',
  aud: 'relay',
  host: HOST,
  user: 'gh_42',
  dev: DEV,
  iat: 1_700_000_000_000,
  exp: 1_700_000_030_000,
};

/** The payload's canonical JSON, spelled out so a reordered field is visible. */
const CANONICAL_JSON =
  `{"v":1,"jti":"aWQtMDAwMDAwMDE","aud":"relay","host":"${HOST}",` +
  `"user":"gh_42","dev":"${DEV}","iat":1700000000000,"exp":1700000030000}`;

/**
 * The exact bytes the account service's key signs for `TICKET`.
 *
 * A hex literal, because that is the artifact a second implementation is
 * transcribed against — the assertion below it says *which byte* moved when
 * this stops matching, but only this one is copyable into another language.
 */
const GOLDEN_PREIMAGE_HEX =
  '7a65737465726d2d7369672d76310072656c6179006174746163682d7469636b6574007b2276223a312c226a' +
  '7469223a22615751744d4441774d4441774d4445222c22617564223a2272656c6179222c22686f7374223a22' +
  '6162616261626162616261626162616261626162616261626162616261626162616261626162616261626162' +
  '6162616261626162616261626162616261626162222c2275736572223a2267685f3432222c22646576223a22' +
  '6364636463646364636463646364636463646364636463646364636463646364636463646364636463646364' +
  '6364636463646364636463646364636463646364222c22696174223a313730303030303030303030302c2265' +
  '7870223a313730303030303033303030307d';

test('the preimage is the signing domain, the relay role, the purpose, then the payload', () => {
  const payload = attachTicketPayload(TICKET);

  assert.equal(
    hex(attachTicketPreimage(payload)),
    GOLDEN_PREIMAGE_HEX,
    'a byte of difference here refuses every ticket the account service mints, and the error names neither side',
  );
  assert.deepEqual(
    attachTicketPreimage(payload),
    utf8(`zesterm-sig-v1\0relay\0attach-ticket\0${CANONICAL_JSON}`),
    'the golden above is copyable; this is the one that says which byte moved',
  );
});

test('the payload is written field by field, not spread from its argument', () => {
  // A mint built from a larger record must not be able to change the bytes or
  // ship a field that record happened to carry -- both would be signed.
  const extra = { ...TICKET, secret: 'do not ship me', v: 1 } as AttachTicket;
  assert.equal(
    new TextDecoder().decode(attachTicketPayload(extra)),
    CANONICAL_JSON,
    'an unknown field on the argument must not reach the signed bytes',
  );
});

test('a ticket is base64url with no padding, which is what makes it a legal header value', () => {
  // `Sec-WebSocket-Protocol` values are HTTP tokens: `/`, `+` and `=` are not
  // token characters, so a ticket carrying any of them is a malformed header
  // that some intermediaries pass and others reject -- it would work under
  // `wrangler dev` and fail through a CDN.
  for (let seed = 0; seed < 64; seed++) {
    const payload = attachTicketPayload({ ...TICKET, jti: 'x'.repeat(seed) });
    const signature = new Uint8Array(ATTACH_TICKET_SIGNATURE_LEN).fill(seed);
    const encoded = encodeAttachTicket(payload, signature);
    assert.match(
      encoded,
      /^[A-Za-z0-9_-]+\.[A-Za-z0-9_-]+$/,
      `jti of ${seed} characters encoded outside the base64url alphabet: ${encoded}`,
    );
  }
});

test('a ticket decodes to the bytes that arrived, not to a re-serialization', () => {
  // The signature covers those exact bytes. A verifier that re-encoded the
  // parsed object would depend on two JSON writers agreeing about key order,
  // number formatting and escaping.
  const payload = attachTicketPayload(TICKET);
  const signature = new Uint8Array(ATTACH_TICKET_SIGNATURE_LEN).fill(7);

  const decoded = decodeAttachTicket(encodeAttachTicket(payload, signature));
  assert.ok(decoded, 'a ticket this file encoded must decode');
  assert.deepEqual(decoded.payload, payload, 'the payload bytes must survive the round trip verbatim');
  assert.deepEqual(decoded.signature, signature);
  assert.deepEqual(decoded.ticket, TICKET, 'and the claims must narrow back to what was minted');
});

test('a padded ticket is refused rather than mangled', () => {
  // The reason unpadded is load-bearing, asserted from the other side: if a
  // padded ticket were accepted here, nothing would stop one being minted, and
  // it would then be a header value some proxy in the path silently drops.
  const payload = attachTicketPayload(TICKET);
  const signature = new Uint8Array(ATTACH_TICKET_SIGNATURE_LEN).fill(1);
  const encoded = encodeAttachTicket(payload, signature);

  const [head = '', tail = ''] = encoded.split('.');
  assert.equal(decodeAttachTicket(`${head}=.${tail}`), null, 'padding on the payload');
  assert.equal(decodeAttachTicket(`${head}.${tail}==`), null, 'padding on the signature');
  // The two characters standard base64 spends that base64url does not, and the
  // two an HTTP token may not contain either.
  for (const c of ['+', '/']) {
    assert.equal(decodeAttachTicket(`${head}${c}.${tail}`), null, `${c} in the payload`);
    assert.equal(decodeAttachTicket(`${head}.${tail}${c}`), null, `${c} in the signature`);
  }
});

test('every malformed shape is one refusal, and none of them throws', () => {
  const payload = attachTicketPayload(TICKET);
  const signature = new Uint8Array(ATTACH_TICKET_SIGNATURE_LEN).fill(2);
  const encoded = encodeAttachTicket(payload, signature);
  const [head = '', tail = ''] = encoded.split('.');

  const bad: Array<[string, string]> = [
    ['', 'the empty string'],
    [head, 'no separator at all'],
    [`${head}.${tail}.${tail}`, 'two separators leave the halves ambiguous'],
    [`.${tail}`, 'an empty payload'],
    [`${head}.`, 'an empty signature'],
    [encodeAttachTicket(payload, signature.slice(1)), 'a 63-byte signature is an error, never a pad'],
    ['a'.repeat(MAX_ATTACH_TICKET_CHARS + 1), 'past the length bound, refused unread'],
  ];
  for (const [text, why] of bad) {
    assert.equal(decodeAttachTicket(text), null, why);
  }
});

test('a claim of the wrong type or the wrong shape never becomes a decision', () => {
  // This is the one place bytes off the network become claims an authorization
  // decision is made from. An `exp` that is a string compares `>` against a
  // number in ways nobody intends.
  const signature = new Uint8Array(ATTACH_TICKET_SIGNATURE_LEN).fill(3);
  const encode = (claims: unknown) =>
    encodeAttachTicket(utf8(JSON.stringify(claims)), signature);

  const bad: Array<[unknown, string]> = [
    [{ ...TICKET, v: 2 }, 'a version this build does not mint'],
    [{ ...TICKET, v: '1' }, 'a version as a string'],
    [{ ...TICKET, aud: 'directory' }, 'a ticket minted for another audience'],
    [{ ...TICKET, host: HOST.toUpperCase() }, 'uppercase hex is a different primary key, not the same machine'],
    [{ ...TICKET, host: `${HOST}ff` }, 'a host id that is not 32 bytes'],
    [{ ...TICKET, host: 'zz'.repeat(32) }, 'a host id that is not hex'],
    [{ ...TICKET, exp: '1700000030000' }, 'an expiry as a string'],
    [{ ...TICKET, exp: 1.5 }, 'an expiry that is not a whole millisecond'],
    [{ ...TICKET, iat: Number.MAX_VALUE }, 'an issue time past what a number represents exactly'],
    [{ ...TICKET, jti: '' }, 'an empty replay key'],
    [{ ...TICKET, user: '' }, 'an empty user'],
    [{ ...TICKET, dev: '' }, 'an empty session'],
    [[TICKET], 'an array of claims'],
    ['a ticket', 'a bare string'],
    [null, 'null'],
  ];
  for (const [claims, why] of bad) {
    assert.equal(decodeAttachTicket(encode(claims)), null, why);
  }
});

test('the lifetime is short enough that a captured ticket is worthless before it is read', () => {
  assert.ok(
    ATTACH_TICKET_TTL_MS > 0 && ATTACH_TICKET_TTL_MS <= 60_000,
    'a ticket outliving the gesture that minted it is a bearer credential in an edge log',
  );
});

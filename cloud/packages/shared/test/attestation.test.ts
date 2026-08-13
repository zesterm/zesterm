/**
 * The attestation encoding against the Rust's own fixture,
 * `crates/zest-proto/fixtures/attest.json` — read from the repo, not copied,
 * so the file the Rust test gates is the file this one runs against.
 *
 * What is pinned here is the *bytes*: the canonical message for every fixture
 * case, the blob alphabet, and every shape the decoder must refuse. Whether a
 * real signature over those bytes verifies is proved a layer up, where the
 * Ed25519 is — `packages/web/test/attest-golden.test.ts` — because this
 * package deliberately has no runtime dependencies. Same split as
 * `ticket.test.ts`, for the same reason.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

import {
  ATTESTATION_SIGNATURE_LEN,
  ATTESTATION_VERSION,
  MAX_ATTESTATION_CHARS,
  attestationMessage,
  decodeAttestation,
  encodeAttestation,
  fromHex,
  hex,
  toBase64Url,
  type AttestationFields,
} from '../src/index.ts';

const HERE = dirname(fileURLToPath(import.meta.url));
const FIXTURE = JSON.parse(
  readFileSync(join(HERE, '../../../../crates/zest-proto/fixtures/attest.json'), 'utf8'),
) as {
  cases: Array<{
    name: string;
    attestation: AttestationFields;
    message: string;
    signature: string;
    now_ms: number;
    expect_verify: boolean;
  }>;
};

test('every fixture case encodes to the exact bytes the Rust signed', () => {
  assert.ok(FIXTURE.cases.length >= 3, 'the fixture pins at least three cases');
  for (const c of FIXTURE.cases) {
    assert.equal(
      hex(attestationMessage(c.attestation)),
      c.message,
      `${c.name}: a byte of difference here refuses every real voucher — and the astral case ` +
        'is the one that catches a length counted in UTF-16 units',
    );
  }
});

test('a blob round-trips: decode(encode) is the fields, the bytes and the signature', () => {
  const c = FIXTURE.cases[0]!;
  const message = attestationMessage(c.attestation);
  const signature = fromHex(c.signature, ATTESTATION_SIGNATURE_LEN)!;
  const blob = encodeAttestation(message, signature);

  // The exact string, derived independently, so the encoder cannot drift from
  // the `payload.signature` shape the daemons will parse.
  assert.equal(blob, `${toBase64Url(message)}.${toBase64Url(signature)}`);
  assert.ok(!blob.includes('='), 'unpadded: `=` is not safe in every carrier this will ride');

  const decoded = decodeAttestation(blob);
  assert.ok(decoded, 'our own encoding must decode');
  assert.deepEqual(decoded.fields, c.attestation, 'every signed field survives the round trip');
  assert.equal(hex(decoded.message), c.message, 'the arrived bytes are what verification will cover');
  assert.equal(hex(decoded.signature), c.signature);
});

test('the decoder refuses every malformed shape with one null', () => {
  const c = FIXTURE.cases[0]!;
  const message = attestationMessage(c.attestation);
  const signature = fromHex(c.signature, ATTESTATION_SIGNATURE_LEN)!;
  const good = encodeAttestation(message, signature);
  assert.ok(decodeAttestation(good), 'the control must decode, or the refusals below prove nothing');

  const trailing = new Uint8Array(message.length + 1);
  trailing.set(message);
  const truncated = message.subarray(0, message.length - 1);
  const v2 = new Uint8Array(message);
  v2[18] = 2; // the u16be version straight after the 17-byte domain
  const domain = new Uint8Array(message);
  domain[0] = 0x5a; // 'Z' — not the attest domain
  // exp with its top bit set: a u64 no JS number can hold exactly, which a
  // reader that rounded would compare against Date.now() as some other instant.
  const hugeExp = new Uint8Array(message);
  hugeExp[message.length - 8] = 0x80;

  const cases: Array<[string, string]> = [
    ['empty', ''],
    ['no separator', toBase64Url(message)],
    ['two separators', `${good}.extra`],
    ['padding smuggled in', `${toBase64Url(message)}=.${toBase64Url(signature)}`],
    ['not base64url at all', `!!!.${toBase64Url(signature)}`],
    ['a 63-byte signature', `${toBase64Url(message)}.${toBase64Url(signature.subarray(1))}`],
    ['trailing bytes after exp', encodeAttestation(trailing, signature)],
    ['a truncated message', encodeAttestation(truncated, signature)],
    ['a version this build does not speak', encodeAttestation(v2, signature)],
    ['a different domain', encodeAttestation(domain, signature)],
    ['a timestamp past MAX_SAFE_INTEGER', encodeAttestation(hugeExp, signature)],
    ['longer than the bound', `${'a'.repeat(MAX_ATTESTATION_CHARS)}.${toBase64Url(signature)}`],
  ];
  for (const [why, text] of cases) {
    assert.equal(decodeAttestation(text), null, why);
  }
});

test('the boundary between account and device cannot be moved', () => {
  // The Rust test's own construction: without length prefixes these two would
  // concatenate to identical bytes while naming DIFFERENT device keys, so one
  // signature would vouch for a key the approver never saw.
  const base = FIXTURE.cases[0]!.attestation;
  const left = {
    ...base,
    account: 'ab',
    device: '63' + '41'.repeat(31), // 'c' ++ [0x41; 31]
    label: 'cd',
  };
  const right = {
    ...base,
    account: 'abc',
    device: '41'.repeat(31) + '63', // [0x41; 31] ++ 'c'
    label: 'd',
  };
  assert.notEqual(
    hex(attestationMessage(left)),
    hex(attestationMessage(right)),
    'field boundaries must be unambiguous',
  );
});

test('an unencodable field is refused rather than truncated', () => {
  const base = FIXTURE.cases[0]!.attestation;
  const huge = 'x'.repeat(0xffff + 1);
  assert.throws(() => attestationMessage({ ...base, account: huge }), RangeError, 'account');
  assert.throws(() => attestationMessage({ ...base, label: huge }), RangeError, 'label');
  // Exactly at the boundary still encodes, so the limit is the encoding's.
  assert.ok(attestationMessage({ ...base, label: 'x'.repeat(0xffff) }));
  // And a key that is not lowercase hex is refused before any bytes exist.
  assert.throws(() => attestationMessage({ ...base, by: base.by.toUpperCase() }), RangeError);
});

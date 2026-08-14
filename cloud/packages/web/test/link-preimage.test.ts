/**
 * The link messages against the Rust's own fixture,
 * `crates/zest-proto/fixtures/link.json` — read from the repo, not copied, so
 * the file `cargo xtask check-fixtures` gates is the file this runs against.
 *
 * The desktop app signs in Rust and this Worker verifies in TypeScript; they
 * share no code, so a drift surfaces as a sign-in that hangs on "pending"
 * forever with neither side able to say which moved. The fixture's
 * MUST-fail case is the property the tag byte exists for: an approval-phase
 * signature presented as a claim.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

import { fromHex, hex } from '@zesterm/cloud-shared';

import {
  linkClaim,
  linkRequest,
  verifyLinkClaim,
  verifyLinkRequest,
} from '../src/enroll/link-preimage.ts';
import { KEY_LEN, SIGNATURE_LEN } from '../src/enroll/preimage.ts';

const HERE = dirname(fileURLToPath(import.meta.url));
const FIXTURE = JSON.parse(
  readFileSync(join(HERE, '../../../../crates/zest-proto/fixtures/link.json'), 'utf8'),
) as {
  cases: Array<{
    name: string;
    key: string;
    verify_as: 'request' | 'claim';
    label?: string;
    grant?: string;
    message: string;
    signature: string;
    expect_verify: boolean;
  }>;
};

function key(hexText: string): Uint8Array {
  const bytes = fromHex(hexText, KEY_LEN);
  assert.ok(bytes, 'the fixture key must be 32 bytes of hex');
  return bytes;
}

test('every fixture case builds the exact bytes the Rust signed, and the verdicts hold', async () => {
  assert.ok(FIXTURE.cases.length >= 4, 'the fixture pins at least four cases');
  let sawARefusal = false;
  for (const c of FIXTURE.cases) {
    const k = key(c.key);
    const message =
      c.verify_as === 'request' ? linkRequest(k, c.label!) : linkClaim(c.grant!, k);
    assert.equal(
      hex(message),
      c.message,
      `${c.name}: a byte of difference refuses every real sign-in — the astral case is the ` +
        'one that catches a length counted in UTF-16 units',
    );

    const signature = fromHex(c.signature, SIGNATURE_LEN)!;
    const verified =
      c.verify_as === 'request'
        ? await verifyLinkRequest({ key: k, label: c.label!, signature })
        : await verifyLinkClaim({ grant: c.grant!, key: k, signature });
    assert.equal(
      verified,
      c.expect_verify,
      `${c.name}: the MUST-fail case is a request signature spent as a claim — the tag byte's whole job`,
    );
    sawARefusal ||= !c.expect_verify;
  }
  assert.ok(sawARefusal, 'the fixture must carry a case that verifies false, or a verifier that accepts everything passes');
});

test('the two messages differ even over identical field bytes', () => {
  // The tag byte, pinned structurally: same key, and the request's label
  // equal to the claim's grant, must still build different bytes.
  const k = key(FIXTURE.cases[0]!.key);
  assert.notEqual(
    hex(linkRequest(k, 'same-string')),
    hex(linkClaim('same-string', k)),
    'without the tag, disjointness would rest on key bytes never resembling a length prefix',
  );
});

test('a malformed signature or key is false, never a throw', async () => {
  const cases = [
    { key: new Uint8Array(31), signature: new Uint8Array(SIGNATURE_LEN) },
    { key: new Uint8Array(KEY_LEN), signature: new Uint8Array(63) },
    // Valid lengths, nonsense contents: [2; 32] is not a curve point.
    { key: new Uint8Array(KEY_LEN).fill(2), signature: new Uint8Array(SIGNATURE_LEN).fill(9) },
  ];
  for (const { key: k, signature } of cases) {
    assert.equal(await verifyLinkRequest({ key: k, label: 'x', signature }), false);
    assert.equal(await verifyLinkClaim({ grant: 'g', key: k, signature }), false);
  }
});

test('a small-order key cannot link', async () => {
  // The all-zero point verifies almost anything under ZIP-215, noble's
  // default; the app side is dalek's verify_strict world, so `zip215: false`
  // here is what keeps the two agreeing about who may enter.
  assert.equal(
    await verifyLinkRequest({
      key: new Uint8Array(KEY_LEN),
      label: 'x',
      signature: new Uint8Array(SIGNATURE_LEN),
    }),
    false,
  );
});

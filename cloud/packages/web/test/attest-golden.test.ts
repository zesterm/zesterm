/**
 * The Rust's attestation signatures against this Worker's verification —
 * the Ed25519 half of the golden, read from
 * `crates/zest-proto/fixtures/attest.json` like the message half in
 * `packages/shared/test/attestation.test.ts`.
 *
 * Every signature in the fixture came out of the Rust signer, so what is
 * proved here is the cross-implementation contract itself: the purpose string
 * in the wrap, the zip215:false verification, and the half-open window whose
 * expired case is evaluated at `now == exp` exactly.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

import {
  attestationMessage,
  encodeAttestation,
  decodeAttestation,
  fromHex,
  type AttestationFields,
} from '@zesterm/cloud-shared';

import { attestationSignatureOk } from '../src/api/attest.ts';

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

test('every Rust signature verifies here, and the expired case is the window, not the key', async () => {
  let sawARefusal = false;
  for (const c of FIXTURE.cases) {
    const blob = encodeAttestation(attestationMessage(c.attestation), fromHex(c.signature, 64)!);
    const decoded = decodeAttestation(blob);
    assert.ok(decoded, `${c.name}: the fixture case must decode`);

    // The signature itself: the Rust signed these bytes under
    // `client\0device-attestation`, and this side must agree — including for
    // the expired case, whose signature is perfectly valid.
    assert.equal(
      await attestationSignatureOk(decoded),
      true,
      `${c.name}: a valid Rust signature must verify, or every real voucher is refused`,
    );

    // The fixture's verdict combines window and signature; the route applies
    // the window separately (iat skew, TTL), so what is asserted here is that
    // the *refusing* case refuses for the window alone.
    const inWindow = c.now_ms >= c.attestation.iat && c.now_ms < c.attestation.exp;
    assert.equal(
      inWindow,
      c.expect_verify,
      `${c.name}: [iat, exp) evaluated at now_ms must match the fixture — the expired case sits ` +
        'at now == exp exactly, so an implementation accepting now <= exp fails here by name',
    );
    sawARefusal ||= !c.expect_verify;
  }
  assert.ok(sawARefusal, 'the fixture must carry a MUST-fail case, or a verifier that accepts everything passes');
});

test('a tampered message or a foreign key does not verify', async () => {
  const c = FIXTURE.cases[0]!;
  const message = attestationMessage(c.attestation);
  const signature = fromHex(c.signature, 64)!;

  const tampered = decodeAttestation(
    encodeAttestation(attestationMessage({ ...c.attestation, label: 'renamed' }), signature),
  );
  assert.ok(tampered);
  assert.equal(
    await attestationSignatureOk(tampered),
    false,
    'a renamed label is a different statement — the signature must not transfer',
  );

  const wrongKey = decodeAttestation(
    encodeAttestation(
      attestationMessage({ ...c.attestation, by: c.attestation.device }),
      signature,
    ),
  );
  assert.ok(wrongKey);
  assert.equal(
    await attestationSignatureOk(wrongKey),
    false,
    'verification is against `by` and no other key — a swapped approver must fail',
  );

  const smallOrder = decodeAttestation(
    encodeAttestation(
      attestationMessage({ ...c.attestation, by: '00'.repeat(32) }),
      new Uint8Array(64),
    ),
  );
  assert.ok(smallOrder);
  assert.equal(
    await attestationSignatureOk(smallOrder),
    false,
    'zip215:false — the all-zero point verifies almost anything under ZIP-215, and the daemons use verify_strict',
  );
});

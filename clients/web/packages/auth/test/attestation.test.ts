/**
 * `attestation.ts` against `crates/zest-proto/fixtures/attest.json` — read
 * from the repo like `secure.test.ts` reads `handshake.json`, so the file the
 * Rust test gates is the file this one runs against.
 *
 * Three implementations build these bytes (Rust, the Worker, this package)
 * and none can import another, so the fixture is the only thing keeping them
 * byte-identical. Ed25519 is deterministic, which buys the strongest pin
 * available: the fixture's approver seed driven through this package's OWN
 * signer must reproduce the fixture's signatures bit for bit — message, wrap
 * and purpose string all at once.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

import { bytesToHex } from '@zesterm/proto';

import {
  attestationMessage,
  attestDevice,
  encodeAttestation,
  verifyAttestation,
  type AttestationFields,
} from '../src/attestation.ts';
import { generateIdentity, seedSigner } from '../src/identity.ts';

const HERE = dirname(fileURLToPath(import.meta.url));
const FIXTURE = JSON.parse(
  readFileSync(join(HERE, '../../../../../crates/zest-proto/fixtures/attest.json'), 'utf8'),
) as {
  cases: Array<{
    name: string;
    approver_seed: string;
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
      bytesToHex(attestationMessage(c.attestation)),
      c.message,
      `${c.name}: a byte of difference refuses every real voucher — the astral case is the ` +
        'one that catches a length counted in UTF-16 units instead of UTF-8 bytes',
    );
  }
});

test('the fixture verdicts hold: valid signatures verify, the expired case refuses', () => {
  let sawARefusal = false;
  for (const c of FIXTURE.cases) {
    assert.equal(
      verifyAttestation(c.attestation, c.signature, c.now_ms),
      c.expect_verify,
      `${c.name}: the expired case sits at now == exp exactly, so an implementation ` +
        'accepting now <= exp fails here by name',
    );
    sawARefusal ||= !c.expect_verify;
  }
  assert.ok(sawARefusal, 'the fixture must carry a MUST-fail case, or a verifier that accepts everything passes');
});

test('signing through this package’s own signer reproduces the fixture signatures', async () => {
  // The strongest pin determinism buys: same seed, same bytes, same wrap →
  // the same 64 bytes the Rust produced. A drift in the message, the
  // `device-attestation` purpose string or the signer's preimage wrap all
  // fail here, naming this side as the one that moved.
  for (const c of FIXTURE.cases) {
    const identity = generateIdentity(c.approver_seed);
    assert.equal(identity.clientId, c.attestation.by, `${c.name}: the fixture seed must name the fixture approver`);
    const signer = seedSigner(identity);
    const sig = await signer.sign('device-attestation', attestationMessage(c.attestation));
    assert.equal(sig, c.signature, `${c.name}: Ed25519 is deterministic, so anything but equality is drift`);
  }
});

test('attestDevice builds the blob the Worker will decode', async () => {
  const c = FIXTURE.cases[0]!;
  const signer = seedSigner(generateIdentity(c.approver_seed));
  const blob = await attestDevice(signer, {
    account: c.attestation.account,
    device: c.attestation.device,
    label: c.attestation.label,
    iat: c.attestation.iat,
    exp: c.attestation.exp,
  });

  // The exact string, derived independently from the fixture bytes — pinning
  // the blob format (unpadded base64url, one dot) against the Worker's
  // decoder without being able to import it.
  const expected =
    Buffer.from(c.message, 'hex').toString('base64url') +
    '.' +
    Buffer.from(c.signature, 'hex').toString('base64url');
  assert.equal(blob, expected, 'message and signature, base64url unpadded, dot-separated');
  assert.ok(!blob.includes('='), 'unpadded: `=` is not safe in every carrier this will ride');

  // `by` is the signer's, not a parameter: a signer cannot be asked to vouch
  // under someone else's name.
  assert.equal(
    blob,
    encodeAttestation(attestationMessage(c.attestation), c.signature),
    'and the pieces compose to the same blob',
  );
});

test('a changed field or an out-of-window clock refuses', () => {
  const c = FIXTURE.cases[0]!;
  assert.equal(
    verifyAttestation({ ...c.attestation, label: 'renamed' }, c.signature, c.now_ms),
    false,
    'the label is signed so a row cannot be renamed to impersonate afterwards',
  );
  assert.equal(
    verifyAttestation(c.attestation, c.signature, c.attestation.iat - 1),
    false,
    'not live before iat: a stolen approver key must not pre-date vouchers past its revocation',
  );
  assert.equal(
    verifyAttestation(c.attestation, c.signature, c.attestation.iat),
    true,
    'iat itself is inside the window — sign-then-verify on a fast clock must not fail',
  );
});

test('an oversize or malformed field refuses to encode at all', () => {
  const c = FIXTURE.cases[0]!;
  const huge = 'x'.repeat(0xffff + 1);
  assert.throws(() => attestationMessage({ ...c.attestation, account: huge }), RangeError);
  assert.throws(() => attestationMessage({ ...c.attestation, label: huge }), RangeError);
  assert.throws(
    () => attestationMessage({ ...c.attestation, by: c.attestation.by.toUpperCase() }),
    RangeError,
    'an uppercase key would silently sign different bytes than the registry compares',
  );
});

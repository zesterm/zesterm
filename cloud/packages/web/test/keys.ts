/**
 * The daemon's half of enrolment, in the tests.
 *
 * Real Ed25519 over the real preimage — not a stub that returns "valid". A fake
 * signer would let every ordering and role test below pass against a
 * verification that does nothing, which is the one thing they exist to rule out.
 */

import { getPublicKeyAsync, signAsync } from '@noble/ed25519';

import {
  attestationMessage,
  encodeAttestation,
  hex,
  signingPreimage,
  type AttestationFields,
} from '@zesterm/cloud-shared';
import { enrollmentPreimage, type Role } from '../src/enroll/preimage.ts';
import { linkClaim, linkRequest } from '../src/enroll/link-preimage.ts';
import { registerPreimage } from '../src/enroll/register-preimage.ts';

export interface TestKey {
  readonly seed: Uint8Array;
  /** 64 hex — the public key, which *is* the id. */
  readonly id: string;
}

/** A deterministic key, so a failure names the same id every run. */
export async function testKey(seed: number): Promise<TestKey> {
  const bytes = new Uint8Array(32).fill(seed);
  return { seed: bytes, id: hex(await getPublicKeyAsync(bytes)) };
}

/** What `zest-daemon` sends: the signature over `(code, key, label)`. */
export async function signEnrollment(args: {
  key: TestKey;
  code: string;
  label: string;
  role?: Role;
}): Promise<string> {
  const { key, code, label, role = 'host' } = args;
  const preimage = enrollmentPreimage(role, code, await getPublicKeyAsync(key.seed), label);
  return hex(await signAsync(preimage, key.seed));
}

/** What a signed-in browser sends: the signature over `(account, key, label)`. */
export async function signRegistration(args: {
  key: TestKey;
  account: string;
  label: string;
}): Promise<string> {
  const { key, account, label } = args;
  const preimage = registerPreimage(account, await getPublicKeyAsync(key.seed), label);
  return hex(await signAsync(preimage, key.seed));
}

/**
 * A complete attestation blob, signed by `key` as the approver.
 *
 * `by` defaults to the signing key's own id — the honest voucher — and can be
 * overridden so tests can build the dishonest one: a statement naming one
 * approver, signed by another, which the route must refuse.
 */
export async function attestationBlob(
  key: TestKey,
  fields: Omit<AttestationFields, 'v' | 'by'> & { by?: string },
): Promise<string> {
  const message = attestationMessage({ v: 1, ...fields, by: fields.by ?? key.id });
  const sig = await signAsync(signingPreimage('client', 'device-attestation', message), key.seed);
  return encodeAttestation(message, sig);
}

/** What the desktop app sends to ask for a link grant. */
export async function signLinkRequest(key: TestKey, label: string): Promise<string> {
  const message = linkRequest(await getPublicKeyAsync(key.seed), label);
  return hex(await signAsync(signingPreimage('client', 'enrollment', message), key.seed));
}

/** What the desktop app sends to spend one. */
export async function signLinkClaim(key: TestKey, grant: string): Promise<string> {
  const message = linkClaim(grant, await getPublicKeyAsync(key.seed));
  return hex(await signAsync(signingPreimage('client', 'enrollment', message), key.seed));
}

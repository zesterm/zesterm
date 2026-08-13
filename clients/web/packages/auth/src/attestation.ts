/**
 * The device attestation, as the approving browser builds it — this
 * workspace's half of `crates/zest-mesh/src/attest.rs`, and of
 * `cloud/packages/shared/src/attestation.ts` on the Worker side.
 *
 * ```
 * message  = "zesterm-attest-v1" ++ u16be(v)
 *                                ++ u16be(len(account)) ++ account
 *                                ++ device (32 raw bytes)
 *                                ++ u16be(len(label)) ++ label
 *                                ++ by (32 raw bytes)
 *                                ++ u64be(iat) ++ u64be(exp)
 * blob     = base64url(message) "." base64url(signature)   (unpadded)
 * ```
 *
 * The signature is `ClientSigner.sign('device-attestation', message)` — the
 * signer applies the `zesterm-sig-v1\0client\0device-attestation\0` wrap
 * itself, exactly as it does for `register.ts`'s request. Lengths count
 * **UTF-8 bytes**, never UTF-16 code units; the window is `[iat, exp)`,
 * iat inclusive, exp exclusive.
 *
 * This workspace cannot import the Worker's encoder (three projects, three
 * lockfiles), so all three copies — Rust, Worker, this — are pinned to one
 * another by `crates/zest-proto/fixtures/attest.json`: the message hex, the
 * signatures (Ed25519 is deterministic, so a fixture seed reproduces them
 * exactly), and the expired case that must refuse.
 */

import { bytesToHex, hexToBytes } from '@zesterm/proto';

import { verifyClientSignature } from './identity.ts';
import type { ClientSigner } from './signer.ts';

const TEXT = new TextEncoder();

const ATTEST_DOMAIN = 'zesterm-attest-v1';

/** The only layout this build encodes. Signed, so it cannot be replayed as another. */
export const ATTESTATION_VERSION = 1;

/**
 * A year — the same constant the Worker's `attestation.ts` holds, duplicated
 * because the workspaces cannot share it. Long-lived on purpose: the daemon
 * records trust one-time on first contact, so an attestation is an
 * *introduction*, not a lease — revocation, not expiry, is what undoes it —
 * and renewal is simply re-approving.
 */
export const ATTESTATION_TTL_MS = 365 * 24 * 60 * 60 * 1000;

/** The statement, with the two keys as 64-lowercase-hex `ClientId`s. */
export interface AttestationFields {
  readonly v: typeof ATTESTATION_VERSION;
  readonly account: string;
  /** The key being vouched for. */
  readonly device: string;
  readonly label: string;
  /** The approver — verification is against this key and no other. */
  readonly by: string;
  readonly iat: number;
  readonly exp: number;
}

function lengthPrefixed(text: string, what: string): Uint8Array[] {
  const bytes = TEXT.encode(text);
  if (bytes.length > 0xffff) {
    // Refused rather than truncated, as the Rust refuses: two values sharing
    // a 65535-byte prefix must not produce identical signed bytes.
    throw new RangeError(`${what} is at most 65535 bytes, this one is ${bytes.length}`);
  }
  return [new Uint8Array([bytes.length >>> 8, bytes.length & 0xff]), bytes];
}

function keyBytes(hexText: string, what: string): Uint8Array {
  const bytes = hexToBytes(hexText);
  if (bytes.length !== 32 || bytesToHex(bytes) !== hexText) {
    // The round-trip check is the lowercase check: `hexToBytes` folds case,
    // and an uppercase id re-encoded lowercase would silently sign different
    // bytes than the caller compared against the registry.
    throw new RangeError(`${what} is 32 bytes of lowercase hex`);
  }
  return bytes;
}

function u64be(value: number): Uint8Array {
  if (!Number.isSafeInteger(value) || value < 0) {
    throw new RangeError(`a timestamp is a non-negative safe integer, got ${value}`);
  }
  const out = new Uint8Array(8);
  new DataView(out.buffer).setBigUint64(0, BigInt(value), false);
  return out;
}

/** `zest_mesh::attest::attestation_message`, verbatim. */
export function attestationMessage(a: AttestationFields): Uint8Array {
  const parts: Uint8Array[] = [
    TEXT.encode(ATTEST_DOMAIN),
    new Uint8Array([(a.v >>> 8) & 0xff, a.v & 0xff]),
    ...lengthPrefixed(a.account, 'an account id'),
    keyBytes(a.device, 'device'),
    ...lengthPrefixed(a.label, 'a device label'),
    keyBytes(a.by, 'by'),
    u64be(a.iat),
    u64be(a.exp),
  ];
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let at = 0;
  for (const part of parts) {
    out.set(part, at);
    at += part.length;
  }
  return out;
}

/**
 * `base64url(message) "." base64url(signature)` — **unpadded**, the ticket
 * discipline: `=` and `/` are the characters some intermediary will
 * eventually escape, truncate or reject.
 */
export function encodeAttestation(message: Uint8Array, signatureHex: string): string {
  return `${base64Url(message)}.${base64Url(hexToBytes(signatureHex))}`;
}

function base64Url(bytes: Uint8Array): string {
  let binary = '';
  for (const b of bytes) binary += String.fromCharCode(b);
  return btoa(binary).replace(/\+/g, '-').replace(/\//g, '_').replace(/=+$/, '');
}

/**
 * Vouch for a device: build the statement, sign it as this signer, and hand
 * back the blob for `POST /api/devices/:id/approve`.
 *
 * `by` is the signer's own id — a signer cannot be asked to vouch under
 * someone else's name, mirroring the Rust's `SignerIsNotApprover` refusal at
 * mint time rather than as a verify that fails later naming nothing.
 */
export async function attestDevice(
  signer: ClientSigner,
  args: {
    readonly account: string;
    readonly device: string;
    readonly label: string;
    readonly iat: number;
    readonly exp: number;
  },
): Promise<string> {
  const message = attestationMessage({ v: ATTESTATION_VERSION, by: signer.clientId, ...args });
  return encodeAttestation(message, await signer.sign('device-attestation', message));
}

/**
 * Did the holder of `by` vouch for exactly this statement, and is it live at
 * `nowMs`? The window is checked first — `[iat, exp)`, exp exclusive — so a
 * voucher dead on arrival costs no signature work, exactly as the Rust
 * orders it. Nothing here consults a trust store: non-transitivity is the
 * daemon's check against its own, deliberately not mixed in where a missing
 * check would look like a passing one.
 */
export function verifyAttestation(a: AttestationFields, signatureHex: string, nowMs: number): boolean {
  if (nowMs < a.iat || nowMs >= a.exp) return false;
  try {
    return verifyClientSignature(a.by, 'device-attestation', attestationMessage(a), signatureHex);
  } catch {
    return false;
  }
}

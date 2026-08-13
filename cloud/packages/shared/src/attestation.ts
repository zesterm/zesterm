/**
 * The device attestation: one trusted device vouching for a new one — a
 * byte-for-byte port of `crates/zest-mesh/src/attest.rs`, pinned to it by
 * `crates/zest-proto/fixtures/attest.json`.
 *
 * ```
 * message  = "zesterm-attest-v1" ++ u16be(v)
 *                                ++ u16be(len(account)) ++ account
 *                                ++ device (32 raw bytes)
 *                                ++ u16be(len(label)) ++ label
 *                                ++ by (32 raw bytes)
 *                                ++ u64be(iat) ++ u64be(exp)
 * preimage = "zesterm-sig-v1" \0 "client" \0 "device-attestation" \0 message
 * blob     = base64url(message) "." base64url(signature)
 * ```
 *
 * Lengths count **UTF-8 bytes**, never UTF-16 code units — the fixture's
 * astral-label case is what catches `.length`. The window is `[iat, exp)` in
 * epoch milliseconds: iat inclusive, exp exclusive, and the fixture's expired
 * case is evaluated at `now == exp` exactly so an implementation that accepts
 * `now <= exp` fails it by name.
 *
 * **Length-prefixed, not NUL-separated**, for `attest.rs`'s reason: `account`
 * and `label` are both caller-supplied, and without prefixes the
 * account/device boundary is movable — two statements naming *different device
 * keys* could concatenate to identical bytes, so a signature over one would
 * vouch for a key the approver never saw.
 *
 * Bytes only, like `ticket.ts`: no Ed25519 here — the web Worker and the
 * browser each carry their own — which is what keeps this package
 * dependency-free and every property below under `node --test`.
 *
 * **Verification is over the bytes that arrived, never a re-encoding.**
 * `decodeAttestation` hands back the exact message bytes it parsed, and that
 * is what the signature covers; the blob is likewise stored and re-served
 * verbatim, because a daemon at the far end verifies the same bytes.
 */

import { fromBase64Url, fromHex, hex, toBase64Url, utf8 } from './bytes.ts';

/** The only layout this build encodes or accepts. Signed, so it cannot be replayed as another. */
export const ATTESTATION_VERSION = 1;

const ATTEST_DOMAIN = 'zesterm-attest-v1';

/** A `ClientId` is the device's Ed25519 public key verbatim (ADR-006), so 32 bytes. */
export const ATTESTATION_KEY_LEN = 32;

/** An Ed25519 signature. Always 64 bytes; a short one is an error, never a pad. */
export const ATTESTATION_SIGNATURE_LEN = 64;

/**
 * A year. Long-lived on purpose (revocation Model B): the daemon records
 * trust one-time on first contact, so an attestation is an *introduction*,
 * not a lease — what un-introduces a device is the revocation list served
 * beside the blobs, not the clock. Renewal is simply re-approving, which the
 * approve route treats as an idempotent replace.
 */
export const ATTESTATION_TTL_MS = 365 * 24 * 60 * 60 * 1000;

/**
 * How long a blob may be before it is refused unread — `ticket.ts`'s
 * discipline, and for its reason: the blob is caller-supplied and reaches the
 * decoder before any signature is checked, so the cheap bound comes first.
 * Over real fields — a ~30-byte account id, a 64-code-point label at four
 * bytes each — the message is under 450 bytes and the blob under 700
 * characters, so two kilobytes is nearly three times any honest one.
 */
export const MAX_ATTESTATION_CHARS = 2048;

/**
 * The statement, all of it — `zest_mesh::attest::Attestation`, with the two
 * keys as 64-lowercase-hex strings because that is how the API and the
 * database spell a `ClientId`; the message carries their 32 raw bytes.
 */
export interface AttestationFields {
  readonly v: typeof ATTESTATION_VERSION;
  /** The control plane's user id, so a voucher for one account cannot admit to another. */
  readonly account: string;
  /** The key being vouched for. */
  readonly device: string;
  /** Human name, signed so the row cannot be renamed to impersonate afterwards. */
  readonly label: string;
  /** The approver — verification is against this key and no other. */
  readonly by: string;
  /** Issued at, epoch ms. Inclusive edge of the window. */
  readonly iat: number;
  /** Expires at, epoch ms. Exclusive: dead the millisecond it names. */
  readonly exp: number;
}

function pushLenPrefixed(parts: Uint8Array[], text: string, what: string): void {
  const bytes = utf8(text);
  if (bytes.length > 0xffff) {
    // Refused rather than truncated, as the Rust refuses: truncating would
    // make two values sharing a 65535-byte prefix produce identical signed
    // bytes, and both strings here are caller-supplied.
    throw new RangeError(`${what} is at most 65535 bytes, this one is ${bytes.length}`);
  }
  parts.push(new Uint8Array([bytes.length >>> 8, bytes.length & 0xff]), bytes);
}

function u16be(value: number): Uint8Array {
  return new Uint8Array([(value >>> 8) & 0xff, value & 0xff]);
}

function u64be(value: number): Uint8Array {
  if (!Number.isSafeInteger(value) || value < 0) {
    throw new RangeError(`a timestamp is a non-negative safe integer, got ${value}`);
  }
  const out = new Uint8Array(8);
  new DataView(out.buffer).setBigUint64(0, BigInt(value), false);
  return out;
}

function keyBytes(hexText: string, what: string): Uint8Array {
  const bytes = fromHex(hexText, ATTESTATION_KEY_LEN);
  if (bytes === null) {
    throw new RangeError(`${what} is ${ATTESTATION_KEY_LEN} bytes of lowercase hex`);
  }
  return bytes;
}

/** `zest_mesh::attest::attestation_message`, verbatim. */
export function attestationMessage(a: AttestationFields): Uint8Array {
  const parts: Uint8Array[] = [utf8(ATTEST_DOMAIN), u16be(a.v)];
  pushLenPrefixed(parts, a.account, 'an account id');
  parts.push(keyBytes(a.device, 'device'));
  pushLenPrefixed(parts, a.label, 'a device label');
  parts.push(keyBytes(a.by, 'by'), u64be(a.iat), u64be(a.exp));

  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let at = 0;
  for (const part of parts) {
    out.set(part, at);
    at += part.length;
  }
  return out;
}

/** `base64url(message) "." base64url(signature)` — unpadded, `ticket.ts`'s discipline. */
export function encodeAttestation(message: Uint8Array, signature: Uint8Array): string {
  return `${toBase64Url(message)}.${toBase64Url(signature)}`;
}

export interface DecodedAttestation {
  readonly fields: AttestationFields;
  /** The message exactly as it arrived — the bytes the signature covers. */
  readonly message: Uint8Array;
  readonly signature: Uint8Array;
}

/**
 * Split, decode and narrow — and **verify nothing**, `decodeAttachTicket`'s
 * split for its reason: this package has no Ed25519 and the caller does.
 * Every malformed shape collapses to one `null`.
 *
 * The binary walk refuses short buffers, **trailing bytes**, and any version
 * or domain that is not this one. Trailing bytes matter more than they look:
 * a decoder that ignored them would accept two different blobs as one
 * statement, and the extra bytes would still be inside what the signature is
 * checked over — a mismatch that surfaces as "bad signature" and names
 * nothing.
 */
export function decodeAttestation(text: string): DecodedAttestation | null {
  if (text.length === 0 || text.length > MAX_ATTESTATION_CHARS) return null;

  const dot = text.indexOf('.');
  if (dot === -1) return null;
  if (text.indexOf('.', dot + 1) !== -1) return null;

  const message = fromBase64Url(text.slice(0, dot));
  const signature = fromBase64Url(text.slice(dot + 1));
  if (message === null || signature === null) return null;
  if (signature.length !== ATTESTATION_SIGNATURE_LEN) return null;

  const fields = parseMessage(message);
  return fields === null ? null : { fields, message, signature };
}

/** Field by field off the wire bytes, never a cast. `null` on anything malformed. */
function parseMessage(message: Uint8Array): AttestationFields | null {
  const domain = utf8(ATTEST_DOMAIN);
  let at = 0;

  const take = (n: number): Uint8Array | null => {
    if (at + n > message.length) return null;
    const out = message.subarray(at, at + n);
    at += n;
    return out;
  };

  const head = take(domain.length);
  if (head === null || hex(head) !== hex(domain)) return null;

  const readU16 = (): number | null => {
    const b = take(2);
    return b === null ? null : (b[0]! << 8) | b[1]!;
  };
  const readU64 = (): number | null => {
    const b = take(8);
    if (b === null) return null;
    const value = new DataView(b.buffer, b.byteOffset, 8).getBigUint64(0, false);
    // Timestamps are compared against `Date.now()` as numbers, so a value a
    // JS number cannot hold exactly is refused rather than rounded — a
    // rounded `exp` is a window boundary the two implementations disagree on.
    return value > BigInt(Number.MAX_SAFE_INTEGER) ? null : Number(value);
  };
  const readString = (): string | null => {
    const len = readU16();
    if (len === null) return null;
    const bytes = take(len);
    if (bytes === null) return null;
    // An invalid UTF-8 sequence is a refusal, not U+FFFD: a string that
    // re-encodes to different bytes than were signed is a field the encoder
    // side could never have produced. Checked by round-trip rather than
    // `{ fatal: true }`, because the Workers lib and Node disagree about that
    // option's type — and valid UTF-8 always round-trips byte-identically,
    // so the check refuses exactly the malformed sequences.
    const text = new TextDecoder().decode(bytes);
    const back = utf8(text);
    if (back.length !== bytes.length || hex(back) !== hex(bytes)) return null;
    return text;
  };

  const v = readU16();
  if (v !== ATTESTATION_VERSION) return null;
  const account = readString();
  if (account === null) return null;
  const device = take(ATTESTATION_KEY_LEN);
  if (device === null) return null;
  const label = readString();
  if (label === null) return null;
  const by = take(ATTESTATION_KEY_LEN);
  if (by === null) return null;
  const iat = readU64();
  const exp = readU64();
  if (iat === null || exp === null) return null;

  if (at !== message.length) return null;

  return { v: ATTESTATION_VERSION, account, device: hex(device), label, by: hex(by), iat, exp };
}

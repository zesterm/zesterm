/**
 * The authentication transcript — a byte-for-byte port of
 * `zest-mesh/src/pairing.rs`'s `auth_transcript` and `pairing_code`.
 *
 * The Rust pins this layout with a SHA-256 golden because **changing it
 * unpairs every device in the field**; the tests here carry a vector printed
 * by that same Rust, so the two implementations can only drift loudly.
 *
 * Fixed-width fields first, then the two labels length-prefixed: labels are
 * attacker-influenced *and* variable, so they carry their own lengths rather
 * than relying on NUL separation.
 */

import { sha256 } from '@noble/hashes/sha2.js';
import { hexToBytes } from '@zesterm/proto';

const TEXT = new TextEncoder();

/**
 * **The `v2` counts transcript layouts, not protocol versions.**
 *
 * This is the second shape the signed bytes have ever had; the protocol they
 * carry is at 3, and the two will keep diverging. Deriving one from the other
 * is one line away and produces signatures that will not verify with nothing
 * in the error naming the cause. The Rust pins the literal with a test for the
 * same reason.
 */
const AUTH_DOMAIN = 'zesterm-auth-v2';
/** Distinct from the signature domains, so the code can never collide with a preimage. */
const CODE_DOMAIN = 'zesterm-pairing-code-v1\0';

export const PAIRING_CODE_DIGITS = 6;

/** Everything both ends agree they are talking about. Ids and nonces as hex. */
export interface Transcript {
  readonly version: number;
  /** 64 hex chars — the host's public key. */
  readonly host: string;
  /** 64 hex chars — this client's public key. */
  readonly client: string;
  /** 64 hex chars each. */
  readonly hostNonce: string;
  readonly clientNonce: string;
  readonly hostLabel: string;
  readonly clientLabel: string;
  /**
   * 64 hex chars each — the two ephemeral X25519 public keys.
   *
   * In here so the Ed25519 signatures that were already being exchanged
   * certify them, which is what removes the need for a certificate type or a
   * stored static key. A relay that substituted one would have to forge a
   * signature to make both sides agree on a channel.
   */
  readonly hostDh: string;
  readonly clientDh: string;
}

function fixed(hex: string, bytes: number, what: string): Uint8Array {
  const out = hexToBytes(hex);
  if (out.length !== bytes) {
    throw new Error(`${what}: expected ${bytes} bytes, got ${out.length}`);
  }
  return out;
}

/**
 * The label, UTF-8, u16-BE length prefix. **Refuses** anything that will not
 * fit rather than truncating.
 *
 * It used to clamp to 0xffff and truncate, matching what the Rust then did.
 * Both were wrong the same way: two labels sharing their first 65535 bytes
 * produced identical signed bytes, so a signature over one was a valid
 * signature over the other — and a label is attacker-influenced and is the
 * entire text of the approval prompt a person reads. Worse, the truncation was
 * an implicit rule this file had to reproduce exactly, and a disagreement
 * surfaced as a signature that would not verify with nothing naming the
 * length. (#43.)
 */
function lenPrefixed(label: string): Uint8Array {
  const bytes = TEXT.encode(label);
  if (bytes.length > 0xffff) {
    throw new Error(`label is ${bytes.length} bytes, which will not fit in a transcript`);
  }
  const out = new Uint8Array(2 + bytes.length);
  out[0] = bytes.length >> 8;
  out[1] = bytes.length & 0xff;
  out.set(bytes, 2);
  return out;
}

/** The exact bytes both sides sign. */
export function authTranscript(t: Transcript): Uint8Array {
  const parts: Uint8Array[] = [
    TEXT.encode(AUTH_DOMAIN),
    Uint8Array.of((t.version >> 8) & 0xff, t.version & 0xff),
    fixed(t.host, 32, 'Transcript.host'),
    fixed(t.client, 32, 'Transcript.client'),
    fixed(t.hostNonce, 32, 'Transcript.hostNonce'),
    fixed(t.clientNonce, 32, 'Transcript.clientNonce'),
    // After the nonces, before the labels: fixed-width fields stay together,
    // ahead of anything carrying its own length.
    fixed(t.hostDh, 32, 'Transcript.hostDh'),
    fixed(t.clientDh, 32, 'Transcript.clientDh'),
    lenPrefixed(t.hostLabel),
    lenPrefixed(t.clientLabel),
  ];
  const total = parts.reduce((n, p) => n + p.length, 0);
  const out = new Uint8Array(total);
  let at = 0;
  for (const p of parts) {
    out.set(p, at);
    at += p.length;
  }
  return out;
}

/**
 * The six digits a person compares between two screens.
 *
 * Derived from the transcript the signatures cover because its entire job is
 * to *differ* on the two screens when a relay is in the path: two handshakes
 * mean two nonce pairs mean two codes. Digits rather than hex because it is
 * read aloud.
 */
export function pairingCode(t: Transcript): string {
  const digest = sha256(concat(TEXT.encode(CODE_DOMAIN), authTranscript(t)));
  const n =
    (((digest[0] ?? 0) << 24) | ((digest[1] ?? 0) << 16) | ((digest[2] ?? 0) << 8) | (digest[3] ?? 0)) >>>
    0;
  return String(n % 10 ** PAIRING_CODE_DIGITS).padStart(PAIRING_CODE_DIGITS, '0');
}

function concat(a: Uint8Array, b: Uint8Array): Uint8Array {
  const out = new Uint8Array(a.length + b.length);
  out.set(a, 0);
  out.set(b, a.length);
  return out;
}

/** All zeroes reads as "absent", and the host refuses it explicitly. */
export function isAbsentNonce(hex: string): boolean {
  return /^0*$/.test(hex);
}

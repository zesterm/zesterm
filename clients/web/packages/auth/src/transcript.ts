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

const AUTH_DOMAIN = 'zesterm-auth-v1';
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
}

function fixed(hex: string, bytes: number, what: string): Uint8Array {
  const out = hexToBytes(hex);
  if (out.length !== bytes) {
    throw new Error(`${what}: expected ${bytes} bytes, got ${out.length}`);
  }
  return out;
}

/** The label, UTF-8, u16-BE length prefix — truncated at 64 KiB like the Rust. */
function lenPrefixed(label: string): Uint8Array {
  const bytes = TEXT.encode(label);
  const len = Math.min(bytes.length, 0xffff);
  const out = new Uint8Array(2 + len);
  out[0] = len >> 8;
  out[1] = len & 0xff;
  out.set(bytes.subarray(0, len), 2);
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

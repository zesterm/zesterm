/**
 * The exact bytes an Ed25519 key is asked to sign — a byte-for-byte port of
 * `zest-mesh/src/identity.rs`'s `preimage`.
 *
 * `zesterm-sig-v1 \0 role \0 purpose \0 message`. NUL separators are sound
 * here for one specific reason the Rust states: `role` and `purpose` come
 * from closed unions with fixed NUL-free ASCII names, so no shorter field can
 * be extended to look like a longer one, and the only attacker-influenced
 * field is last.
 *
 * A prefix rather than Ed25519ctx *on purpose*: the Rust chose it because
 * this package's libraries — `@noble/ed25519` and friends — do not implement
 * Ed25519ctx, and prefix-then-plain-Ed25519 verifies everywhere.
 */

const TEXT = new TextEncoder();

const SIGNING_DOMAIN = 'zesterm-sig-v1';

export type Role = 'host' | 'client';

/**
 * What a signature is *for*. `attach-ticket` is reserved by the Rust today so
 * M4's tickets cannot collide with signatures minted now; carried here for the
 * same reason.
 */
export type Purpose = 'auth' | 'enrollment' | 'attach-ticket';

export function preimage(role: Role, purpose: Purpose, message: Uint8Array): Uint8Array {
  const head = TEXT.encode(`${SIGNING_DOMAIN}\0${role}\0${purpose}\0`);
  const out = new Uint8Array(head.length + message.length);
  out.set(head, 0);
  out.set(message, head.length);
  return out;
}

/**
 * The exact bytes a browser signs to register its device key with the account
 * service — the signing half of
 * `cloud/packages/web/src/enroll/register-preimage.ts`.
 *
 * ```
 * request = "zesterm-register-v1" ++ u16be(len(account)) ++ account
 *                                 ++ key32
 *                                 ++ u16be(len(label)) ++ label
 * ```
 *
 * The signature is `ClientSigner.sign('enrollment', request)` — the signer
 * applies the `zesterm-sig-v1\0client\0enrollment\0` wrap itself, so nothing
 * here touches `preimage`; building the wrap twice is how a request gets
 * double-prefixed and verifies nowhere.
 *
 * `account` is the signed-in user's id, from `/api/bootstrap`. Binding it into
 * the signed bytes is what stops a captured registration replaying under
 * another account's session. **Length-prefixed, not NUL-separated**, because
 * both `account` and `label` are free strings: concatenated, `("ab","cd")` and
 * `("abc","d")` are identical bytes, and the label is chosen by the caller.
 *
 * This workspace cannot import the Worker's encoder — three projects, three
 * lockfiles, `cloud/README.md` says why — so the two hand-built copies are
 * pinned byte-identical by a shared golden vector: `test/register.test.ts`
 * here, `test/register-preimage.test.ts` there. A drift lands as both goldens
 * disagreeing with one side, naming the side that moved.
 */

import { hexToBytes } from '@zesterm/proto';

import type { ClientSigner } from './signer.ts';

const TEXT = new TextEncoder();

const REGISTER_DOMAIN = 'zesterm-register-v1';

function lengthPrefixed(text: string): Uint8Array[] {
  const bytes = TEXT.encode(text);
  if (bytes.length > 0xffff) {
    // The Worker refuses the same way, for the same reason: a preimage that
    // silently drops bytes is a signature over a message nobody meant to send.
    throw new RangeError(`a registration field is at most 65535 bytes, this one is ${bytes.length}`);
  }
  return [new Uint8Array([bytes.length >>> 8, bytes.length & 0xff]), bytes];
}

/** The request a browser signs: `(account, key, label)` under the register domain. */
export function registerRequest(account: string, clientId: string, label: string): Uint8Array {
  // Lowercase-only, matching the rest of this package and — the half that
  // bites — the Worker's `fromHex`, which refuses `deviceId` on shape before
  // it ever verifies. An uppercase id here would build a signature the server
  // rejects every time, with nothing naming the case as the cause.
  if (!/^[0-9a-f]{64}$/.test(clientId)) {
    throw new RangeError(`a client id is 64 lowercase hex characters, got ${clientId.length}`);
  }
  const key = hexToBytes(clientId);
  const parts = [TEXT.encode(REGISTER_DOMAIN), ...lengthPrefixed(account), key, ...lengthPrefixed(label)];
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let at = 0;
  for (const part of parts) {
    out.set(part, at);
    at += part.length;
  }
  return out;
}

/**
 * Sign a registration for this device: 128 hex chars, ready for
 * `POST /api/devices/register`'s `sig`.
 *
 * The key being registered is the one signing — `signer.clientId` goes into
 * the request, so a signer cannot be asked to vouch for some other key's
 * registration.
 */
export function signRegistration(
  signer: ClientSigner,
  account: string,
  label: string,
): Promise<string> {
  return signer.sign('enrollment', registerRequest(account, signer.clientId, label));
}

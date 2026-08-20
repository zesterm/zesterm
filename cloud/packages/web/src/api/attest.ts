/**
 * Attestations: accepting a voucher, and serving the account's set to its
 * daemons.
 *
 * The server's verification here is **hygiene, not authority**: it keeps
 * garbage out of the distribution channel, so a daemon never wastes a
 * handshake on a blob that could not possibly verify. The real trust decision
 * happens at each daemon, which re-verifies the signature over the same bytes
 * and additionally requires `by` in its *own* trust store — that last check
 * is where non-transitivity lives (a chain of attestations is not a path into
 * the fleet), and it deliberately cannot live here: this Worker has no trust
 * store to consult, and pretending to make the decision would teach callers
 * the list is pre-trusted.
 */

import {
  decodeAttestation,
  signingPreimage,
  fromHex,
  ATTESTATION_TTL_MS,
  type DecodedAttestation,
} from '@zesterm/cloud-shared';
import { verifyAsync } from '@noble/ed25519';

import {
  approveDevice as approveDeviceRow,
  listAttestations,
  putAttestation,
  revokedDeviceIds,
} from '../db/attestations.ts';
import type { Db, DeviceStatus } from '../db/types.ts';
import type { Env } from '../env.ts';
import { json, jsonObject } from '../http.ts';
import { KEY_LEN } from '../enroll/preimage.ts';
import { requestPrincipal, unauthorized } from './principal.ts';

/**
 * How far an attestation's `iat` may sit from the server clock, either way.
 *
 * Five minutes, because `iat` is minted by a browser or a desktop app whose
 * clock this Worker does not control, and rejecting honest skew turns "your
 * laptop's clock drifted" into an approval button that silently fails. Any
 * wider and a captured approver key could pre-date vouchers to before its
 * own revocation — the reason the Rust refuses `now < iat` at verify time.
 */
export const ATTESTATION_IAT_SKEW_MS = 5 * 60 * 1000;

/** Verify a decoded attestation over its arrived bytes, against `by`. */
export async function attestationSignatureOk(decoded: DecodedAttestation): Promise<boolean> {
  const by = fromHex(decoded.fields.by, KEY_LEN);
  if (by === null) return false;
  try {
    // `zip215: false` mirrors every other verify on this Worker: the daemons
    // re-verify with dalek's verify_strict, and a blob this side accepts that
    // the fleet refuses is a voucher that "worked" and then did nothing.
    return await verifyAsync(
      decoded.signature,
      signingPreimage('client', 'device-attestation', decoded.message),
      by,
      { zip215: false },
    );
  } catch {
    return false;
  }
}

/** A live device row of this account, or `null`. */
async function liveDevice(
  db: Db,
  id: string,
  userId: string,
): Promise<{ status: DeviceStatus } | null> {
  return db
    .prepare(
      `SELECT status FROM devices WHERE id = ? AND user_id = ? AND revoked_at IS NULL`,
    )
    .bind(id, userId)
    .first<{ status: DeviceStatus }>();
}

/**
 * `POST /api/devices/:id/approve` — a trusted device vouches for `:id`.
 *
 * Two kinds of caller, one rule each. A **person** (cookie) approves from the
 * devices screen, and the attestation names whichever of their approved
 * devices signed it — the browser signs with its own key. A **device**
 * (bearer) is the desktop app approving with its token, and it may only
 * submit vouchers it signed itself: `principal.id` must equal `by`, or a
 * leaked token could launder vouchers under other devices' names.
 *
 * Checks in the claim's order — cheap and self-contained first, the signature
 * last, the write after everything:
 *
 * 1. principal, or 401
 * 2. decode + narrow (size-bounded before any parse)
 * 3. the statement is about this account, this route, and two distinct keys
 * 4. the subject is a live device here; the approver is live AND approved
 * 5. (bearer only) the caller is the approver it claims
 * 6. the window is sane: `exp` after `iat`, no longer than the TTL, `iat`
 *    within clock skew of now
 * 7. the signature verifies over the arrived bytes, against `by`
 * 8. store the blob verbatim; mark the device approved
 */
export async function approveDevice(
  request: Request,
  env: Env,
  id: string,
  now: number,
): Promise<Response> {
  const principal = await requestPrincipal(request, env, now);
  if (principal === null) return unauthorized(request, env, now);
  // A host serves shells; letting its token mint device approvals would make
  // one compromised machine the doorman for every other device.
  if (principal.kind === 'host') return json({ error: 'unauthorized' }, 401);
  const userId = principal.kind === 'user' ? principal.user.id : principal.userId;

  if (fromHex(id, KEY_LEN) === null) return json({ error: 'not_found' }, 404);

  const body = await jsonObject(request);
  const blob = body?.['attestation'];
  if (typeof blob !== 'string') return json({ error: 'bad_request', detail: 'attestation' }, 400);

  const decoded = decodeAttestation(blob);
  if (decoded === null) return json({ error: 'bad_request', detail: 'attestation' }, 400);
  const a = decoded.fields;

  // The account inside the signed bytes must be the caller's: a voucher
  // captured from one account must not admit the same key to another.
  if (a.account !== userId) return json({ error: 'bad_request', detail: 'account' }, 400);
  // The statement must be about the device the route names — accepting a blob
  // about some other key would make the URL decoration and the body the
  // authority, and the two could then disagree silently.
  if (a.device !== id) return json({ error: 'bad_request', detail: 'device' }, 400);
  // Self-vouching is not approval: the whole point of `pending` is that the
  // session alone could not promote a key, and a key vouching for itself is
  // the same act wearing a signature.
  if (a.by === a.device) return json({ error: 'bad_request', detail: 'by' }, 400);

  // Not this account's, or revoked, is the same 404 a revoke answers — the
  // ids are public keys, and this endpoint must not say whether a stranger's
  // key is enrolled anywhere.
  const subject = await liveDevice(env.DB, a.device, userId);
  if (subject === null) return json({ error: 'not_found' }, 404);

  // The approver must itself be approved: a pending device that could vouch
  // would bootstrap a whole family of keys off one unapproved registration.
  const approver = await liveDevice(env.DB, a.by, userId);
  if (approver === null || approver.status !== 'approved') {
    return json({ error: 'bad_request', detail: 'by' }, 400);
  }

  if (principal.kind === 'device' && principal.id !== a.by) {
    // The bearer path speaks only for itself. Without this, any device token
    // could submit vouchers naming any approver whose signature it captured.
    return json({ error: 'forbidden' }, 403);
  }

  if (a.exp <= a.iat || a.exp - a.iat > ATTESTATION_TTL_MS) {
    return json({ error: 'bad_request', detail: 'exp' }, 400);
  }
  if (a.exp <= now) {
    // Reachable despite the skew check: an `iat` a few minutes back with a
    // window of seconds is inside every bound above and already dead. Storing
    // it would mark the device approved on the word of a voucher this account
    // will never serve and no daemon would accept — an approval with nothing
    // behind it.
    return json({ error: 'bad_request', detail: 'exp' }, 400);
  }
  if (Math.abs(a.iat - now) > ATTESTATION_IAT_SKEW_MS) {
    // Freshly minted or refused: this route exists to accept vouchers made
    // for it just now, and a wide-open `iat` would let a long-captured blob
    // be replayed as a new approval.
    return json({ error: 'bad_request', detail: 'iat' }, 400);
  }

  if (!(await attestationSignatureOk(decoded))) {
    return json({ error: 'bad_signature' }, 400);
  }

  await putAttestation(env.DB, {
    deviceId: a.device,
    byDevice: a.by,
    userId,
    blob,
    iat: a.iat,
    exp: a.exp,
  });
  const device = await approveDeviceRow(env.DB, a.device, userId, a.by, now);
  if (device === null) {
    // The subject was revoked between the check above and the write. The
    // attestation row it left behind is revoked by the same revoke that won
    // the race — `revokeAttestationsFor` runs on every device revoke.
    return json({ error: 'not_found' }, 404);
  }

  return json({ device });
}

/**
 * `GET /api/attestations` — the account's vouchers and its revocation list,
 * for daemons.
 *
 * People (cookie) and **hosts** (bearer) read it: a daemon pulls the set to
 * learn which client keys its account vouches for, and the browser has no
 * use for the blobs — it learns approval state from `/api/devices`. Device
 * tokens are refused for that reason: serving them would only widen what a
 * leaked browser token can enumerate.
 *
 * The whole list every time, no deltas: an account's fleet is a handful of
 * devices, so the delta protocol would cost more in "these two disagree"
 * than the bytes it saves.
 */
export async function listAccountAttestations(
  request: Request,
  env: Env,
  now: number,
): Promise<Response> {
  const principal = await requestPrincipal(request, env, now);
  if (principal === null) return unauthorized(request, env, now);
  if (principal.kind === 'device') return json({ error: 'unauthorized' }, 401);
  const userId = principal.kind === 'user' ? principal.user.id : principal.userId;

  return json({
    attestations: await listAttestations(env.DB, userId, now),
    revoked: await revokedDeviceIds(env.DB, userId),
  });
}

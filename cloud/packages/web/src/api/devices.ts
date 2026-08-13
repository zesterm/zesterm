/**
 * Device registration: a signed-in browser puts its own public key into
 * `devices`, as `pending`.
 *
 * The third way into the registry, and deliberately the weakest. Code
 * enrolment is a person's explicit act, so its devices are born approved;
 * registration is a browser acting on nothing but the session it already
 * holds, so its rows are born `pending` — listed, revocable, and refused by
 * the token resolver until something stronger approves them (a later PR's
 * attestations). What the signature proves is narrower than it looks:
 * possession of the key, bound to this account. It does not prove the
 * *account holder* wanted this key registered — any script running with the
 * session could have asked — which is exactly why pending exists.
 *
 * **Full CSRF posture, never `ORIGINLESS`, never bearer.** This route reads
 * the session cookie, so it keeps both halves of the rule — a route that took
 * the cookie and an exemption would be the hole the router's `ORIGINLESS`
 * comment warns about. And a bearer principal must not reach it: a device
 * that could register *other* devices would make one leaked token a family
 * of keys.
 */

import { fromHex, hex } from '@zesterm/cloud-shared';

import { countDevices, existingDevice, registerDevice as registerDeviceRow } from '../db/registry.ts';
import type { DeviceKind } from '../db/types.ts';
import type { Env } from '../env.ts';
import { json, jsonObject } from '../http.ts';
import { KEY_LEN, SIGNATURE_LEN } from '../enroll/preimage.ts';
import { verifyRegistration } from '../enroll/register-preimage.ts';
import { DEVICE_KINDS, labelOk } from './enroll.ts';
import { currentUser } from './session.ts';

/**
 * The lid on unapproved rows per account. Registration is the one write a
 * session performs without a person's explicit act, so left unbounded it is a
 * table any script with the session can grow forever. Thirty-two is far past
 * any honest household of browsers and small enough that the devices screen
 * stays readable while the owner cleans up.
 */
export const MAX_PENDING_DEVICES = 32;

/**
 * `POST /api/devices/register` — the session's browser answers for its own key.
 *
 * The order is the claim's discipline, for the claim's reasons:
 *
 * 1. **Session**, first — everything after speaks about a specific account.
 * 2. **Shape**, so junk costs no database round trip.
 * 3. **The signature verifies** — before any read of the registry, so this
 *    endpoint never says anything about a key its caller has not proven.
 * 4. **Incumbent checks** — another account's key or a revoked one is 409,
 *    exactly as the claim answers; the same account's live key is idempotent.
 * 5. **Bound, then write.**
 */
export async function registerDevice(request: Request, env: Env, now: number): Promise<Response> {
  const user = await currentUser(request, env, now);
  if (user === null) return json({ error: 'unauthorized' }, 401);

  const body = await jsonObject(request);
  if (body === null) return json({ error: 'bad_request' }, 400);

  const { deviceId, label, kind, extractable, sig } = body as {
    deviceId?: unknown;
    label?: unknown;
    kind?: unknown;
    extractable?: unknown;
    sig?: unknown;
  };

  // Lowercase-only via `fromHex`, because `devices.id` is stored as `hex()`
  // writes it and an uppercase spelling of a key is a different primary key.
  const key = typeof deviceId === 'string' ? fromHex(deviceId, KEY_LEN) : null;
  if (key === null) return json({ error: 'bad_request', detail: 'deviceId' }, 400);

  if (typeof label !== 'string' || !labelOk(label)) {
    return json({ error: 'bad_request', detail: 'label' }, 400);
  }
  // `browser` as the default rather than a required field: this route exists
  // for browsers, and the other kinds only pass through here if a phone or
  // desktop app ever grows a cookie session of its own.
  if (kind !== undefined && !DEVICE_KINDS.includes(kind as DeviceKind)) {
    return json({ error: 'bad_request', detail: 'kind' }, 400);
  }
  if (extractable !== undefined && typeof extractable !== 'boolean') {
    return json({ error: 'bad_request', detail: 'extractable' }, 400);
  }
  const signature = typeof sig === 'string' ? fromHex(sig, SIGNATURE_LEN) : null;
  if (signature === null) return json({ error: 'bad_request', detail: 'sig' }, 400);

  // `account` in the signed bytes is this session's user id — never a field of
  // the request — so a registration captured off another account's wire is a
  // signature over the wrong account here, and dies now. Named back to the
  // caller, unlike the claim's collapsed `invalid_code`: this caller is
  // authenticated and the key is its own, so there is no oracle to starve.
  if (!(await verifyRegistration({ account: user.id, key, label, signature }))) {
    return json({ error: 'bad_signature' }, 400);
  }

  const id = hex(key);
  const incumbent = await existingDevice(env.DB, id);
  if (incumbent !== null && (incumbent.user_id !== user.id || incumbent.revoked_at !== null)) {
    // The claim's two-situations-one-answer: another account's key is not this
    // caller's business, and a revoked key that could re-register has
    // un-revoked itself — un-revoking is an act by the owner, not by the key.
    return json({ error: 'already_enrolled' }, 409);
  }

  if (incumbent === null) {
    const counts = await countDevices(env.DB, user.id);
    // Only a row that would be *born pending* counts against the lid — an
    // account bootstrapping its first device adds nothing unapproved. Checked
    // here rather than inside the insert: a concurrent burst can overshoot by
    // its own width, which costs one extra row, not an invariant.
    if (counts.approved > 0 && counts.pending >= MAX_PENDING_DEVICES) {
      return json({ error: 'too_many_pending' }, 429);
    }
  }

  // The bootstrap rule lives in the insert's CASE (see `registerDeviceRow`):
  // an account with ZERO live approved devices gets this row born `approved`,
  // `approved_by` NULL — approved by dint of the session that registered it.
  // That grants no shell anywhere: no attestation exists for this key, no
  // daemon's trust store knows it, and ADR-009's first-device-pairs-at-the-
  // machine still stands — the row only makes the account's *list* usable, so
  // its first browser is not pending forever with no incumbent to approve it.
  const device = await registerDeviceRow(env.DB, {
    id,
    userId: user.id,
    label,
    kind: (kind as DeviceKind | undefined) ?? 'browser',
    // Absent reads as extractable — the cautious direction, matching the
    // schema's default: over-warning beats a green tick that is not true.
    extractable: (extractable as boolean | undefined) ?? true,
    now,
  });

  if (device === null) {
    // The upsert's own guard fired between the check above and the write —
    // the owner revoked the key in that window. Same answer as finding it
    // revoked up front, because it is the same fact a moment later.
    return json({ error: 'already_enrolled' }, 409);
  }

  // The listing's envelope shape. No token: registration hands out no
  // credential — a pending device holds nothing, and even the bootstrap row
  // gets a token only through paths that already exist for approved devices.
  return json({ device });
}

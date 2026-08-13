/**
 * The attestation store: keeping verified vouchers, serving live ones, and
 * revoking them alongside their devices.
 *
 * The two rules from `registry.ts` run through here too: ownership is part of
 * every `WHERE` clause, and anything that must be atomic lives inside one
 * statement. What is deliberately absent is any verification — a blob reaches
 * `putAttestation` only after the approve route verified it over the arrived
 * bytes, and it leaves `listAttestations` verbatim for the daemons to verify
 * again themselves.
 */

import { publicDevice, type Db, type DeviceRow, type PublicDevice } from './types.ts';

/**
 * Store a verified attestation, replacing this approver's earlier statement
 * about this device if one exists.
 *
 * The `ON CONFLICT` target is the primary key `(device_id, by_device)`, so
 * renewal — the same approver vouching again — replaces rather than
 * accumulates, and `revoked_at` is reset to NULL: the new approval is a new
 * statement, made now, by a caller the route has just re-checked is a live
 * approved device. A row revoked because its *subject* was revoked cannot
 * come back this way, because a revoked device fails the route's incumbent
 * check long before the write.
 */
export async function putAttestation(
  db: Db,
  args: {
    deviceId: string;
    byDevice: string;
    userId: string;
    blob: string;
    iat: number;
    exp: number;
  },
): Promise<void> {
  await db
    .prepare(
      `INSERT INTO device_attestations (device_id, by_device, user_id, blob, iat, exp)
       VALUES (?, ?, ?, ?, ?, ?)
       ON CONFLICT(device_id, by_device) DO UPDATE
         SET blob = excluded.blob, iat = excluded.iat, exp = excluded.exp, revoked_at = NULL
         WHERE device_attestations.user_id = excluded.user_id`,
    )
    .bind(args.deviceId, args.byDevice, args.userId, args.blob, args.iat, args.exp)
    .run();
}

/**
 * The account's live vouchers, verbatim, oldest first.
 *
 * Unexpired and unrevoked only: an expired attestation is dead by its own
 * signed `exp`, and serving it would just make every daemon verify and refuse
 * the same bytes. The whole list, no deltas — an account's fleet is a handful
 * of devices, so the honest cost of re-sending everything is bytes nobody
 * will miss, and a delta protocol is a second copy of "what changed" that can
 * disagree with the table.
 */
export async function listAttestations(db: Db, userId: string, now: number): Promise<string[]> {
  const { results } = await db
    .prepare(
      `SELECT blob FROM device_attestations
        WHERE user_id = ? AND revoked_at IS NULL AND exp > ?
        ORDER BY iat, device_id`,
    )
    .bind(userId, now)
    .all<{ blob: string }>();
  return results.map((r) => r.blob);
}

/**
 * The account's revocation list: every device id whose row is revoked.
 *
 * Served beside the blobs because Model B needs both halves: an attestation
 * is long-lived, so the list is what un-introduces a device from daemons that
 * already recorded it. Ids only — a daemon holds keys, not labels.
 */
export async function revokedDeviceIds(db: Db, userId: string): Promise<string[]> {
  const { results } = await db
    .prepare(
      `SELECT id FROM devices
        WHERE user_id = ? AND revoked_at IS NOT NULL
        ORDER BY revoked_at, id`,
    )
    .bind(userId)
    .all<{ id: string }>();
  return results.map((r) => r.id);
}

/**
 * Revoke every attestation a device appears in — as subject AND as approver.
 *
 * Both sides on purpose: revoking a device must stop the account serving
 * vouchers *for* it, and also the vouchers *by* it — an approver that has
 * been thrown out of the account must not keep introducing devices from
 * beyond the grave. `COALESCE` keeps the first revocation's timestamp, as
 * `revokeDevice` does and for the same reason.
 */
export async function revokeAttestationsFor(
  db: Db,
  deviceId: string,
  userId: string,
  now: number,
): Promise<void> {
  await db
    .prepare(
      `UPDATE device_attestations SET revoked_at = COALESCE(revoked_at, ?)
        WHERE user_id = ? AND (device_id = ? OR by_device = ?)`,
    )
    .bind(now, userId, deviceId, deviceId)
    .run();
}

/**
 * Mark a device approved, recording who and when — but only the *first* time.
 *
 * `COALESCE` on both columns: `approved_at`/`approved_by` answer "when did
 * this stop being pending, and on whose word", and a later re-vouch is a new
 * attestation row, not a rewrite of that history. `status` is set
 * unconditionally, which is a no-op for an already-approved row. `null` means
 * no live row under this account — revoked or absent, the same answer as
 * everywhere else in this schema.
 */
export async function approveDevice(
  db: Db,
  id: string,
  userId: string,
  byDevice: string,
  now: number,
): Promise<PublicDevice | null> {
  const row = await db
    .prepare(
      `UPDATE devices
          SET status = 'approved',
              approved_at = COALESCE(approved_at, ?),
              approved_by = COALESCE(approved_by, ?)
        WHERE id = ? AND user_id = ? AND revoked_at IS NULL
       RETURNING *`,
    )
    .bind(now, byDevice, id, userId)
    .first<DeviceRow>();
  return row === null ? null : publicDevice(row);
}

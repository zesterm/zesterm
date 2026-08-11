/**
 * The device registry: enrolment codes, and the machines and browsers an
 * account owns.
 *
 * Two rules run through every query here, and both are enforced in SQL rather
 * than in the caller:
 *
 * - **Ownership is part of the `WHERE` clause.** Never "read the row, then
 *   check `user_id`" — that is the same code with the check somewhere it can be
 *   forgotten, and a forgotten one is silent.
 * - **Spending is a compare-and-set, not a read followed by a write.** D1 gives
 *   no transaction across two `prepare` calls, so the guard has to live inside
 *   the single statement that changes the row. `UPDATE … WHERE used_at IS NULL
 *   … RETURNING` is atomic in SQLite; two concurrent claims therefore see one
 *   winner and one empty result, rather than two winners.
 */

import { newEnrollCode, ENROLL_CODE_TTL_MS } from '../enroll/codes.ts';
import {
  publicDevice,
  publicHost,
  type Db,
  type DeviceKind,
  type DeviceRow,
  type EnrollCodeRow,
  type EnrollKind,
  type HostRow,
  type PublicDevice,
  type PublicHost,
} from './types.ts';

export interface MintedCode {
  readonly code: string;
  readonly expiresAt: number;
}

/** Mint a one-shot code for `userId`. The only place a code is created. */
export async function createEnrollCode(
  db: Db,
  userId: string,
  kind: EnrollKind,
  now: number,
): Promise<MintedCode> {
  let code = newEnrollCode();
  const expiresAt = now + ENROLL_CODE_TTL_MS;

  // `code` is the primary key, and spent codes are kept rather than deleted --
  // deliberately, so "why can this not enrol" has an answer. That means the
  // space fills, slowly, and a collision is an `INSERT` that throws. Left
  // alone it surfaces as a 500 on a request that did nothing wrong.
  //
  // Retried rather than made collision-proof: at ~39 bits a second attempt is
  // overwhelmingly likely to land, and a longer code is paid for by the person
  // retyping it. Three attempts, then the failure is real and propagates.
  for (let attempt = 0; ; attempt++) {
    try {
      await db
        .prepare(
          `INSERT INTO enroll_codes (code, user_id, kind, created_at, expires_at)
           VALUES (?, ?, ?, ?, ?)`,
        )
        .bind(code, userId, kind, now, expiresAt)
        .run();
      return { code, expiresAt };
    } catch (e) {
      if (attempt >= 2) throw e;
      code = newEnrollCode();
    }
  }
}

/**
 * A live, unspent code — or `null`, whatever the reason.
 *
 * Expired, already used and never issued all collapse here on purpose. The
 * distinction is kept in the *database* (`used_at` is set, the row is not
 * deleted) so that "why can this not enrol" has an answer for whoever looks;
 * it is not offered to the caller, because a caller holding the two apart
 * ends up reporting which, and "that code existed and you were a minute late"
 * is worth more to whoever is guessing than to the person who mistyped.
 */
export async function findLiveEnrollCode(
  db: Db,
  code: string,
  now: number,
): Promise<EnrollCodeRow | null> {
  const row = await db
    .prepare(`SELECT * FROM enroll_codes WHERE code = ?`)
    .bind(code)
    .first<EnrollCodeRow>();
  if (row === null) return null;
  if (row.used_at !== null) return null;
  if (row.expires_at <= now) return null;
  return row;
}

/**
 * Spend a code, atomically, and say whether this caller is the one who did.
 *
 * The `WHERE` is the whole guarantee: a replay of a claim that already
 * succeeded matches no row, so it enrols nothing. `null` therefore means "lost
 * the race, or it was never live", and the caller must not proceed.
 */
export async function spendEnrollCode(
  db: Db,
  code: string,
  usedBy: string,
  now: number,
): Promise<EnrollKind | null> {
  const row = await db
    .prepare(
      `UPDATE enroll_codes SET used_at = ?, used_by = ?
        WHERE code = ? AND used_at IS NULL AND expires_at > ?
       RETURNING kind`,
    )
    .bind(now, usedBy, code, now)
    .first<{ kind: EnrollKind }>();
  return row === null ? null : row.kind;
}

/** What an existing registry row says, when deciding whether a key may enrol. */
export interface Incumbent {
  readonly user_id: string;
  readonly revoked_at: number | null;
}

export async function existingHost(db: Db, id: string): Promise<Incumbent | null> {
  return db
    .prepare(`SELECT user_id, revoked_at FROM hosts WHERE id = ?`)
    .bind(id)
    .first<Incumbent>();
}

export async function existingDevice(db: Db, id: string): Promise<Incumbent | null> {
  return db
    .prepare(`SELECT user_id, revoked_at FROM devices WHERE id = ?`)
    .bind(id)
    .first<Incumbent>();
}

/**
 * Enrol a host, or re-label one this account already owns.
 *
 * `hosts.id` *is* the public key (ADR-006), so a machine enrolling a second
 * time collides with itself rather than appearing twice — which is right: it is
 * the same machine, and it has just re-proved that it holds the key. The
 * conflict branch therefore updates the label and leaves `enrolled_at` alone,
 * so "yours since" keeps meaning the first time.
 *
 * The two conditions on `DO UPDATE` are the ones the schema's own comment
 * demands. Another account's key is refused because possession of a key is not
 * a claim on someone else's row; a **revoked** key is refused because
 * revocation is a positive statement, and a revoked machine that could re-enrol
 * itself with a fresh code has un-revoked itself. `RETURNING` yields nothing when
 * the branch is skipped, so a refusal arrives as `null` rather than as a silent
 * success.
 */
export async function enrolHost(
  db: Db,
  args: { id: string; userId: string; label: string; platform: string; now: number },
): Promise<PublicHost | null> {
  const row = await db
    .prepare(
      `INSERT INTO hosts (id, user_id, label, platform, enrolled_at)
       VALUES (?, ?, ?, ?, ?)
       ON CONFLICT(id) DO UPDATE SET label = excluded.label, platform = excluded.platform
        WHERE hosts.user_id = excluded.user_id AND hosts.revoked_at IS NULL
       RETURNING *`,
    )
    .bind(args.id, args.userId, args.label, args.platform, args.now)
    .first<HostRow>();
  return row === null ? null : publicHost(row);
}

/** The same shape for a browser, phone or desktop app. See `enrolHost`. */
export async function enrolDevice(
  db: Db,
  args: { id: string; userId: string; label: string; kind: DeviceKind; now: number },
): Promise<PublicDevice | null> {
  const row = await db
    .prepare(
      `INSERT INTO devices (id, user_id, label, kind, enrolled_at)
       VALUES (?, ?, ?, ?, ?)
       ON CONFLICT(id) DO UPDATE SET label = excluded.label, kind = excluded.kind
        WHERE devices.user_id = excluded.user_id AND devices.revoked_at IS NULL
       RETURNING *`,
    )
    .bind(args.id, args.userId, args.label, args.kind, args.now)
    .first<DeviceRow>();
  return row === null ? null : publicDevice(row);
}

export async function listHosts(db: Db, userId: string): Promise<PublicHost[]> {
  const { results } = await db
    .prepare(
      `SELECT * FROM hosts
        WHERE user_id = ? AND revoked_at IS NULL
        ORDER BY enrolled_at, id`,
    )
    .bind(userId)
    .all<HostRow>();
  return results.map(publicHost);
}

export async function listDevices(db: Db, userId: string): Promise<PublicDevice[]> {
  const { results } = await db
    .prepare(
      `SELECT * FROM devices
        WHERE user_id = ? AND revoked_at IS NULL
        ORDER BY enrolled_at, id`,
    )
    .bind(userId)
    .all<DeviceRow>();
  return results.map(publicDevice);
}

/**
 * Is this machine one this account may attach to right now?
 *
 * All three conditions are in the `WHERE` clause, for the reason at the top of
 * this file: "read the row, then check `user_id`" is the same code with the
 * check somewhere it can be forgotten. It answers a boolean rather than the
 * row, because the caller mints a ticket naming the id it already has — a row
 * in hand is a row a future edit could be tempted to copy a field out of, and
 * every one of those fields is something the relay would then be told.
 */
export async function ownsLiveHost(db: Db, id: string, userId: string): Promise<boolean> {
  const row = await db
    .prepare(`SELECT 1 AS ok FROM hosts WHERE id = ? AND user_id = ? AND revoked_at IS NULL`)
    .bind(id, userId)
    .first<{ ok: number }>();
  return row !== null;
}

/**
 * Revoke, idempotently, and answer with *when* — which is the fact anyone
 * asking later actually wants.
 *
 * `COALESCE` rather than an unconditional set, so revoking twice does not move
 * the timestamp forward: the first statement is the one that was true, and a
 * second click on a slow page must not rewrite it. `null` means no such row
 * under this account — which is also what another account's machine looks like,
 * deliberately, because the alternative is an endpoint that answers "does this
 * key exist" for keys that are none of the caller's business.
 */
export async function revokeHost(
  db: Db,
  id: string,
  userId: string,
  now: number,
): Promise<number | null> {
  const row = await db
    .prepare(
      `UPDATE hosts SET revoked_at = COALESCE(revoked_at, ?)
        WHERE id = ? AND user_id = ?
       RETURNING revoked_at`,
    )
    .bind(now, id, userId)
    .first<{ revoked_at: number }>();
  return row === null ? null : row.revoked_at;
}

export async function revokeDevice(
  db: Db,
  id: string,
  userId: string,
  now: number,
): Promise<number | null> {
  const row = await db
    .prepare(
      `UPDATE devices SET revoked_at = COALESCE(revoked_at, ?)
        WHERE id = ? AND user_id = ?
       RETURNING revoked_at`,
    )
    .bind(now, id, userId)
    .first<{ revoked_at: number }>();
  return row === null ? null : row.revoked_at;
}

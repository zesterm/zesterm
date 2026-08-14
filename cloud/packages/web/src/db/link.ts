/**
 * The link-grant store: parking a proven key's ask, and spending the answer.
 *
 * `registry.ts`'s two rules run through here with one twist. Ownership cannot
 * be in every `WHERE` clause because the row is *pre-account* — the account
 * is decided by whoever approves — so the id itself carries the machine-token
 * entropy, and everything that changes a row is a compare-and-set inside one
 * statement, because D1 gives no transaction across two `prepare` calls.
 */

import { randomBytes, toBase64Url } from '@zesterm/cloud-shared';

import type { Db, DeviceKind } from './types.ts';

/**
 * Ten minutes — enough to sign in through OAuth on the way to the approval
 * page, short enough that a grant id captured from a screen recording is
 * stale by the time anyone types it back in. The app polls inside this
 * window; a grant that outlives it is asked for again with one click.
 */
export const LINK_GRANT_TTL_MS = 10 * 60 * 1000;

/** `base64url(32 random bytes)` — 43 chars, the machine-token entropy. */
export function newLinkGrantId(): string {
  return toBase64Url(randomBytes(32));
}

/**
 * Shape-only: is this string even a grant id? 43 base64url chars, so junk is
 * refused before it costs a database round trip — the `looksLike*` discipline.
 */
export function looksLikeLinkGrant(text: string): boolean {
  return /^[A-Za-z0-9_-]{43}$/.test(text);
}

export interface LinkGrantRow {
  readonly id: string;
  readonly device_id: string;
  readonly label: string;
  readonly kind: DeviceKind;
  readonly platform: string;
  readonly created_at: number;
  readonly expires_at: number;
  readonly approved_at: number | null;
  readonly approved_by_user: string | null;
  readonly claimed_at: number | null;
}

/**
 * Park a fresh grant for a key, replacing any it already had.
 *
 * The conflict target is `UNIQUE(device_id)`, so one statement enforces the
 * machine-token discipline: asking again *rotates* — new id, fresh window,
 * approval and claim state wiped — rather than accumulating rows an
 * unauthenticated caller could farm. Wiping `approved_at` on replace is not
 * incidental: a fresh ask must not inherit an approval a person gave the
 * previous one.
 */
export async function startLinkGrant(
  db: Db,
  args: {
    id: string;
    deviceId: string;
    label: string;
    kind: DeviceKind;
    platform: string;
    now: number;
  },
): Promise<void> {
  await db
    .prepare(
      `INSERT INTO link_grants (id, device_id, label, kind, platform, created_at, expires_at)
       VALUES (?, ?, ?, ?, ?, ?, ?)
       ON CONFLICT(device_id) DO UPDATE SET
         id = excluded.id, label = excluded.label, kind = excluded.kind,
         platform = excluded.platform, created_at = excluded.created_at,
         expires_at = excluded.expires_at,
         approved_at = NULL, approved_by_user = NULL, claimed_at = NULL`,
    )
    .bind(args.id, args.deviceId, args.label, args.kind, args.platform, args.now, args.now + LINK_GRANT_TTL_MS)
    .run();
}

/**
 * A grant that is still spendable or approvable — or `null`, whatever the
 * reason. Unknown, expired and already-claimed collapse here on purpose, the
 * `findLiveEnrollCode` discipline: the caller must not be able to report
 * which, because a grant id is briefly a capability and a liveness oracle is
 * a free search for one.
 */
export async function findLiveLinkGrant(db: Db, id: string, now: number): Promise<LinkGrantRow | null> {
  const row = await db.prepare(`SELECT * FROM link_grants WHERE id = ?`).bind(id).first<LinkGrantRow>();
  if (row === null) return null;
  if (row.claimed_at !== null) return null;
  if (row.expires_at <= now) return null;
  return row;
}

/**
 * Stamp the approval, idempotently, into the approver's account.
 *
 * `COALESCE` on both columns, the `revokeHost` discipline: a second click on
 * a slow page must not move the moment of approval — nor, worse, move the
 * grant to a *different* account when two sessions race. The first approval
 * is the one that was true. `null` means no live grant: unknown, expired and
 * claimed are one answer, as everywhere else.
 */
export async function approveLinkGrant(
  db: Db,
  id: string,
  userId: string,
  now: number,
): Promise<{ approved_at: number } | null> {
  return db
    .prepare(
      `UPDATE link_grants
          SET approved_at = COALESCE(approved_at, ?),
              approved_by_user = COALESCE(approved_by_user, ?)
        WHERE id = ? AND claimed_at IS NULL AND expires_at > ?
       RETURNING approved_at`,
    )
    .bind(now, userId, id, now)
    .first<{ approved_at: number }>();
}

/**
 * Deny by deletion. A denied grant has no history worth keeping — the device
 * it names was never enrolled — and the deleted row frees the key to ask
 * again. `false` means there was nothing live to deny.
 */
export async function denyLinkGrant(db: Db, id: string, now: number): Promise<boolean> {
  const row = await db
    .prepare(`DELETE FROM link_grants WHERE id = ? AND claimed_at IS NULL AND expires_at > ? RETURNING id`)
    .bind(id, now)
    .first<{ id: string }>();
  return row !== null;
}

/**
 * Spend an approved grant, atomically, and say who approved it.
 *
 * The `WHERE` is the whole guarantee, `spendEnrollCode`'s shape: approved,
 * unclaimed, unexpired, and naming exactly this device — so a replay matches
 * no row, and two concurrent claims see one winner. `null` means "did not
 * spend", for every reason at once; the caller answering `pending` decides
 * that *before* calling this, off a read.
 */
export async function claimLinkGrant(
  db: Db,
  id: string,
  deviceId: string,
  now: number,
): Promise<{ approved_by_user: string } | null> {
  return db
    .prepare(
      `UPDATE link_grants SET claimed_at = ?
        WHERE id = ? AND device_id = ? AND approved_at IS NOT NULL
          AND claimed_at IS NULL AND expires_at > ?
       RETURNING approved_by_user`,
    )
    .bind(now, id, deviceId, now)
    .first<{ approved_by_user: string }>();
}

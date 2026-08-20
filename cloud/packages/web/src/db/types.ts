/**
 * The D1 surface this Worker uses, declared rather than imported.
 *
 * `@cloudflare/workers-types` has `D1Database`, but naming a structural subset
 * here buys the thing that matters: the store's tests run against a plain
 * object under `node --test`, with no workerd and no miniflare. Security code
 * that can only be exercised by deploying is security code that is exercised
 * rarely.
 *
 * It is deliberately the *narrow* subset. If a future query needs `batch` or
 * `exec`, adding it here is a visible decision rather than a discovery.
 */

import { controlLinkIsLive } from '@zesterm/cloud-shared';

export interface D1Result<T> {
  readonly results: T[];
}

export interface D1PreparedStatement {
  bind(...values: unknown[]): D1PreparedStatement;
  first<T>(): Promise<T | null>;
  run(): Promise<unknown>;
  all<T>(): Promise<D1Result<T>>;
}

export interface Db {
  prepare(query: string): D1PreparedStatement;
}

// --- rows ------------------------------------------------------------------

export interface UserRow {
  readonly id: string;
  readonly email: string | null;
  readonly display_name: string;
  readonly avatar_url: string | null;
  readonly created_at: number;
  readonly updated_at: number;
  readonly disabled_at: number | null;
}

export interface SessionRow {
  readonly id: string;
  readonly user_id: string;
  readonly created_at: number;
  readonly last_seen_at: number;
  readonly expires_at: number;
  readonly revoked_at: number | null;
}

/** What the app is told about the signed-in person. Never the row verbatim. */
export interface PublicUser {
  readonly id: string;
  readonly displayName: string;
  readonly email: string | null;
  readonly avatarUrl: string | null;
}

export function publicUser(row: UserRow): PublicUser {
  // Field by field rather than a spread: `users` will grow columns, and a
  // spread would ship each new one to the browser by default. The safe
  // direction for that mistake is "a field is missing", not "a field leaked".
  return {
    id: row.id,
    displayName: row.display_name,
    email: row.email,
    avatarUrl: row.avatar_url,
  };
}

// --- the device registry ---------------------------------------------------

/** Which table a code enrols into, and which role its signature must carry. */
export type EnrollKind = 'host' | 'device';

/** `devices.kind`. Closed, because the devices screen renders an icon per value. */
export type DeviceKind = 'browser' | 'phone' | 'desktop';

/**
 * `devices.status`. `pending` is a registered key awaiting approval — listed
 * and revocable, but its bearer token resolves to nothing and no attestation
 * names it. There is no 'denied': denying is the existing revoke, which is
 * what keeps a denied key from registering again (migration 0004).
 */
export type DeviceStatus = 'pending' | 'approved';

export interface EnrollCodeRow {
  readonly code: string;
  readonly user_id: string;
  readonly kind: EnrollKind;
  readonly created_at: number;
  readonly expires_at: number;
  readonly used_at: number | null;
  readonly used_by: string | null;
}

export interface HostRow {
  readonly id: string;
  readonly user_id: string;
  readonly label: string;
  readonly platform: string;
  readonly enrolled_at: number;
  readonly last_seen_at: number | null;
  /**
   * When the relay last had proof this machine's control link was parked, or
   * `null` for one that has never dialled in — or whose link has gone.
   *
   * Written only by the relay Worker (`room/presence.ts`), read only here, and
   * never handed to a client raw: the bound it is read against is the server's
   * business, so `publicHost` answers a boolean. → migration 0007, #237.
   */
  readonly control_seen_at: number | null;
  readonly revoked_at: number | null;
}

export interface DeviceRow {
  readonly id: string;
  readonly user_id: string;
  readonly label: string;
  readonly kind: DeviceKind;
  readonly extractable: number;
  readonly status: DeviceStatus;
  readonly approved_at: number | null;
  readonly approved_by: string | null;
  readonly enrolled_at: number;
  readonly last_seen_at: number | null;
  readonly revoked_at: number | null;
}

/**
 * What the app is told about a machine. Never the row verbatim — `user_id` in
 * particular is a fact about the account, and the caller already knows it.
 */
export interface PublicHost {
  readonly id: string;
  readonly label: string;
  readonly platform: string;
  readonly enrolledAt: number;
  readonly lastSeenAt: number | null;
  /**
   * Can this machine be reached through the relay right now?
   *
   * A boolean rather than the timestamp behind it, deliberately: how stale a
   * parked link may be before it stops counting is a fact about the relay's
   * refresh cadence, and a client that had to know it is a client that will
   * eventually disagree with the server about what "online" means. #237 is
   * what happens when a screen has to guess this — it guessed "asleep" for a
   * machine that opens a shell instantly.
   *
   * `false` also covers "no relay in this deployment", which is right: the
   * fleet screen's other sources (mDNS on the LAN) are what speak for those.
   */
  readonly online: boolean;
  /**
   * Present only in the owner's `include=revoked` view (`null` there for a
   * live row), absent everywhere else — so the default listing and every
   * machine-bearer listing keep the shape they always had. When it is set the
   * row is history the owner can act on (restore), never a machine to launch.
   */
  readonly revokedAt?: number | null;
}

export interface PublicDevice {
  readonly id: string;
  readonly label: string;
  readonly kind: DeviceKind;
  /**
   * Whether the private key can be read by script on the origin. Reported
   * rather than assumed, so the devices screen can say which instead of
   * showing a green tick that is not true.
   */
  readonly extractable: boolean;
  /**
   * `pending` until the account approves this key. The screen renders the
   * difference, because a pending device that looks approved is a device
   * whose owner never learns it is waiting.
   */
  readonly status: DeviceStatus;
  readonly enrolledAt: number;
  readonly lastSeenAt: number | null;
  /** Same contract as `PublicHost.revokedAt`: the owner's revoked view only. */
  readonly revokedAt?: number | null;
}

// Field by field, for the reason `publicUser` is: both tables will grow
// columns, `do_id` is already one of them, and a spread ships each new one to
// the browser by default.
// `now` because one of the fields is a *verdict* rather than a column: the
// freshness of `control_seen_at` is only meaningful against a clock, and
// resolving it here means every caller answers the same question the same way.
export function publicHost(row: HostRow, now: number): PublicHost {
  return {
    id: row.id,
    label: row.label,
    platform: row.platform,
    enrolledAt: row.enrolled_at,
    lastSeenAt: row.last_seen_at,
    online: controlLinkIsLive(row.control_seen_at, now),
  };
}

export function publicDevice(row: DeviceRow): PublicDevice {
  return {
    id: row.id,
    label: row.label,
    kind: row.kind,
    extractable: row.extractable !== 0,
    status: row.status,
    enrolledAt: row.enrolled_at,
    lastSeenAt: row.last_seen_at,
  };
}

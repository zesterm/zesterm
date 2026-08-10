/**
 * Sessions: creating one, resolving one, and ending one.
 *
 * The token never reaches this file's storage. `sessions.id` is
 * `sha256(token)`, so what is written down cannot be replayed as a cookie —
 * see `@zesterm/cloud-shared`'s `tokens.ts` for why that is worth one hash per
 * request.
 */

import { looksLikeSessionToken, newSessionToken, sessionIdOf } from '@zesterm/cloud-shared';

import { publicUser, type Db, type PublicUser, type SessionRow, type UserRow } from './types.ts';

/** Thirty days. Long enough not to be a nuisance, short enough to be a bound. */
export const SESSION_TTL_MS = 30 * 24 * 60 * 60 * 1000;

/**
 * How stale `last_seen_at` is allowed to get.
 *
 * Sliding expiry is worth having, but a write per request turns every page
 * load into a D1 round trip nothing reads. An hour's granularity is invisible
 * to a person and removes essentially all of that cost.
 */
export const LAST_SEEN_GRANULARITY_MS = 60 * 60 * 1000;

export interface NewSession {
  /** The value for the cookie. Returned once, and never stored. */
  readonly token: string;
  readonly expiresAt: number;
}

export interface SessionContext {
  readonly userAgent?: string | null;
  readonly country?: string | null;
}

export async function createSession(
  db: Db,
  userId: string,
  now: number,
  ctx: SessionContext = {},
): Promise<NewSession> {
  const token = newSessionToken();
  const id = await sessionIdOf(token);
  const expiresAt = now + SESSION_TTL_MS;

  await db
    .prepare(
      `INSERT INTO sessions (id, user_id, created_at, last_seen_at, expires_at, user_agent, country)
       VALUES (?, ?, ?, ?, ?, ?, ?)`,
    )
    .bind(id, userId, now, now, expiresAt, ctx.userAgent ?? null, ctx.country ?? null)
    .run();

  return { token, expiresAt };
}

/**
 * The user behind a cookie, or `null`.
 *
 * Every rejection returns the same `null`: expired, revoked, unknown, disabled
 * account, malformed token. A caller that could tell them apart would be
 * tempted to say which — and "this token existed but expired" is a fact worth
 * more to whoever is guessing than to the person who is simply logged out.
 */
export async function resolveSession(
  db: Db,
  token: string | null,
  now: number,
): Promise<PublicUser | null> {
  // Shape first, so junk costs no round trip at all.
  if (token === null || !looksLikeSessionToken(token)) return null;

  const id = await sessionIdOf(token);
  const row = await db
    .prepare(
      `SELECT s.id, s.user_id, s.created_at, s.last_seen_at, s.expires_at, s.revoked_at,
              u.id AS u_id, u.email, u.display_name, u.avatar_url,
              u.created_at AS u_created_at, u.updated_at AS u_updated_at, u.disabled_at
         FROM sessions s
         JOIN users u ON u.id = s.user_id
        WHERE s.id = ?`,
    )
    .bind(id)
    .first<SessionRow & UserRow & { u_id: string; u_created_at: number; u_updated_at: number }>();

  if (row === null) return null;
  if (row.revoked_at !== null) return null;
  if (row.expires_at <= now) return null;
  if (row.disabled_at !== null) return null;

  if (now - row.last_seen_at > LAST_SEEN_GRANULARITY_MS) {
    await db.prepare(`UPDATE sessions SET last_seen_at = ? WHERE id = ?`).bind(now, id).run();
  }

  return publicUser({
    id: row.u_id,
    email: row.email,
    display_name: row.display_name,
    avatar_url: row.avatar_url,
    created_at: row.u_created_at,
    updated_at: row.u_updated_at,
    disabled_at: row.disabled_at,
  });
}

/**
 * End a session.
 *
 * Deleted rather than marked revoked: `revoked_at` exists for sessions ended by
 * someone *else* — a future "sign out my other devices" — where the row is worth
 * keeping so the reason survives. A person signing themselves out here leaves
 * nothing worth retaining, and the smallest table is the one least able to leak.
 */
export async function destroySession(db: Db, token: string | null): Promise<void> {
  if (token === null || !looksLikeSessionToken(token)) return;
  const id = await sessionIdOf(token);
  await db.prepare(`DELETE FROM sessions WHERE id = ?`).bind(id).run();
}

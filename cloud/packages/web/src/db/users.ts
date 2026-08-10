/**
 * Turning a provider profile into an account.
 *
 * One function, and most of it is the linking rule — which is the only part
 * with a way to be catastrophically wrong rather than merely broken.
 */

import { hex, randomBytes } from '@zesterm/cloud-shared';

import type { Db, UserRow } from './types.ts';

/** What a provider tells us about someone. Shape-identical across providers. */
export interface Profile {
  /** The provider's **immutable** id. GitHub's numeric user id, never `login`. */
  readonly subject: string;
  readonly email: string | null;
  readonly emailVerified: boolean;
  readonly displayName: string;
  readonly avatarUrl: string | null;
}

function newUserId(): string {
  return hex(randomBytes(16));
}

/**
 * Find or create the account behind a provider identity.
 *
 * The order is the security property:
 *
 * 1. **`(provider, subject)`** — an identity we have seen. Nothing else is
 *    consulted, because this is the only lookup that proves anything.
 * 2. **A verified email matching a verified identity elsewhere** — the same
 *    person arriving through a second provider.
 * 3. Otherwise a new account.
 *
 * Step 2 is where this goes wrong if it is loosened. Linking on an
 * *unverified* address is a complete account-takeover primitive: sign up at
 * any provider claiming `victim@example.com`, and get merged into their
 * account. **Both** sides must assert verification — the arriving profile, and
 * the identity already on file — which is why `email_verified` is stored per
 * identity rather than per user.
 *
 * When verification is missing on either side the result is a separate
 * account, deliberately. That is a support question ("why do I have two?") and
 * the alternative is a breach.
 */
export async function upsertUser(db: Db, provider: string, profile: Profile, now: number) {
  const existing = await db
    .prepare(`SELECT user_id FROM oauth_identities WHERE provider = ? AND subject = ?`)
    .bind(provider, profile.subject)
    .first<{ user_id: string }>();

  if (existing !== null) {
    await db
      .prepare(
        `UPDATE oauth_identities
            SET last_login_at = ?, email = ?, email_verified = ?
          WHERE provider = ? AND subject = ?`,
      )
      .bind(now, profile.email, profile.emailVerified ? 1 : 0, provider, profile.subject)
      .run();
    await db
      .prepare(
        `UPDATE users SET display_name = ?, avatar_url = ?, updated_at = ? WHERE id = ?`,
      )
      .bind(profile.displayName, profile.avatarUrl, now, existing.user_id)
      .run();
    return existing.user_id;
  }

  let userId: string | null = null;
  /** Non-null only when this call is the one that inserted a fresh `users` row. */
  let createdUserId: string | null = null;

  if (profile.emailVerified && profile.email !== null) {
    const linkable = await db
      .prepare(
        `SELECT user_id FROM oauth_identities
          WHERE email = ? AND email_verified = 1
          LIMIT 1`,
      )
      .bind(profile.email)
      .first<{ user_id: string }>();
    userId = linkable?.user_id ?? null;
  }

  if (userId === null) {
    userId = newUserId();
    createdUserId = userId;
    await db
      .prepare(
        `INSERT INTO users (id, email, display_name, avatar_url, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?)`,
      )
      .bind(userId, profile.email, profile.displayName, profile.avatarUrl, now, now)
      .run();
  }

  // `DO NOTHING`, then read back what is actually there.
  //
  // Two first sign-ins for the same (provider, subject) can race -- one person,
  // two tabs, or a retried callback. A plain INSERT means the loser throws a
  // primary-key conflict out of the OAuth callback as a 500, *and* leaves the
  // `users` row it had already created behind with nothing pointing at it.
  //
  // Whoever wrote the identity first decides which account this is. That is
  // arbitrary between two racers and it is the only property that matters:
  // both requests must end up on the same account.
  await db
    .prepare(
      `INSERT INTO oauth_identities
         (provider, subject, user_id, email, email_verified, created_at, last_login_at)
       VALUES (?, ?, ?, ?, ?, ?, ?)
       ON CONFLICT (provider, subject) DO NOTHING`,
    )
    .bind(
      provider,
      profile.subject,
      userId,
      profile.email,
      profile.emailVerified ? 1 : 0,
      now,
      now,
    )
    .run();

  const settled = await db
    .prepare(`SELECT user_id FROM oauth_identities WHERE provider = ? AND subject = ?`)
    .bind(provider, profile.subject)
    .first<{ user_id: string }>();
  const winner = settled?.user_id ?? userId;

  if (winner !== createdUserId && createdUserId !== null) {
    // We lost, and the account we made has no identity pointing at it. Left
    // alone it is an orphan that a later verified-email link could attach to,
    // which would silently split one person across two accounts.
    await db.prepare(`DELETE FROM users WHERE id = ?`).bind(createdUserId).run();
  }

  return winner;
}

export async function findUser(db: Db, id: string): Promise<UserRow | null> {
  return db.prepare(`SELECT * FROM users WHERE id = ?`).bind(id).first<UserRow>();
}

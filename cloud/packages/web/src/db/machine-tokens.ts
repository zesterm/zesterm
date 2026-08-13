/**
 * Machine tokens: minting one at enrolment, and resolving one per request.
 *
 * The token never reaches storage — `machine_tokens.id` is `sha256(token)`,
 * exactly as `sessions` does it and for the same reason.
 *
 * The property that shapes both queries below: **revoking a principal revokes
 * its token, with no second write**. Liveness is a JOIN against the principal's
 * own row (`hosts` or `devices`) with every condition in the WHERE clause, so
 * the existing revoke routes kill the token as a side effect of what they
 * already do. A separate "also revoke the token" write would be a write someone
 * forgets, and the forgotten half would be the credential.
 */

import { looksLikeMachineToken, newMachineToken, sessionIdOf } from '@zesterm/cloud-shared';

import type { Db, EnrollKind } from './types.ts';
import { LAST_SEEN_GRANULARITY_MS } from './sessions.ts';

/**
 * Ninety days, sliding. A machine that phones home at all keeps its token
 * alive; one dark for a whole quarter re-enrols with a fresh code, which is a
 * person's act — the right admission gate for something that has been off the
 * account's radar that long.
 */
export const MACHINE_TOKEN_TTL_MS = 90 * 24 * 60 * 60 * 1000;

export interface NewMachineToken {
  /** Returned once, in the claim response, and never stored. */
  readonly token: string;
  readonly expiresAt: number;
}

/** Who a bearer token turned out to be. `userId` is the owning account. */
export interface MachinePrincipal {
  readonly kind: EnrollKind;
  readonly id: string;
  readonly userId: string;
}

export async function createMachineToken(
  db: Db,
  opts: { userId: string; kind: EnrollKind; principalId: string; now: number },
): Promise<NewMachineToken> {
  const { userId, kind, principalId, now } = opts;

  // One live token per machine: re-enrolling rotates rather than accumulates.
  // A principal with three live tokens is three credentials to leak and no
  // extra capability, and "how many are live" stops being a question.
  await db
    .prepare(
      `UPDATE machine_tokens SET revoked_at = ?
        WHERE principal_kind = ? AND principal_id = ? AND revoked_at IS NULL`,
    )
    .bind(now, kind, principalId)
    .run();

  const token = newMachineToken();
  const id = await sessionIdOf(token);
  const expiresAt = now + MACHINE_TOKEN_TTL_MS;

  await db
    .prepare(
      `INSERT INTO machine_tokens
         (id, user_id, principal_kind, principal_id, created_at, last_seen_at, expires_at)
       VALUES (?, ?, ?, ?, ?, ?, ?)`,
    )
    .bind(id, userId, kind, principalId, now, now, expiresAt)
    .run();

  return { token, expiresAt };
}

interface TokenHit {
  readonly user_id: string;
  readonly principal_id: string;
  readonly last_seen_at: number;
}

// One query per principal kind, because the liveness conditions live in the
// JOIN and the two joins name different tables. Everything that could disable
// this credential is in the WHERE clause: token revoked or expired, principal
// revoked, account disabled. A row comes back or it does not.
const RESOLVE_HOST = `
  SELECT t.user_id, t.principal_id, t.last_seen_at
    FROM machine_tokens t
    JOIN hosts h ON h.id = t.principal_id AND h.user_id = t.user_id
    JOIN users u ON u.id = t.user_id
   WHERE t.id = ? AND t.principal_kind = 'host'
     AND t.revoked_at IS NULL AND t.expires_at > ?
     AND h.revoked_at IS NULL AND u.disabled_at IS NULL`;

const RESOLVE_DEVICE = `
  SELECT t.user_id, t.principal_id, t.last_seen_at
    FROM machine_tokens t
    JOIN devices d ON d.id = t.principal_id AND d.user_id = t.user_id
    JOIN users u ON u.id = t.user_id
   WHERE t.id = ? AND t.principal_kind = 'device'
     AND t.revoked_at IS NULL AND t.expires_at > ?
     AND d.revoked_at IS NULL AND u.disabled_at IS NULL`;

/**
 * The principal behind a bearer token, or `null`.
 *
 * Every rejection is the same `null` — expired, revoked, revoked principal,
 * unknown, malformed — for the reason `resolveSession` collapses its answers:
 * "this token existed but its machine was revoked" is a fact worth more to
 * whoever found the token than to the machine that legitimately held it.
 */
export async function resolveMachineToken(
  db: Db,
  token: string,
  now: number,
): Promise<MachinePrincipal | null> {
  // Shape first, so junk costs no round trip at all.
  if (!looksLikeMachineToken(token)) return null;

  const id = await sessionIdOf(token);
  const host = await db.prepare(RESOLVE_HOST).bind(id, now).first<TokenHit>();
  const hit = host ?? (await db.prepare(RESOLVE_DEVICE).bind(id, now).first<TokenHit>());
  if (hit === null) return null;

  const kind: EnrollKind = host !== null ? 'host' : 'device';

  // Sliding expiry at the hour granularity sessions use, and the same write
  // refreshes the principal row's `last_seen_at` — which is the only writer
  // devices have for that column, so "last seen" on the fleet screen is this
  // line working.
  if (now - hit.last_seen_at > LAST_SEEN_GRANULARITY_MS) {
    await db
      .prepare(`UPDATE machine_tokens SET last_seen_at = ?, expires_at = ? WHERE id = ?`)
      .bind(now, now + MACHINE_TOKEN_TTL_MS, id)
      .run();
    const table = kind === 'host' ? 'hosts' : 'devices';
    await db
      .prepare(`UPDATE ${table} SET last_seen_at = ? WHERE id = ?`)
      .bind(now, hit.principal_id)
      .run();
  }

  return { kind, id: hit.principal_id, userId: hit.user_id };
}

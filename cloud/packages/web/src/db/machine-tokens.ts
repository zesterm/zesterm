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

// `d.status = 'approved'` sits beside the liveness conditions for the reason
// they are all here: approval is a fact about the principal, and putting it in
// this JOIN means demoting a device disables its token with no second write —
// and a `pending` registration (#184) never had a working credential to begin
// with, however its holder comes by a token string.
const RESOLVE_DEVICE = `
  SELECT t.user_id, t.principal_id, t.last_seen_at
    FROM machine_tokens t
    JOIN devices d ON d.id = t.principal_id AND d.user_id = t.user_id
    JOIN users u ON u.id = t.user_id
   WHERE t.id = ? AND t.principal_kind = 'device'
     AND t.revoked_at IS NULL AND t.expires_at > ?
     AND d.revoked_at IS NULL AND d.status = 'approved' AND u.disabled_at IS NULL`;

/**
 * The principal behind a bearer token, or `null`.
 *
 * Every rejection is the same `null` — expired, revoked, revoked principal,
 * unknown, malformed — and the resolve itself keeps that collapse. The one
 * deliberate crack is [`explainMachineToken`], which runs only on the failure
 * path and only for a token whose hash matches a real row: there the holder
 * provably once held a minted credential, and "this token existed but its
 * machine was revoked" is exactly the fact its owner needs — the original
 * reasoning here weighed it against a token *thief*, but the token is 48
 * random bytes, and starving the legitimate holder to deny a guesser
 * confirmation of a string nobody can guess bought nothing (#371).
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

/** Why a presented token no longer works — the holder's next move by name. */
export type TokenRefusal = 'revoked' | 'expired' | 'pending';

/**
 * Why `resolveMachineToken` answered `null`, when that is knowable and safe
 * to say (#371). Runs ONLY on the failure path, so the resolve's happy path
 * pays nothing; answers `null` for anything that must stay a bare 401 — a
 * token that never existed (no oracle for guessers) and a disabled account
 * (the account's standing is not the machine's business).
 *
 * A *rotated* token answers `revoked` on purpose: to the machine still
 * presenting it the credential is dead and re-enrolling is the fix, which is
 * the same next move an explicit revocation demands.
 */
export async function explainMachineToken(
  db: Db,
  token: string,
  now: number,
): Promise<TokenRefusal | null> {
  if (!looksLikeMachineToken(token)) return null;
  const id = await sessionIdOf(token);
  const row = await db
    .prepare(
      `SELECT user_id, principal_kind, principal_id, expires_at, revoked_at
         FROM machine_tokens WHERE id = ?`,
    )
    .bind(id)
    .first<{
      user_id: string;
      principal_kind: EnrollKind;
      principal_id: string;
      expires_at: number;
      revoked_at: number | null;
    }>();
  if (row === null) return null;
  if (row.revoked_at !== null) return 'revoked';
  if (row.expires_at <= now) return 'expired';

  // The token itself is live, so the resolve's JOIN refused the principal.
  // Ownership stays in the WHERE, this file's rule, so a row that moved
  // accounts (impossible today) would answer the bare refusal.
  if (row.principal_kind === 'host') {
    const host = await db
      .prepare(`SELECT revoked_at FROM hosts WHERE id = ? AND user_id = ?`)
      .bind(row.principal_id, row.user_id)
      .first<{ revoked_at: number | null }>();
    return host !== null && host.revoked_at !== null ? 'revoked' : null;
  }
  const device = await db
    .prepare(`SELECT revoked_at, status FROM devices WHERE id = ? AND user_id = ?`)
    .bind(row.principal_id, row.user_id)
    .first<{ revoked_at: number | null; status: string }>();
  if (device === null) return null;
  if (device.revoked_at !== null) return 'revoked';
  if (device.status === 'pending') return 'pending';
  return null;
}

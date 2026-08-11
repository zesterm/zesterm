/**
 * The one question the relay asks D1: is this machine still one of ours?
 *
 * The signature on a `hello` proves possession of a key. It does not say the
 * key was ever enrolled, or that the account has not revoked it since — and
 * "possession of a key is not a claim on an account" is the rule the registry
 * enforces everywhere else. So the control link needs one lookup, and exactly
 * one.
 *
 * # Why the answer is cached, and why only the yes
 *
 * The lookup is on the connect path, not the data path, so it is cheap by
 * position rather than by frequency — but a daemon that reconnects through a
 * flapping link would otherwise issue one query per attempt. Sixty seconds in
 * `ctx.storage` removes that, at the cost of one storage write per minute per
 * host.
 *
 * **Only the positive answer is cached.** A negative cache would mean a machine
 * that enrolled thirty seconds ago is told it is unknown for another thirty,
 * which is the shape of bug that gets reported as "enrolment is flaky" and is
 * miserable to reproduce. The cost of not caching it is a D1 query per dial
 * from an unenrolled key — and such a dial already woke the object, so the
 * query is not the marginal expense.
 *
 * The stale positive is worth naming too: a revoked host keeps its control link
 * for up to a minute. It buys nothing, because the *ticket* is what admits a
 * browser and `ownsLiveHost` refuses to mint one for a revoked machine the
 * instant it is revoked. Revocation cuts attaches; this only delays the parked
 * link noticing.
 */

import type { D1Binding } from '../env.ts';
import type { RoomStorage } from './state.ts';

/** How long a positive lookup stands. → ADR-009: never write storage per message. */
export const HOST_LOOKUP_TTL_MS = 60_000;

/**
 * The cache key. Unqualified because the object *is* the host's: one room per
 * host id, addressed by `idFromName('host:' + hostId)`, so there is no second
 * machine whose answer could land here.
 */
const CACHE_KEY = 'host:enrolled';

interface CachedLookup {
  readonly at: number;
}

/**
 * Is `host` enrolled to some account and not revoked?
 *
 * Deliberately a boolean rather than the row. The relay is told which machine a
 * pipe is for and nothing else; a row in hand is a row a later edit is tempted
 * to copy a field out of, and every one of those fields is something Cloudflare
 * would then hold. `ownsLiveHost` in the web Worker answers the same way and
 * says so.
 */
export async function hostIsEnrolled(args: {
  db: D1Binding;
  storage: RoomStorage;
  host: string;
  now: number;
}): Promise<boolean> {
  const { db, storage, host, now } = args;

  const cached = await storage.get<CachedLookup>(CACHE_KEY);
  // `now - at` rather than a stored expiry, so a clock that jumps backwards
  // shortens the cache instead of pinning it open until the clock catches up.
  if (cached !== undefined && now - cached.at < HOST_LOOKUP_TTL_MS && now >= cached.at) {
    return true;
  }

  const row = await db
    .prepare(`SELECT 1 AS ok FROM hosts WHERE id = ? AND revoked_at IS NULL`)
    .bind(host)
    .first<{ ok: number }>();
  if (row === null) return false;

  await storage.put(CACHE_KEY, { at: now } satisfies CachedLookup);
  return true;
}

/**
 * Record that this machine is reachable, **on connect and never per message**.
 *
 * The fleet screen's "last seen" wants to be live, and the way to make it live
 * is not to write a row every time a keystroke crosses. `zest-mesh`'s trust
 * store debounces the same field for the same reason; here position does the
 * work, because the control link is parked once and then silent for hours.
 *
 * Nothing is done with the result: a failed update means a slightly stale
 * timestamp, and refusing an authenticated host over it would take the fleet
 * down for a cosmetic column.
 */
export async function touchHost(db: D1Binding, host: string, now: number): Promise<void> {
  await db.prepare(`UPDATE hosts SET last_seen_at = ? WHERE id = ?`).bind(now, host).run();
}

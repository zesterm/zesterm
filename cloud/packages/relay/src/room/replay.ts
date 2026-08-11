/**
 * The attach ticket's replay set: a ticket is spent once, and once only.
 *
 * The edge verifies a ticket statelessly — our signature, our audience, this
 * room, still inside its thirty seconds — and every one of those is something a
 * *copy* of a ticket satisfies just as well. Until this file existed a ticket
 * seen on the wire (or in whatever logged the `Sec-WebSocket-Protocol` header on
 * the way past) was good for as many pipes as its holder cared to open, for the
 * rest of its window. The `jti` has been in the payload since the mint was
 * written, for this.
 *
 * # Why this cannot live at the edge, where the rest of the check does
 *
 * The Worker holds no state, and two Workers in two colos would not see each
 * other's spent ids anyway — a replay from a second colo would be admitted by a
 * check that looked, at every level of a metrics dashboard, like it was
 * working. The Durable Object the ticket names is the only thing that is
 * single-instance per host, so it is the only place the question "has this one
 * been spent" has an answer. Moving it forward for the round trip it appears to
 * save silently stops enforcing anything.
 *
 * # `ctx.storage`, never D1
 *
 * ADR-009 is explicit: the attach path is hot and does not have a D1 round trip
 * in it. The object's own storage is where per-object state that must outlive an
 * eviction lives — and an eviction is exactly what a set held in an instance
 * field would not survive. That failure is *silent and in the attacker's
 * favour*, since after a quiet minute every captured ticket looks fresh again,
 * which is why the suite runs this in both modes and evicts between the two
 * attaches.
 *
 * A write on the *attach* path is allowed; a write on the *data* path is what
 * the cost model forbids. One write per attach against a session's thousands of
 * messages is the whole of the difference, and `test/pipe.test.ts` still pins
 * the data path at zero.
 */

import { ATTACH_TICKET_TTL_MS } from '@zesterm/cloud-shared';

import type { RoomStorage } from './state.ts';

/**
 * The namespace spent ids live in.
 *
 * Prefixed because the sweep is a `list({ prefix })` and the room's storage has
 * another tenant: `room/hosts.ts` keeps its enrolment cache under `host:` in the
 * same key space, and a sweep that took the whole of storage would drop it every
 * thirty seconds.
 */
const SPENT_PREFIX = 'jti:';

/**
 * How long a spent id is remembered.
 *
 * An entry is worthless the moment its ticket expires, and the ticket's whole
 * remaining life when it is spent is at most one TTL: the edge refuses a ticket
 * whose `exp - iat` exceeds `ATTACH_TICKET_TTL_MS` and one whose `iat` has not
 * arrived, so `exp <= iat + TTL <= now + TTL` on the clock the edge read.
 *
 * Doubled anyway, because the clock *this* reads is a different machine's. The
 * two are NTP-disciplined and no skew has been measured here, so rather than
 * pick a margin out of the air the retention is simply generous: forgetting an
 * id early opens a replay window, remembering it too long costs one ~40-byte
 * row that the alarm deletes regardless.
 */
export const SPENT_RETENTION_MS = ATTACH_TICKET_TTL_MS * 2;

/** What is recorded against a spent id: when it stops being worth remembering. */
interface SpentTicket {
  readonly exp: number;
}

/**
 * The outcome of trying to spend a ticket.
 *
 * `unclaimable` is separate from `spent` so the close *reason* can say which,
 * while the close *code* deliberately does not: both mean this ticket does not
 * admit its holder, and telling a guesser which is which is worth more to them
 * than to anyone honest.
 */
export type TicketClaim = 'claimed' | 'spent' | 'unclaimable';

/** Where a `jti` is recorded. Exported for the tests, which assert on the key. */
export function spentTicketKey(jti: string): string {
  return `${SPENT_PREFIX}${jti}`;
}

/**
 * Spend `jti`, or refuse it.
 *
 * **Presence alone refuses**, without comparing the recorded expiry to `now`. A
 * lingering entry can only refuse a ticket whose own `exp` has passed, and the
 * edge refuses those before this object is ever woken — so the comparison would
 * add a way to be wrong and no way to be right.
 *
 * The read and the write are not a transaction and do not need to be one: there
 * is no `await` between them other than the storage calls themselves, which is
 * the shape a Durable Object's input gate holds off concurrent events across.
 * Nothing here rests on that being airtight — losing the race costs one
 * duplicate attach on one host, which is the state before this file existed.
 *
 * Every storage failure is `unclaimable` rather than a throw: an uncaught one
 * rejects `openAttach`, which hands the browser a 500, and a 500 is the one
 * answer it cannot read. Failing closed is the only safe direction — admitting
 * on a storage error would turn an outage into an open replay window.
 */
export async function claimAttachTicket(args: {
  storage: RoomStorage;
  jti: string;
  now: number;
}): Promise<TicketClaim> {
  const { storage, jti, now } = args;
  // Only the Worker addresses this object, and it passes the `jti` out of a
  // ticket whose decoder already refused an empty one — so this is our own bug
  // rather than a caller's. It fails closed on purpose: a refactor that stopped
  // passing the id would otherwise disable this check with nothing to notice,
  // which is the failure mode the whole file exists to avoid.
  if (jti.length === 0) return 'unclaimable';

  const entry: SpentTicket = { exp: now + SPENT_RETENTION_MS };
  try {
    if ((await storage.get(spentTicketKey(jti))) !== undefined) return 'spent';
    await storage.put(spentTicketKey(jti), entry);
  } catch {
    // Refused even though the id may well have been recorded, which costs this
    // holder their ticket. That is the right way round: the browser mints a
    // fresh one on the next dial, and the alternative is admitting an attach
    // whose id this object has failed to remember.
    return 'unclaimable';
  }

  await armSweep(storage, entry.exp);
  return 'claimed';
}

/**
 * Drop every entry that has expired, and re-arm while any remain.
 *
 * A set that only grows is a leak, and an entry is worthless
 * `SPENT_RETENTION_MS` after it was written. Deleting on the attach path
 * instead would put an unbounded scan in front of a browser that is waiting.
 */
export async function sweepSpentTickets(storage: RoomStorage, now: number): Promise<void> {
  const entries = await storage.list<unknown>({ prefix: SPENT_PREFIX });

  let next: number | null = null;
  for (const [key, value] of entries) {
    const exp = expiryOf(value);
    // An entry whose expiry cannot be read is one no later sweep could expire
    // either, so it is dropped rather than kept for ever. It costs at most one
    // replay window, for a ticket the edge is already bounding by `exp`.
    if (exp === null || exp <= now) {
      await storage.delete(key);
      continue;
    }
    next = next === null ? exp : Math.min(next, exp);
  }

  // Re-armed only while something is left. An alarm that always rescheduled
  // itself would give "an idle host costs nothing" away one wake-up at a time —
  // a room whose last browser detached hours ago has to go quiet completely.
  if (next !== null) await storage.setAlarm(next);
}

/**
 * Arm the sweep for `at`, unless one is already pending.
 *
 * There is one alarm per object, so `setAlarm` replaces rather than adds:
 * re-arming on every attach would slide the sweep forward for as long as a host
 * keeps receiving them, and a busy room would never sweep at all. A pending
 * alarm is always no later than the first unswept entry's expiry, so leaving it
 * alone is what keeps the set bounded.
 *
 * A failure is swallowed rather than failing the claim, on `touchHost`'s
 * argument: the id is recorded, this attach is honest, and refusing it would
 * take a browser offline over the *housekeeping*. The cost is one unswept entry
 * until the next attach finds no pending alarm and arms one.
 */
async function armSweep(storage: RoomStorage, at: number): Promise<void> {
  try {
    if ((await storage.getAlarm()) !== null) return;
    await storage.setAlarm(at);
  } catch {
    // See above.
  }
}

/** A stored entry's expiry, or `null` if this is not one. */
function expiryOf(value: unknown): number | null {
  if (typeof value !== 'object' || value === null) return null;
  const exp = (value as Record<string, unknown>)['exp'];
  return typeof exp === 'number' && Number.isFinite(exp) ? exp : null;
}

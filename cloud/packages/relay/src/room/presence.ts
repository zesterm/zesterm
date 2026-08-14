/**
 * Is this room's daemon still parked, and how would we know? (#237)
 *
 * # What the room can actually observe
 *
 * Almost nothing, and that is deliberate. The daemon pings every thirty
 * seconds (`KEEPALIVE_INTERVAL`, `crates/zest-daemon/src/relay.rs`) and those
 * are ordinary *application* text frames — but `room.ts` hands the platform a
 * `WebSocketRequestResponsePair` for them, so workerd answers `ping` with
 * `pong` **beneath the object** and `webSocketMessage` is never called. ADR-009's
 * "an idle host costs nothing" is that one line, so the traffic-driven refresh
 * the issue first proposed cannot be built: there is no traffic the room sees.
 *
 * Two things are left, and this file is both:
 *
 * 1. `getWebSocketAutoResponseTimestamp` — when the platform last answered a
 *    ping, recorded without waking anything. Free evidence, readable whenever
 *    the object happens to be up.
 * 2. The **alarm**, which is what makes it be up on a schedule.
 *
 * # Why the alarm is necessary rather than merely convenient
 *
 * The column is read by a *different* Worker through D1, so a fact the room
 * knows and never writes down is a fact the fleet screen cannot see. Without a
 * timer the room only wakes on connect and on attach — so a machine parked and
 * unused for eight hours would have written nothing for eight hours, and any
 * bound short enough to expire a dead room would call that machine asleep.
 * That is #237 again, wearing the opposite sign.
 *
 * It is self-limiting, which is what keeps ADR-009 honest: the alarm is
 * re-armed only while a ready control link is actually there, so the moment a
 * daemon goes away the room stops waking. An account with nothing dialled in
 * costs nothing, exactly as before; a *parked* host costs one wake every
 * `CONTROL_SEEN_REFRESH_MS`.
 */

import { CONTROL_SEEN_REFRESH_MS } from '@zesterm/cloud-shared';

import { readControlAttachment, readyControlLink } from './control.ts';
import type { RoomState, RoomStorage } from './state.ts';

/**
 * How long since the last auto-answered ping before a parked link is treated
 * as dead.
 *
 * Four missed keepalives at the daemon's thirty-second interval. Generous on
 * purpose: the cost of being late is a machine shown online that answers an
 * attach with `CLOSE_PIPE_DIAL_TIMEOUT` — a named failure — while the cost of
 * being early is a reachable machine shown asleep, which is the bug this file
 * exists to fix and which the UI gives no way to argue with.
 *
 * Not derived from the Rust constant, because it cannot be: separate builds
 * ship separately. It is a multiple large enough that the two can drift by a
 * factor of two without this becoming wrong.
 */
export const KEEPALIVE_STALE_MS = 120_000;

/** What the room can say about its daemon right now. */
export interface ParkedLiveness {
  /** The machine this room belongs to, from the authenticated attachment. */
  readonly host: string;
  /**
   * Whether the link is demonstrably still there.
   *
   * `false` means the socket is still held by the platform but has stopped
   * answering keepalives — a peer that vanished without a close, which is the
   * case a socket count alone cannot see.
   */
  readonly alive: boolean;
}

/**
 * The parked link's liveness, or `null` when there is no ready link at all.
 *
 * The three answers are deliberately distinct, and they do **not** all lead to
 * a write:
 *
 * - `alive: true` — the only answer that writes a timestamp.
 * - `alive: false` — parked and gone silent. The caller clears the column,
 *   and can, because this answer still carries the host id.
 * - `null` — no ready link at all, and therefore *no host id to clear a row
 *   by*. The caller cannot write anything here, and does not try: the column
 *   was already cleared by the close handler for a link that closed, and for
 *   one lost without a close it decays on its own once the bound passes. That
 *   the two silent cases are handled by different mechanisms is the reason
 *   they are different return values rather than one falsy answer.
 *
 * **A missing auto-response timestamp counts as alive**, and that is the
 * careful part. `null` there means the platform has recorded none *yet* — the
 * ordinary state of a link that parked seconds ago and has not been pinged —
 * so treating it as death would take every freshly-parked machine offline for
 * one refresh interval. The socket still being in `getWebSockets` is the same
 * evidence `openAttach` already dials on, so falling back to it is no weaker
 * than the behaviour that shipped.
 */
export function parkedLiveness(state: RoomState, now: number): ParkedLiveness | null {
  const ws = readyControlLink(state);
  if (ws === null) return null;

  const attachment = readControlAttachment(ws);
  // `readyControlLink` already filtered on `s === 'ready'`, so this is a
  // shape-narrowing rather than a real branch — but the attachment is the only
  // place the host id survives an eviction, and inventing one would be worse
  // than declining to answer.
  if (attachment === null || attachment.s !== 'ready') return null;

  const answered = state.getWebSocketAutoResponseTimestamp(ws);
  if (answered === null) return { host: attachment.host, alive: true };

  const at = answered.getTime();
  // A timestamp from the future is not evidence: a clock that jumped forward
  // would otherwise pin a dead machine online until it caught up.
  const alive = at <= now && now - at < KEEPALIVE_STALE_MS;
  return { host: attachment.host, alive };
}

/**
 * Is anything already scheduled to keep this room's presence fresh?
 *
 * The guard on the attach-time heal, and the reason that heal costs nothing
 * in steady state. A pending alarm means the dispatcher will run
 * `refreshPresence` when it fires, so an attach has nothing to add; no alarm
 * at all means the column is not being maintained by anyone, which is exactly
 * the state a link parked before this code deployed is left in.
 *
 * Deliberately "is *an* alarm pending" rather than "is a *presence* alarm
 * pending": there is one alarm per object and the replay sweep shares it, so
 * the two cannot be told apart — and they need not be, because
 * `RelayRoom.alarm` does both jobs whoever asked for the wake. A sweep alarm
 * therefore also repairs presence, one interval later; this only makes the
 * repair immediate.
 *
 * A storage failure answers `true` — "assume something is scheduled" — so a
 * blip skips the heal rather than writing D1 on every attach for as long as
 * it lasts. The alarm path repairs it either way.
 */
export async function presenceIsScheduled(storage: RoomStorage): Promise<boolean> {
  try {
    return (await storage.getAlarm()) !== null;
  } catch {
    return true;
  }
}

/**
 * Ask to be woken in `CONTROL_SEEN_REFRESH_MS`, without moving an earlier alarm.
 *
 * There is one alarm per object and `setAlarm` **replaces**, so this is the
 * discipline `room/replay.ts` states and the reason `room.ts`'s handler is a
 * dispatcher: the replay sweep also wants the alarm, and whichever of the two
 * is scheduled first must survive. Setting only when strictly earlier means
 * neither user can push the other's deadline out, and an early wake is
 * harmless because the handler does both jobs every time.
 *
 * Failures are swallowed on `touchHost`'s argument: this is housekeeping for a
 * dot on a screen, and there is no peer waiting on it. The cost is one missed
 * refresh, after which the column decays to `offline` — the safe direction.
 */
export async function armPresenceRefresh(storage: RoomStorage, now: number): Promise<void> {
  const at = now + CONTROL_SEEN_REFRESH_MS;
  try {
    const pending = await storage.getAlarm();
    if (pending !== null && pending <= at) return;
    await storage.setAlarm(at);
  } catch {
    // See above.
  }
}

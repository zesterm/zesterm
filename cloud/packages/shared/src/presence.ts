/**
 * How fresh a parked control link has to be to count as *online* (#237).
 *
 * Two constants because two separately-deployed Workers have to agree about
 * one fact, and neither can import the other: the **relay** refreshes
 * `hosts.control_seen_at` on the first cadence while a link is parked, and the
 * **web Worker** turns that column into `online` against the second. Here for
 * `ATTACH_TICKET_TTL_MS`'s reason — a number two deploys must share is a
 * number that drifts the moment it is written down twice.
 *
 * # What the relay can actually observe, which is what these numbers are for
 *
 * The daemon pings every 30s (`KEEPALIVE_INTERVAL` in
 * `crates/zest-daemon/src/relay.rs`), and those are *application* text frames
 * — but the room registers them as a `WebSocketRequestResponsePair`, so
 * workerd answers them **beneath the object** and `webSocketMessage` is never
 * called. That is not an oversight to route around: it is the whole of
 * ADR-009's "an idle host costs nothing".
 *
 * So the room cannot be woken *by* the keepalive, and a refresh driven by the
 * link's own traffic cannot be built. What the platform does offer is
 * `getWebSocketAutoResponseTimestamp` — when it last answered a ping, recorded
 * without waking anything — which the room can read whenever it wakes for its
 * own reasons. The alarm is what makes it wake on a schedule, and these two
 * numbers are that schedule and the tolerance read against it.
 */

/**
 * How often the relay refreshes the column while a control link is parked.
 *
 * This is a **wake-up per parked host**, and therefore the one place ADR-009's
 * arithmetic changes: an idle host now costs one alarm every five minutes
 * instead of nothing. It is bounded and self-limiting — the alarm is re-armed
 * only while a ready control link is actually there, so a room whose daemon
 * went away goes completely quiet again, and an account with nothing dialled
 * in still costs nothing.
 *
 * Five minutes rather than one: the fact being kept fresh is a dot on a
 * screen, and the failure it guards against (a machine shown asleep that
 * answers instantly) is fixed by *any* cadence far below the bound. Paying
 * five times more for a dot that goes stale five minutes sooner is not a
 * trade worth making.
 */
export const CONTROL_SEEN_REFRESH_MS = 5 * 60 * 1000;

/**
 * How stale the column may be before `/api/hosts` stops saying `online`.
 *
 * Three refresh intervals, so two missed alarms in a row do not flap a machine
 * that is sitting there perfectly reachable — a Durable Object's alarm is
 * retried rather than guaranteed-punctual, and a bound equal to the cadence
 * would turn every retry into a machine blinking out of the fleet.
 *
 * The asymmetry of being wrong here is deliberate and it decides the number.
 * Too *long* and a machine that died is shown online for up to fifteen
 * minutes — after which clicking it fails with `CLOSE_HOST_ABSENT` or
 * `CLOSE_PIPE_DIAL_TIMEOUT`, both of which say what happened. Too *short* and
 * a machine that is parked and reachable is shown asleep, which is #237 —
 * the bug this exists to fix, and the one with no recovery in the UI at all.
 */
export const CONTROL_SEEN_BOUND_MS = 3 * CONTROL_SEEN_REFRESH_MS;

/**
 * Is a machine's control link fresh enough to call `online`?
 *
 * One function so the relay's own tests and the web Worker's read the same
 * comparison. `null` — never connected, or cleared when the link closed — is
 * offline, and so is a timestamp from the future: a clock that jumped is not
 * evidence of anything, and treating it as live would pin a machine online
 * until the clock caught up.
 */
export function controlLinkIsLive(controlSeenAt: number | null, now: number): boolean {
  if (controlSeenAt === null) return false;
  if (controlSeenAt > now) return false;
  return now - controlSeenAt < CONTROL_SEEN_BOUND_MS;
}

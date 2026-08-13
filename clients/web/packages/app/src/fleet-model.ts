/**
 * Pure fleet-screen logic (design §7) — what a host card SAYS, kept out of
 * the component so `node --test` proves the honesty rules without a DOM.
 *
 * The rule the tests pin: **absent fields are omitted, never faked.** The
 * registry carries enrolment facts only (ADR-006 — no session state, no
 * endpoints), so a card shows what the record holds and nothing else. The
 * mock's `path`/latency rows and tunnel pill are #148, wake-over-LAN is
 * #146, and a session count appears only when a caller has one to give —
 * rendering `0` for "unknown" would be the comfortable lie.
 */

// Type-only, so this model keeps its no-runtime-dependency property: importing
// `directory-source.ts` for a value would pull `@sigx/actors` into every test
// that renders a card.
import type { DirectoryStatus } from './directory-source.ts';
import type { Device, Host } from './registry.ts';

/**
 * A key fingerprint, truncated head+tail for a card row (`8f2a…c41d`).
 *
 * Pure string surgery — no hashing here. `hosts.id` IS the enrolled Ed25519
 * public key (ADR-006: identities are public keys, no fingerprinting), so
 * what the row shows is the head and tail of the key itself. A key within
 * one character of the visible budget is shown whole — the ellipsis itself
 * costs a character, so truncating there would hide one character behind
 * one ellipsis and save nothing.
 */
export function fingerprintDisplay(key: string, visible = 8): string {
  const head = Math.ceil(visible / 2);
  const tail = visible - head;
  // `<= visible + 1`: the ellipsis itself costs a character, so truncation
  // only ever shortens keys at least two past the visible budget.
  if (key.length <= visible + 1) return key;
  return `${key.slice(0, head)}…${key.slice(key.length - tail)}`;
}

/**
 * Rough, and deliberately so — an exact age is not a thing anyone reads.
 * `now` is a parameter rather than a `Date.now()` here so the function is
 * pure; the component reads the clock at render (a snapshot captured at
 * setup froze every age at mount time, which is wrong rather than stale).
 */
export function ago(at: number | null, now: number): string {
  if (at === null) return 'never';
  const mins = Math.floor((now - at) / 60_000);
  if (mins < 1) return 'just now';
  if (mins < 60) return `${mins}m ago`;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return `${hours}h ago`;
  return `${Math.floor(hours / 24)}d ago`;
}

/**
 * How long an enrolment code has left, as the panel shows it (`9:47`), or
 * `expired` once it is gone. Pure over the given clock for the same reason
 * `ago` is; the component ticks and re-reads.
 *
 * `ceil`, not `floor`: a countdown rounds up so `0:00` is never shown beside
 * a code that still works — the display reaches `expired` at the same moment
 * the server stops honouring the code, not a second before it.
 */
export function codeCountdown(expiresAt: number, now: number): string {
  const left = expiresAt - now;
  if (left <= 0) return 'expired';
  const secs = Math.ceil(left / 1000);
  return `${Math.floor(secs / 60)}:${String(secs % 60).padStart(2, '0')}`;
}

/**
 * What the one enrolment panel shows the moment a mint for `kind` starts.
 *
 * Single occupancy is decided at START, not when the new code lands: leaving
 * a host code on screen while a device mint is in flight leaves a code
 * someone copies into the wrong sign-in, under a panel claiming a mint it
 * did not start. A remint for the SAME kind keeps its panel — the visible
 * code is still the server's, and a panel that blinks away mid-remint reads
 * as the mint having failed.
 */
export function mintPanelOnStart<P extends { readonly kind: 'host' | 'device' }>(
  panel: P | null,
  kind: 'host' | 'device',
): P | null {
  return panel !== null && panel.kind === kind ? panel : null;
}

/**
 * One resolved answer for a clipboard write, however it goes wrong.
 *
 * On an insecure origin `navigator.clipboard` is `undefined` — not a
 * clipboard that rejects — so the unguarded call THROWS synchronously and a
 * `.catch` chained on its result never runs; the click handler errors instead
 * of the copy failing politely. Folded here so the absent API, a synchronous
 * throw and an ordinary rejection (denied permission) all resolve to
 * `copy failed`, and `copied` is said only once the write actually took.
 */
export async function copyOutcome(
  clipboard: { writeText(text: string): Promise<void> } | undefined,
  text: string,
): Promise<'copied' | 'copy failed'> {
  try {
    if (clipboard === undefined) return 'copy failed';
    await clipboard.writeText(text);
    return 'copied';
  } catch {
    return 'copy failed';
  }
}

/**
 * What the fleet screen should do about this browser's own key, decided after
 * each registry load.
 *
 * - `register` — the key is not in the account's list, and it is a key worth
 *   listing: auto-register it silently.
 * - `awaiting-approval` — the account knows the key and has not approved it;
 *   the screen shows the banner, because a pending browser that looks
 *   approved is one whose owner never learns it is waiting.
 * - `nothing` — approved, or **ephemeral**. An ephemeral identity was minted
 *   because the key store could not be read and is deliberately not
 *   persisted, so registering it would enrol a key that is gone on the next
 *   load — a pending row per visit, forever, none of them this browser by
 *   the time anyone approves it.
 */
export type OwnDeviceAction = 'register' | 'awaiting-approval' | 'nothing';

export function ownDeviceAction(
  devices: readonly Device[],
  ownId: string,
  ephemeral: boolean,
): OwnDeviceAction {
  if (ephemeral) return 'nothing';
  const own = devices.find((d) => d.id === ownId);
  if (own === undefined) return 'register';
  return own.status === 'pending' ? 'awaiting-approval' : 'nothing';
}

/**
 * What a browser calls itself when it self-registers.
 *
 * A product name rather than the raw UA string: the label lands on the
 * devices screen of every other browser on the account, where "Firefox" tells
 * the owner which row is which and the 120-character UA tells them nothing.
 * The order is the UA's own compatibility archaeology — everything claims
 * `Chrome/` and almost everything claims `Safari/`, so the more specific
 * tokens must win first. `browser` is the honest fallback, and the Worker
 * accepts it as an ordinary label.
 */
export function browserLabel(userAgent: string): string {
  if (userAgent.includes('Firefox/')) return 'Firefox';
  if (userAgent.includes('Edg/')) return 'Edge';
  if (userAgent.includes('OPR/')) return 'Opera';
  if (userAgent.includes('Chrome/')) return 'Chrome';
  if (userAgent.includes('Safari/')) return 'Safari';
  return 'browser';
}

/** One device line of the "Browsers and phones" list. */
export interface DeviceRowView {
  readonly id: string;
  readonly name: string;
  /** `browser · last seen 5m ago` — the facts every row has. */
  readonly meta: string;
  /** Renders the pending marker: registered, not yet approved. */
  readonly pending: boolean;
  /** The spoken warning that this key is readable by scripts on the origin. */
  readonly keyReadable: boolean;
}

/**
 * A registry device → what its row renders. Pure over the given clock for
 * the reason `ago` is; the flags are separate from `meta` because the
 * component styles them — a marker and a warning folded into one string
 * would render in one colour and stop reading as either.
 */
export function deviceRow(device: Device, now: number): DeviceRowView {
  return {
    id: device.id,
    name: device.label,
    meta: `${device.kind} · last seen ${ago(device.lastSeenAt, now)}`,
    pending: device.status === 'pending',
    keyReadable: device.extractable,
  };
}

/** One label/value line of a card body. */
export interface CardRow {
  readonly label: string;
  readonly value: string;
  /** Fingerprints render in the mono face at 10.5px; prose values do not. */
  readonly mono: boolean;
}

/**
 * The dot and the words beside it.
 *
 * `asleep` is deliberately not `degraded`, and that is the whole point of this
 * type existing rather than a boolean. Over the relay most of a fleet is asleep
 * most of the time — a lid is shut, a desktop is off — and a screen that paints
 * the ordinary case as a fault is one people stop reading, taking the real
 * faults with it. `unknown` is separate again: it is what a card says when
 * nothing has asked, which is not the same as having asked and heard nothing.
 */
export interface CardPresence {
  readonly text: string;
  readonly kind: 'unknown' | 'pending' | 'ok' | 'asleep' | 'degraded';
  /** Whether the card leads anywhere. Only a machine that answered does. */
  readonly reachable: boolean;
}

export interface HostCard {
  readonly id: string;
  readonly name: string;
  /** The card gets the accent border and the `this machine` note. */
  readonly local: boolean;
  readonly rows: readonly CardRow[];
  readonly presence: CardPresence;
}

export interface HostCardContext {
  /**
   * Which enrolled host is the machine in front of the user, when that is
   * knowable at all. On the hosted path it is not — a browser is a *device*
   * and never appears among hosts — so today's caller passes `null` and no
   * card claims to be this machine.
   */
  readonly localHostId: string | null;
  /** A session count, only when something real supplies one. */
  readonly sessions?: number;
  /**
   * What the live directory says about this machine, where anything is asking.
   *
   * Absent means nothing is — the local path, or a deployment with no relay —
   * and the card says `unknown` rather than inventing an answer. When present
   * it also supplies the session count, so the two can never disagree.
   */
  // `| undefined` explicitly: `exactOptionalPropertyTypes` is on, and the
  // caller passes the absent case as a value rather than omitting the key.
  readonly status?: DirectoryStatus | undefined;
  readonly now: number;
}

/** What the dot and its label say, given what the directory knows. */
export function presenceOf(status: DirectoryStatus | undefined): CardPresence {
  if (status === undefined) return { text: '', kind: 'unknown', reachable: false };
  switch (status.kind) {
    case 'pending':
      return { text: 'connecting…', kind: 'pending', reachable: false };
    case 'pairing':
      // The code is the message: it has to be compared against what the machine
      // itself is showing, and a prompt without it is a wait with no way out.
      return { text: `approve ${status.code} on that machine`, kind: 'degraded', reachable: false };
    case 'offline':
      return { text: 'asleep', kind: 'asleep', reachable: false };
    case 'error':
      return { text: status.message, kind: 'degraded', reachable: false };
    case 'ready': {
      const n = status.view.sessions.length;
      return {
        text: n === 1 ? 'online · 1 session' : `online · ${n} sessions`,
        kind: 'ok',
        reachable: true,
      };
    }
  }
}

/** A registry record → what its card renders. Absent fields are omitted. */
export function hostCard(host: Host, ctx: HostCardContext): HostCard {
  const rows: CardRow[] = [];
  if (host.platform !== '') rows.push({ label: 'os', value: host.platform, mono: false });
  rows.push({ label: 'key', value: fingerprintDisplay(host.id), mono: true });
  // A live count outranks a supplied one — they describe the same thing, and
  // the live one is the one that just came off a socket.
  const live = ctx.status?.kind === 'ready' ? ctx.status.view.sessions.length : undefined;
  const sessions = live ?? ctx.sessions;
  if (sessions !== undefined) {
    rows.push({ label: 'sessions', value: String(sessions), mono: false });
  }
  rows.push({ label: 'last seen', value: ago(host.lastSeenAt, ctx.now), mono: false });
  return {
    id: host.id,
    name: host.label,
    local: ctx.localHostId !== null && host.id === ctx.localHostId,
    rows,
    presence: presenceOf(ctx.status),
  };
}

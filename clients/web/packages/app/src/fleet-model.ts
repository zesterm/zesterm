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

import type { Host } from './registry.ts';

/**
 * A key fingerprint, truncated head+tail for a card row (`8f2a…c41d`).
 *
 * Pure string surgery — no hashing here. `hosts.id` IS the enrolled Ed25519
 * public key (ADR-006: identities are public keys, no fingerprinting), so
 * what the row shows is the head and tail of the key itself. A key short
 * enough that truncating saves nothing is shown whole: `abc…def` for a
 * nine-char key hides one character behind one ellipsis.
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

/** One label/value line of a card body. */
export interface CardRow {
  readonly label: string;
  readonly value: string;
  /** Fingerprints render in the mono face at 10.5px; prose values do not. */
  readonly mono: boolean;
}

export interface HostCard {
  readonly id: string;
  readonly name: string;
  /** The card gets the accent border and the `this machine` note. */
  readonly local: boolean;
  readonly rows: readonly CardRow[];
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
  readonly now: number;
}

/** A registry record → what its card renders. Absent fields are omitted. */
export function hostCard(host: Host, ctx: HostCardContext): HostCard {
  const rows: CardRow[] = [];
  if (host.platform !== '') rows.push({ label: 'os', value: host.platform, mono: false });
  rows.push({ label: 'key', value: fingerprintDisplay(host.id), mono: true });
  if (ctx.sessions !== undefined) {
    rows.push({ label: 'sessions', value: String(ctx.sessions), mono: false });
  }
  rows.push({ label: 'last seen', value: ago(host.lastSeenAt, ctx.now), mono: false });
  return {
    id: host.id,
    name: host.label,
    local: ctx.localHostId !== null && host.id === ctx.localHostId,
    rows,
  };
}

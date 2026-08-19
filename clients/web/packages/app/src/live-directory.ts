/**
 * The hosted path's session list: one connection per machine, held in the tab.
 *
 * The deployed client has no sidecar, so there is nowhere to host the
 * `SessionDirectory` actor and nothing to read a directory *from*
 * (`directory-source.ts` carries the argument, and the two ways of giving a
 * browser an actor that were rejected). What is left is the only honest
 * mechanism: dial each machine yourself with `watch_sessions: true`, and let
 * the daemon's own pushes be the store. The daemon is the truth on loopback
 * too — the sidecar merely relays it — so this holds the same list by the same
 * rule, one layer closer.
 *
 * **Which machines?** `GET /api/hosts` — the account's enrolled machines
 * (`registry.ts`). Not discovery: mDNS cannot cross the internet, and a
 * machine that is asleep still belongs in the list.
 *
 * ## The three things this file exists to get right
 *
 * **A machine that is asleep is not an error.** The relay accepts the socket
 * and then closes it `4404` when no control link is parked, which is the
 * ordinary state of a laptop with its lid shut. `ConnectionClient` cannot see a
 * close code — `ByteLinkHandlers` has none — so all this can observe is that
 * the handshake never completed. After `REACH_TIMEOUT_MS` the watch is stood
 * *down*, not retried harder, and the list says `offline`.
 *
 * **And it must not become a hot loop.** `ConnectionClient`'s redial ladder
 * climbs 200 ms → 5 s and then repeats at 5 s for ever, which is right for a
 * daemon that dropped and wrong for a machine that is asleep: over the relay
 * every repeat is a ticket mint (a `POST` to the account service), a socket at
 * the edge and a Durable Object wake-up — per machine, for as long as the tab
 * is open. A fleet of ten sleeping laptops would be two round trips a second,
 * indefinitely, to learn nothing. So the ladder is allowed to run its course
 * and is then replaced by a slow probe that doubles while the machine stays
 * absent.
 *
 * **One connection per machine, reused.** The obvious way to start a session is
 * `create-session.ts`'s: open a connection, create, close. On loopback that is
 * a cheap second socket. Over the relay it is a second ticket, a second pipe
 * and a second full handshake for one message — so `createSession` below writes
 * down the connection that is already watching, and waits for the `sessions`
 * push that answers it.
 *
 * ## Shape
 *
 * A store keyed by `HostId`, and a `DirectorySource` *per host* rather than one
 * source over a keyed set — `directory-source.ts` says why that turned out to
 * be the better seam. Everything a test needs to drive is behind `openLink`,
 * so the supervision below runs with no network, no crypto and no clock:
 * `ConnectionClient` is already tested against a scripted daemon in its own
 * package, and re-testing it here would prove that package's behaviour rather
 * than this one's.
 */

import type { ClientSigner } from '@zesterm/auth';
import {
  ConnectionClient,
  REDIAL_MAX_MS,
  REDIAL_MIN_MS,
  systemClock,
  type Clock,
  type ConnectionEvents,
  type TimerHandle,
} from '@zesterm/client';
import {
  hostFactsOf,
  sessionEntryOf,
  type HostFacts,
  type HostInfo,
  type SessionEntry,
} from '@zesterm/control';
import { signal } from 'sigx';

import { dialFor, type RelayAccess } from './dial-for.ts';
import type { DirectorySource, DirectoryStatus } from './directory-source.ts';

/** A machine from the account's registry, reduced to what a dial needs. */
export interface LiveHost {
  /** 64 lowercase hex — the daemon's public key, and the relay's room name. */
  readonly id: string;
  readonly label: string;
}

/**
 * How long a machine gets to answer before it is called asleep.
 *
 * Long enough for `ConnectionClient`'s ladder to climb to its ceiling —
 * 200 + 400 + 800 + 1600 + 3200 ms of retries — so a daemon that is merely
 * restarting is waited for rather than stood down. One step past that the
 * ladder repeats at 5 s for ever, which is the loop the module doc costs out.
 *
 * A test pins the arithmetic rather than the number, because the number is only
 * meaningful relative to `REDIAL_MIN_MS`/`REDIAL_MAX_MS` and those live in
 * another package.
 */
export const REACH_TIMEOUT_MS = 8_000;

/** The first wait after a machine has been stood down. */
export const PROBE_MIN_MS = 30_000;

/**
 * The longest wait between probes.
 *
 * A machine asleep overnight should not be probed every thirty seconds, and a
 * machine that has just woken should not take a quarter of an hour to appear.
 * Five minutes is the compromise; the doubling in between is what keeps the
 * cost proportional to how long it has been gone.
 */
export const PROBE_MAX_MS = 300_000;

/** How long a `create_session` may go unanswered before it is given up on. */
export const CREATE_TIMEOUT_MS = 15_000;

/**
 * The part of `ConnectionClient` this file drives.
 *
 * Structural, so a test can supply a link that speaks no protocol at all. The
 * real thing satisfies it without adapting — see `relayLinks`.
 */
export interface DirectoryLink {
  connect(): void;
  close(): void;
  createSession(opts: {
    command: string;
    cwd: string;
    cols: number;
    rows: number;
  }): void;
}

/**
 * Open a watching connection to one machine.
 *
 * `null` means this browser cannot reach that machine at all — a deployment
 * with no relay, which is an ordinary configuration rather than a fault. It is
 * distinct from a dial that fails: nothing is retried for it, because nothing
 * about it will change while the page is loaded.
 */
export type OpenLink = (host: LiveHost, events: ConnectionEvents) => DirectoryLink | null;

/** Where a machine is, as far as this tab can tell. */
export type HostPresence =
  | { readonly kind: 'probing' }
  | { readonly kind: 'pairing'; readonly code: string }
  | { readonly kind: 'online' }
  | { readonly kind: 'offline' }
  | { readonly kind: 'failed'; readonly message: string };

/** One machine's row in the store. */
export interface HostSnapshot {
  readonly host: HostInfo;
  readonly presence: HostPresence;
  readonly sessions: readonly SessionEntry[];
  /**
   * What this machine says it is and can launch (#352), or null while it has
   * not said.
   *
   * Sticky across ordinary session pushes and dropped when the link goes —
   * the rule `zest-app`'s `fleet.rs` settled for the same data: an absent
   * offer on a push means "nothing new to say", while a machine that is no
   * longer answering must not keep offering launch targets that cannot run.
   */
  readonly facts: HostFacts | null;
  readonly lastCreated: string | null;
}

export interface LiveDirectoryOptions {
  readonly openLink: OpenLink;
  readonly clock?: Clock;
  readonly reachTimeoutMs?: number;
  readonly probeMinMs?: number;
  readonly probeMaxMs?: number;
  readonly createTimeoutMs?: number;
}

export interface LiveDirectory {
  /**
   * Watch exactly these machines: open a connection for each new one, close
   * the connections of any that are no longer in the account.
   *
   * Idempotent for a machine already watched — the fleet screen refetches
   * `/api/hosts` after every revoke, and a second call must not mean a second
   * pipe.
   */
  setHosts(hosts: readonly LiveHost[]): void;
  /** The seam `SessionList` reads, for one machine. */
  sourceFor(hostId: string): DirectorySource;
  /** The same status, for a caller that is not a `SessionList` — the fleet rows. */
  statusFor(hostId: string): DirectoryStatus;
  /** Every machine's snapshot, in the order `setHosts` last gave them. */
  snapshots(): readonly HostSnapshot[];
  /**
   * Start a session on a machine, over the connection that is already
   * watching it. Rejects if that machine is not connected.
   */
  createSession(hostId: string, size: { cols: number; rows: number }): Promise<SessionEntry>;
  /** Close every connection and cancel every timer. */
  close(): void;
}

/** Per-machine bookkeeping. Deliberately outside the signal: none of it renders. */
interface Watch {
  host: LiveHost;
  link: DirectoryLink | null;
  /** Armed while a connection has yet to reach `connected`; see `REACH_TIMEOUT_MS`. */
  deadline: TimerHandle | null;
  /** Armed while the machine is stood down, counting to the next probe. */
  probe: TimerHandle | null;
  /** How long the *next* stand-down waits. Doubles while the machine stays absent. */
  probeDelay: number;
  pending: PendingCreate[];
}

interface PendingCreate {
  readonly resolve: (entry: SessionEntry) => void;
  readonly reject: (error: Error) => void;
  readonly timer: TimerHandle;
}

const NO_RELAY = 'this deployment has no relay, so your machines cannot be reached from here';

export function liveDirectory(options: LiveDirectoryOptions): LiveDirectory {
  const clock = options.clock ?? systemClock;
  const reachTimeoutMs = options.reachTimeoutMs ?? REACH_TIMEOUT_MS;
  const probeMinMs = options.probeMinMs ?? PROBE_MIN_MS;
  const probeMaxMs = options.probeMaxMs ?? PROBE_MAX_MS;
  const createTimeoutMs = options.createTimeoutMs ?? CREATE_TIMEOUT_MS;

  // The whole record is replaced on every write rather than a nested field
  // mutated. A top-level property assignment is the one thing every component
  // in this app already relies on being reactive (`Fleet.tsx` does exactly
  // this), and a fleet is a handful of machines — the copy is not a cost worth
  // trading a reactivity assumption for.
  const state = signal<{ hosts: Record<string, HostSnapshot>; order: readonly string[] }>({
    hosts: {},
    order: [],
  });

  const watches = new Map<string, Watch>();

  const patch = (id: string, next: Partial<HostSnapshot>): void => {
    const current = state.hosts[id];
    // A push that lands while the machine is being dropped from the account.
    // Re-creating the row here would resurrect a revoked machine in the list.
    if (current === undefined) return;
    state.hosts = { ...state.hosts, [id]: { ...current, ...next } };
  };

  const clearDeadline = (w: Watch): void => {
    if (w.deadline === null) return;
    clock.cancel(w.deadline);
    w.deadline = null;
  };

  const armDeadline = (w: Watch): void => {
    if (w.deadline !== null) return;
    w.deadline = clock.schedule(() => {
      w.deadline = null;
      standDown(w);
    }, reachTimeoutMs);
  };

  const failCreates = (w: Watch, message: string): void => {
    for (const p of w.pending.splice(0)) {
      clock.cancel(p.timer);
      p.reject(new Error(message));
    }
  };

  /**
   * Stop trying, and start probing.
   *
   * Reached only from a timer, never from inside a `ConnectionClient` event.
   * That is not tidiness: `close()` mid-callback lands in the middle of the
   * client's own dial bookkeeping, and the redial it is part-way through
   * setting up would still open a socket after the close.
   */
  const standDown = (w: Watch): void => {
    clearDeadline(w);
    w.link?.close();
    w.link = null;
    failCreates(w, 'that machine stopped answering');
    // The sessions go with the link. An empty list under an "asleep" banner is
    // honest; the list as it was ten minutes ago, under the same banner, is
    // not — the same rule `SessionDirectory.setLink` applies on loopback.
    patch(w.host.id, { presence: { kind: 'offline' }, sessions: [], facts: null });
    const delay = w.probeDelay;
    w.probeDelay = Math.min(w.probeDelay * 2, probeMaxMs);
    if (w.probe !== null) clock.cancel(w.probe);
    w.probe = clock.schedule(() => {
      w.probe = null;
      open(w);
    }, delay);
  };

  const eventsFor = (w: Watch): ConnectionEvents => ({
    onConnection: (connection) => {
      switch (connection.phase) {
        case 'connecting':
          patch(w.host.id, { presence: { kind: 'probing' } });
          break;
        case 'awaiting-approval':
          // The deadline stands: a person walking to the machine to approve a
          // code takes longer than any timeout worth having, so a pairing
          // request is not a machine that failed to answer.
          clearDeadline(w);
          patch(w.host.id, {
            presence: { kind: 'pairing', code: connection.code },
            sessions: [],
            facts: null,
          });
          break;
        case 'connected':
          clearDeadline(w);
          // Reset here rather than on the first successful *probe*: what earns
          // a machine its short retry budget back is having answered, not
          // having been asked.
          w.probeDelay = probeMinMs;
          patch(w.host.id, { presence: { kind: 'online' } });
          break;
        case 'reconnecting':
          // Fires both when the link drops and when the ladder redials, so the
          // arming is guarded rather than the event.
          armDeadline(w);
          failCreates(w, 'the connection to that machine dropped');
          patch(w.host.id, { presence: { kind: 'probing' }, sessions: [], facts: null });
          break;
        case 'failed': {
          // The handshake said no and said it is not retryable — a denied
          // device, a protocol version that will not change by trying again.
          // Not a stand-down: a probe would ask the same question and get the
          // same answer, and the message is the only useful thing here.
          clearDeadline(w);
          // Closed as well as forgotten. `ConnectionClient` stops of its own
          // accord here — it schedules no redial — but leaving it merely
          // dormant would make `close()` below a no-op for it, and "no
          // connection" is a state worth being able to state exactly.
          w.link?.close();
          w.link = null;
          failCreates(w, connection.message);
          patch(w.host.id, {
            presence: { kind: 'failed', message: connection.message },
            sessions: [],
            facts: null,
          });
          break;
        }
      }
    },
    // Its own event, for the reason `ConnectionEvents` gives: an absent offer
    // means "nothing new to say", so this fires only when there is something.
    onHostOffer: (offer) => {
      patch(w.host.id, { facts: hostFactsOf(offer) });
    },
    onSessions: (sessions, created) => {
      const entries = sessions.map(sessionEntryOf);
      patch(
        w.host.id,
        created === null
          ? { sessions: entries }
          : { sessions: entries, lastCreated: created.toString() },
      );
      if (created === null) return;
      // `created` is set only on the listing that answers *this* connection's
      // own create, so there is exactly one waiter it can belong to. Which one
      // is unknowable — the daemon does not echo a request id — so it is the
      // oldest, which is the only ordering the client can defend.
      const waiter = w.pending.shift();
      if (waiter === undefined) return;
      clock.cancel(waiter.timer);
      const mine = entries.find((e) => e.session === created.toString());
      if (mine === undefined) waiter.reject(new Error('the created session was not in the listing'));
      else waiter.resolve(mine);
    },
    onError: (message) => {
      // A protocol-level error from the daemon. It does not end the connection
      // — the watch keeps running — but it is very likely the answer to a
      // create that will otherwise sit until it times out.
      failCreates(w, message);
    },
  });

  const open = (w: Watch): void => {
    const link = options.openLink(w.host, eventsFor(w));
    if (link === null) {
      patch(w.host.id, {
      presence: { kind: 'failed', message: NO_RELAY },
      sessions: [],
      facts: null,
    });
      return;
    }
    // Set before `connect()`, which emits its first connection event
    // synchronously — an event that finds `w.link` still null would be talking
    // about a watch that does not exist yet.
    w.link = link;
    patch(w.host.id, { presence: { kind: 'probing' }, sessions: [], facts: null });
    armDeadline(w);
    link.connect();
  };

  const closeWatch = (w: Watch): void => {
    clearDeadline(w);
    if (w.probe !== null) clock.cancel(w.probe);
    w.probe = null;
    w.link?.close();
    w.link = null;
    failCreates(w, 'that machine is no longer being watched');
  };

  const statusFor = (hostId: string): DirectoryStatus => {
    const snapshot = state.hosts[hostId];
    if (snapshot === undefined) {
      return { kind: 'error', message: 'this machine is not in your account' };
    }
    const presence = snapshot.presence;
    switch (presence.kind) {
      case 'probing':
        return { kind: 'pending' };
      case 'pairing':
        return { kind: 'pairing', code: presence.code };
      case 'offline':
        return { kind: 'offline' };
      case 'failed':
        return { kind: 'error', message: presence.message };
      case 'online':
        return {
          kind: 'ready',
          view: {
            connected: true,
            host: snapshot.host,
            sessions: snapshot.sessions,
            facts: snapshot.facts,
            // Always `relay`: a hosted `https://` page may not open a `ws://`
            // address on someone's LAN, so there is no other plane this path
            // could name. `dialFor` is what turns it back into a socket.
            dataPlane: { kind: 'relay', hostId: snapshot.host.id },
            lastCreated: snapshot.lastCreated,
          },
        };
    }
  };

  return {
    setHosts(hosts: readonly LiveHost[]): void {
      const next: Record<string, HostSnapshot> = {};
      for (const h of hosts) {
        const current = state.hosts[h.id];
        next[h.id] =
          current === undefined
            ? {
                host: { id: h.id, label: h.label },
                presence: { kind: 'probing' },
                sessions: [],
                facts: null,
                lastCreated: null,
              }
            : { ...current, host: { id: h.id, label: h.label } };
      }
      state.hosts = next;
      state.order = hosts.map((h) => h.id);

      for (const [id, w] of [...watches]) {
        if (next[id] !== undefined) continue;
        closeWatch(w);
        watches.delete(id);
      }
      for (const h of hosts) {
        if (watches.has(h.id)) continue;
        const w: Watch = {
          host: h,
          link: null,
          deadline: null,
          probe: null,
          probeDelay: probeMinMs,
          pending: [],
        };
        watches.set(h.id, w);
        open(w);
      }
    },

    sourceFor(hostId: string): DirectorySource {
      return () => () => statusFor(hostId);
    },

    statusFor,

    snapshots(): readonly HostSnapshot[] {
      const hosts = state.hosts;
      return state.order.flatMap((id) => {
        const snapshot = hosts[id];
        return snapshot === undefined ? [] : [snapshot];
      });
    },

    createSession(hostId: string, size: { cols: number; rows: number }): Promise<SessionEntry> {
      const w = watches.get(hostId);
      const snapshot = state.hosts[hostId];
      if (w === undefined || w.link === null || snapshot?.presence.kind !== 'online') {
        return Promise.reject(new Error('that machine is not connected'));
      }
      const link = w.link;
      return new Promise<SessionEntry>((resolve, reject) => {
        const timer = clock.schedule(() => {
          const index = w.pending.findIndex((p) => p.timer === timer);
          if (index >= 0) w.pending.splice(index, 1);
          reject(new Error('that machine did not answer the create in time'));
        }, createTimeoutMs);
        w.pending.push({ resolve, reject, timer });
        // The watching connection, not a new one — the whole reason this lives
        // here rather than in `create-session.ts`.
        link.createSession({ command: '', cwd: '', cols: size.cols, rows: size.rows });
      });
    },

    close(): void {
      for (const w of watches.values()) closeWatch(w);
      watches.clear();
      state.hosts = {};
      state.order = [];
    },
  };
}

/**
 * The production `OpenLink`: a real `ConnectionClient` over the relay.
 *
 * `expectedHost` is the machine's id from the account, which is the whole
 * point of having an account: the browser pins the key it expects the daemon
 * to prove before it signs anything for it. Nothing on the relay path is
 * trusted to name the machine — the relay routes to a room, and a room is not
 * a proof.
 */
export function relayLinks(
  signer: ClientSigner,
  relay: RelayAccess | null,
  label = 'zesterm-web',
): OpenLink {
  return (host, events) => {
    const dial = dialFor({ kind: 'relay', hostId: host.id }, relay);
    if (dial === null) return null;
    return new ConnectionClient({ dial, signer, label, expectedHost: host.id, events });
  };
}

/**
 * The redial ladder's own arithmetic, so a test can hold `REACH_TIMEOUT_MS` to
 * it rather than to a number someone typed.
 *
 * Returns how long `ConnectionClient` spends getting to its `n`th redial.
 */
export function ladderElapsedMs(attempts: number): number {
  let total = 0;
  for (let i = 1; i <= attempts; i++) {
    total += Math.min(REDIAL_MIN_MS * 2 ** (i - 1), REDIAL_MAX_MS);
  }
  return total;
}

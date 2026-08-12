/**
 * The fleet's session list, as a virtual actor.
 *
 * ADR-005's line, held in code: actors are the **control plane** — session
 * list, cwd, where the data plane lives — and never carry a grid byte. The
 * sidecar's daemon feed is this actor's only writer; every UI is a live
 * reader. The actor never computes anything about a session, because the
 * daemon owns that truth and pushes it whole.
 *
 * Key is `'local'` in v1 — one daemon per sidecar. When the fleet arrives,
 * the key becomes the `HostId` and the same shape scales to one directory
 * actor per host, which is exactly what virtual actors are for.
 *
 * State is deliberately **never persisted**: a directory restored from disk
 * would show ghost sessions until the feed reconnects, and "possibly stale
 * list" is strictly worse than "connecting…". The feed re-primes on connect,
 * so activation-fresh state costs one push.
 */

import { defineActor } from '@sigx/actors';

/**
 * `SessionInfo`, projected to plain JSON. `SessionId` is a `bigint` in the
 * proto package; the actor wire is `@sigx/serialize` JSON and the UI only
 * compares and displays ids, so a string is honest and portable.
 */
export interface SessionEntry {
  readonly host: string;
  readonly session: string;
  readonly title: string;
  readonly cwd: string;
  readonly cols: number;
  readonly rows: number;
  /** What the phone's blocks-first view switches on. */
  readonly altScreen: boolean;
  readonly attached: boolean;
}

/**
 * A daemon's session listing, projected into the entry a directory holds.
 *
 * Beside `SessionEntry` rather than beside either writer, because there are
 * now two of them: the sidecar's feed on loopback, and — on the hosted path,
 * which has no sidecar — the browser's own connection per machine. Two copies
 * of one projection is how the two worlds start disagreeing about what a
 * session is, and the disagreement would show up as a field that is blank in
 * one client and right in the other.
 *
 * The parameter is **structural**, not `@zesterm/proto`'s `SessionInfo`. This
 * package depends on `@sigx/actors` and nothing else on purpose; a dependency
 * added for one argument's type is a dependency every actors host then
 * carries. The shape is `SessionInfo`'s, and a drift in it fails at both call
 * sites rather than here.
 */
export function sessionEntryOf(info: {
  readonly addr: { readonly host: string; readonly session: bigint };
  readonly title: string;
  readonly cwd: string;
  readonly cols: number;
  readonly rows: number;
  readonly alt_screen: boolean;
  readonly attached: boolean;
}): SessionEntry {
  return {
    host: info.addr.host,
    // A `bigint` on the wire; a string here, for the reason `SessionEntry`
    // gives — the actor wire is JSON and the UI only compares and displays.
    session: info.addr.session.toString(),
    title: info.title,
    cwd: info.cwd,
    cols: info.cols,
    rows: info.rows,
    altScreen: info.alt_screen,
    attached: info.attached,
  };
}

export interface HostInfo {
  /** 64 hex chars. */
  readonly id: string;
  readonly label: string;
}

/**
 * How a browser reaches the grid, in one of the two ways it can.
 *
 * `ws` is the daemon's own `--listen-ws` address — its port, never the
 * sidecar's. `relay` names a host to be reached through the edge instead, and
 * carries no address at all: a laptop behind NAT has none to advertise, which
 * is the whole reason the relay exists (ADR-009). A hosted `https://` page
 * also may not open `ws://192.168.1.5:7718` at all — mixed content — so the
 * two shapes are not two ways of saying the same thing, and code that turns a
 * `DataPlane` into a URL must switch rather than read fields off it.
 */
export type DataPlane =
  | { readonly kind: 'ws'; readonly host: string; readonly port: number }
  | { readonly kind: 'relay'; readonly hostId: string };

export interface DirectoryState {
  v: 1;
  /** Whether the sidecar currently holds its daemon connection. */
  connected: boolean;
  host: HostInfo | null;
  sessions: SessionEntry[];
  /**
   * How a browser reaches the grid. Learned from the control plane rather
   * than hardcoded — the seam the daemon's `--listen-ws` address plugs into.
   */
  dataPlane: DataPlane | null;
  /** The newest session this connection created, for select-on-create UIs. */
  lastCreated: string | null;
}

export interface DirectoryView {
  readonly connected: boolean;
  readonly host: HostInfo | null;
  readonly sessions: readonly SessionEntry[];
  readonly dataPlane: DataPlane | null;
  readonly lastCreated: string | null;
}

/**
 * v1 posture, stated where it bites: `allowAnonymous` because the sidecar
 * binds loopback and serves same-origin, so reaching this socket is the same
 * authority the daemon's own loopback socket accepts. The write methods being
 * wire-callable is a cosmetic-corruption risk only — the next daemon push
 * overwrites. The tightening (a policy allowing only `list` for wire
 * principals) lands with device enrollment; see docs/design/phone/README.md.
 */
export const SessionDirectory = defineActor({
  type: 'SessionDirectory',
  allowAnonymous: true,
  state: (): DirectoryState => ({
    v: 1,
    connected: false,
    host: null,
    sessions: [],
    dataPlane: null,
    lastCreated: null,
  }),
  methods: (ctx) => ({
    /** The one read; `useActorState(..., 'list', { live: true })` rides it. */
    async list(): Promise<DirectoryView> {
      return {
        connected: ctx.state.connected,
        host: ctx.state.host,
        sessions: ctx.state.sessions,
        dataPlane: ctx.state.dataPlane,
        lastCreated: ctx.state.lastCreated,
      };
    },

    /**
     * Feed-only: the daemon pushed a complete listing. Replace, never merge —
     * the daemon's list is the truth and a merge would resurrect closed
     * sessions.
     */
    async replaceAll(sessions: SessionEntry[], created: string | null): Promise<void> {
      ctx.state.sessions = sessions;
      if (created !== null) ctx.state.lastCreated = created;
      // No ctx.save(), here or anywhere: see the module doc.
    },

    /** Feed-only: the daemon link came up or went down. */
    async setLink(
      connected: boolean,
      host: HostInfo | null,
      dataPlane: DataPlane | null,
    ): Promise<void> {
      ctx.state.connected = connected;
      if (host !== null) ctx.state.host = host;
      if (dataPlane !== null) ctx.state.dataPlane = dataPlane;
      if (!connected) {
        // A disconnected feed means the list can no longer be trusted; an
        // empty list under a "disconnected" banner beats a stale one under
        // none.
        ctx.state.sessions = [];
      }
    },
  }),
});

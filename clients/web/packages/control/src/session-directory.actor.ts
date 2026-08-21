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
 * `SessionInfo`, projected to plain JSON. The id stays a string even though
 * `SessionId` decodes as a plain `number` since #14: the actor wire already
 * says string — a frozen seam — and the UI only compares and displays ids,
 * so nothing is gained by moving it.
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
  readonly addr: { readonly host: string; readonly session: number };
  readonly title: string;
  readonly cwd: string;
  readonly cols: number;
  readonly rows: number;
  readonly alt_screen: boolean;
  readonly attached: boolean;
}): SessionEntry {
  return {
    host: info.addr.host,
    // A number off the wire; a string here, for the reason `SessionEntry`
    // gives — the actor wire said string before #14 and it is a frozen seam.
    session: info.addr.session.toString(),
    title: info.title,
    cwd: info.cwd,
    cols: info.cols,
    rows: info.rows,
    altScreen: info.alt_screen,
    attached: info.attached,
  };
}

/**
 * One thing a machine can launch, as the launcher needs it (design §12 calls
 * profiles launch targets, which is what this is).
 *
 * **Projected, not `@zesterm/proto`'s `HostProfile`** — the same rule
 * `sessionEntryOf` states above: this package depends on `@sigx/actors` and
 * nothing else, and the actor wire is JSON. Renamed for the same reason
 * `SessionInfo` became `SessionEntry`: the wire shape and the projection are
 * two representations of one concept, and giving them one name is how a
 * `snake_case` field ends up read off a `camelCase` object.
 *
 * No `host` and no `askHost`: a profile a machine publishes is pinned to that
 * machine by construction (ADR-014). Which machine it belongs to is the key
 * it was stored under, never a field on it.
 */
export interface LaunchTarget {
  readonly name: string;
  /** Already resolved through *that* machine's `profiles.defaults`. */
  readonly command: string;
  readonly startingDirectory: string;
  readonly icon: string;
  readonly colorScheme: string;
  /** A palette slot, or null when the profile names no colour. */
  readonly tabColor: number | null;
}

/**
 * What a machine says it is, and what it can launch (#262).
 *
 * Every string may be empty: a daemon that cannot answer one of these sends
 * `""` rather than omitting the field, and a UI showing a fact must check.
 * "An os row we cannot fill would be a dash pretending to be a fact" is the
 * native app's rule for the same data, and it applies here.
 */
export interface HostFacts {
  readonly os: string;
  readonly arch: string;
  readonly osVersion: string;
  readonly defaultShell: string;
  readonly launchTargets: readonly LaunchTarget[];
}

/**
 * `HostOffer`, projected. Structural for the same reason `sessionEntryOf`'s
 * parameter is: the shape is the wire's, and a drift in it fails at the call
 * sites rather than here.
 */
export function hostFactsOf(offer: {
  readonly os: string;
  readonly arch: string;
  readonly os_version: string;
  readonly default_shell: string;
  readonly profiles: readonly {
    readonly name: string;
    readonly command: string;
    readonly starting_directory: string;
    readonly icon: string;
    readonly color_scheme: string;
    readonly tab_color: number | null;
  }[];
}): HostFacts {
  return {
    os: offer.os,
    arch: offer.arch,
    osVersion: offer.os_version,
    defaultShell: offer.default_shell,
    launchTargets: offer.profiles.map((p) => ({
      name: p.name,
      command: p.command,
      startingDirectory: p.starting_directory,
      icon: p.icon,
      colorScheme: p.color_scheme,
      tabColor: p.tab_color,
    })),
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
  /**
   * What this machine says it is and can launch, or null while it has not
   * said. Sticky across ordinary session pushes — an absent offer means
   * "nothing new to say", so clearing on every push would empty the launcher
   * between config edits (#352).
   */
  facts: HostFacts | null;
  /** The newest session this connection created, for select-on-create UIs. */
  lastCreated: string | null;
}

export interface DirectoryView {
  readonly connected: boolean;
  readonly host: HostInfo | null;
  readonly sessions: readonly SessionEntry[];
  readonly dataPlane: DataPlane | null;
  readonly facts: HostFacts | null;
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
    facts: null,
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
        facts: ctx.state.facts,
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

    /**
     * Feed-only: the daemon said what it offers (#352).
     *
     * Separate from `replaceAll` because the offer rides *some* session pushes
     * and not others, and folding the two together would make every caller
     * re-check which half is real. A push with nothing new to say simply does
     * not call this.
     */
    async setFacts(facts: HostFacts): Promise<void> {
      ctx.state.facts = facts;
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
        // The facts go with it, and for a stronger reason than the sessions:
        // a launch target still drawn for a machine that is not answering is
        // a row that must fail when pressed. The next connection republishes
        // on its first listing, so this costs nothing but a blank menu while
        // the link is down.
        ctx.state.facts = null;
      }
    },
  }),
});

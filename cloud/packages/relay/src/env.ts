/**
 * The Worker's bindings, in one place.
 *
 * Secrets are `wrangler secret put`, never `vars` in `wrangler.jsonc` — that
 * file is committed.
 */

/** One object per host. `idFromName('host:' + hostId)` — → ADR-009. */
export interface RelayRoomNamespace {
  idFromName(name: string): RelayRoomId;
  get(id: RelayRoomId): RelayRoomStub;
}

export interface RelayRoomId {
  toString(): string;
}

export interface RelayRoomStub {
  fetch(request: Request): Promise<Response>;
}

/**
 * Enough of D1 to name the binding, and no more.
 *
 * The relay makes no query yet. When it does, the narrow subset it needs is
 * declared here rather than imported from `@cloudflare/workers-types`, for the
 * reason `packages/web/src/db/types.ts` gives: a store typed against a
 * structural subset can be tested with a plain object.
 */
export interface D1Binding {
  prepare(query: string): unknown;
}

export interface Env {
  readonly RELAY_ROOM: RelayRoomNamespace;
  /** D1 `zesterm` — the same database the web Worker owns. */
  readonly DB: D1Binding;

  /**
   * The public keys an attach ticket may be signed by, comma-separated hex.
   *
   * **A list rather than one key, and that is a decision.** With a single key,
   * rotating it means deploying the web Worker and this one atomically, which
   * is not something two `wrangler deploy`s can be — there is always a window
   * where one has the new key and the other does not, and every attach in the
   * fleet fails during it. With a list the new key is added here first, the
   * mint switches to it, and the old one is removed afterwards.
   *
   * Optional, because a relay with no keys configured refuses every attach and
   * that is the correct behaviour for a deployment where the mint has not been
   * set up — as opposed to a startup check, which would take the whole Worker
   * down for it. Not a secret (these are public keys), but bound the same way
   * as one so a rotation is a `wrangler secret put` rather than a commit.
   */
  readonly TICKET_PUBLIC_KEYS?: string;
}

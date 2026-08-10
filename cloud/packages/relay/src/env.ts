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
}

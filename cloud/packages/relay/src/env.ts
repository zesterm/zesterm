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

export interface D1PreparedStatement {
  bind(...values: unknown[]): D1PreparedStatement;
  first<T>(): Promise<T | null>;
  run(): Promise<unknown>;
}

/**
 * Enough of D1 for the one lookup the relay makes, and no more.
 *
 * Declared here rather than imported from `@cloudflare/workers-types`, for the
 * reason `packages/web/src/db/types.ts` gives: a store typed against a
 * structural subset can be tested with a plain object. Deliberately without
 * `all` or `batch` — `room/hosts.ts` reads one row and writes one column, and
 * a wider surface here would be an invitation to put more of the fleet's state
 * on the attach path.
 */
export interface D1Binding {
  prepare(query: string): D1PreparedStatement;
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

  /**
   * This relay's own Ed25519 seed, 64 hex characters. The public half is the
   * `relay_key` in every challenge.
   *
   * **A key pair rather than a configured public key**, and the difference is
   * what the field is for. The daemon side does not exist yet; when it does, it
   * is to remember the `relay_key` from its first control link and refuse a
   * later one presenting a different key, which is only worth anything if the
   * relay can eventually *prove* the pin rather than assert it.
   *
   * **This build proves nothing.** It publishes the public half and signs
   * nothing with it, so the binding a daemon would be pinning today rests on
   * TLS to the relay's origin and on nothing else. The proof is one signature
   * away and needs a nonce from the daemon, which this handshake does not
   * carry. Deriving the published key from a secret now is what makes adding
   * that a protocol change later, rather than a key rotation across every
   * machine in the fleet.
   *
   * Optional, and the same decision `TICKET_PUBLIC_KEYS` records: a relay whose
   * key has not been set up refuses every control link, which is correct and
   * obvious, where a startup check would take the whole Worker down for it.
   * A secret — `wrangler secret put` — because the private half is in it.
   */
  readonly RELAY_SIGNING_KEY?: string;
}

/**
 * The Durable Object surface the room uses, declared rather than imported.
 *
 * `@cloudflare/workers-types` has `DurableObjectState` and `WebSocket`, but
 * naming a structural subset here buys the thing that matters: the room's
 * handlers run against a plain object under `node --test`, with no workerd and
 * no miniflare. `packages/web/src/db/types.ts` does the same for D1 and says
 * why at greater length.
 *
 * It matters more here than it does there, because of what the tests have to
 * prove. **The object is evicted between messages**, so nothing may live in
 * instance fields; the byte pump has to re-derive its pairing from
 * `getWebSockets` every single time. A `Map<WebSocket, WebSocket>` held in
 * memory passes every test written against a single live instance and drops
 * sessions in production after the first idle gap. The guard ADR-009 asks for
 * is the suite run twice, the second time constructing a new instance before
 * every handler call — and that is only cheap if constructing the state is
 * cheap, which is only true if the state is one of these.
 *
 * Deliberately the *narrow* subset. `setWebSocketAutoResponse`, alarms and
 * `blockConcurrencyWhile` are all absent because nothing uses them yet; adding
 * one is a visible decision rather than a discovery.
 */

/**
 * A WebSocket, as the room sees one.
 *
 * Only ever obtained from `getWebSockets` or handed to `acceptWebSocket` — the
 * room never holds one across a handler call, for the reason above.
 */
export interface Sock {
  send(message: string | ArrayBuffer): void;
  close(code?: number, reason?: string): void;
  /**
   * Survives hibernation; bounded by `MAX_ATTACHMENT_BYTES` **after
   * serialization**. The platform stores a snapshot, not a reference, so a
   * later mutation of the value passed here is not visible to the next
   * `deserializeAttachment`.
   */
  serializeAttachment(value: unknown): void;
  deserializeAttachment(): unknown;
}

/**
 * The object's own storage.
 *
 * Present for the attach ticket's replay set, which is the one thing that has
 * to persist. **Not for the data path** — a write there costs a request per
 * message and, worse, keeps the object awake, which converts the dominant cost
 * term from zero into continuous. The fake counts writes so that stays
 * assertable.
 */
export interface RoomStorage {
  get<T>(key: string): Promise<T | undefined>;
  put(key: string, value: unknown): Promise<void>;
  delete(key: string): Promise<boolean>;
}

export interface RoomState {
  readonly storage: RoomStorage;
  /**
   * At most `MAX_TAGS_PER_SOCKET` distinct tags, each at most
   * `MAX_TAG_LENGTH` characters. Both throw rather than truncate.
   */
  acceptWebSocket(ws: Sock, tags?: string[]): void;
  /** Every accepted socket, or those carrying `tag`. This is the pairing. */
  getWebSockets(tag?: string): Sock[];
  getTags(ws: Sock): string[];
}

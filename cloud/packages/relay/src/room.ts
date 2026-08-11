/**
 * The Durable Object. One per host, and the only place a daemon and a browser
 * ever meet.
 *
 * It speaks no protocol yet: it accepts the socket and hangs up. What is real
 * already is the shape — the room is written against `RoomState` rather than
 * against `DurableObjectState`, which is what lets the eviction discipline
 * ADR-009 requires be tested at all. Nothing may live in instance fields,
 * because the object is evicted between messages and tags and attachments are
 * what survive.
 *
 * `fetch` itself has no unit test and cannot have one: `WebSocketPair` is a
 * workerd global with no standalone equivalent, so the upgrade half of this
 * class is only ever exercised by running the bundle. The pipe logic that
 * follows it is written against `RoomState` for exactly that reason — the
 * fake platform can drive everything after the socket exists.
 */

import type { Env } from './env.ts';
import type { RoomState } from './room/state.ts';

/**
 * Normal closure. Not 1011: nothing failed — this build of the relay simply has
 * no pipe to offer, and a client that retries a 1011 forever would be right to.
 */
const CLOSE_NORMAL = 1000;

/**
 * A compile-time proof that the hand-declared `RoomState` really is a subset of
 * the platform's `DurableObjectState`.
 *
 * `room/state.ts` is declared rather than imported so the tests need no
 * workerd. The failure that buys is a declared subset which has drifted from
 * the real interface — tests passing against a shape production does not have,
 * which is worse than having no tests. This alias costs nothing and fails the
 * typecheck the moment the two disagree.
 */
type Subset<Narrow, Wide extends Narrow> = Wide;
export type _PlatformIsARoomState = Subset<RoomState, DurableObjectState>;

export class RelayRoom {
  readonly #state: RoomState;

  constructor(state: RoomState, _env: Env) {
    this.#state = state;
  }

  async fetch(request: Request): Promise<Response> {
    if (request.headers.get('upgrade')?.toLowerCase() !== 'websocket') {
      return new Response('expected a WebSocket upgrade', { status: 426 });
    }

    const { 0: client, 1: server } = new WebSocketPair();
    // The hibernatable accept, not `server.accept()`, even though this socket
    // is closed immediately. The two differ in whether the object stays awake
    // holding the connection, and picking the cheap one only once there is
    // traffic to justify it is how a room ends up never hibernating.
    this.#state.acceptWebSocket(server);
    server.close(CLOSE_NORMAL, 'the relay speaks no protocol yet');

    return new Response(null, { status: 101, webSocket: client });
  }
}

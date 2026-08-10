/**
 * The entrypoint.
 *
 * It answers 404 to everything. The relay has no HTTP surface of its own and
 * will not grow one: what arrives here is a WebSocket upgrade destined for a
 * room, and until there is a ticket to name a room with, refusing is the whole
 * behaviour.
 *
 * The export that matters is `RelayRoom`. A Durable Object class must be
 * exported from the script named by `main`, and the migration in
 * `wrangler.jsonc` names it by string — `test/wrangler-config.test.ts` imports
 * this module and checks the two agree, because nothing else does.
 */

import type { Env } from './env.ts';

export { RelayRoom } from './room.ts';

export default {
  async fetch(): Promise<Response> {
    return new Response('not found', { status: 404 });
  },
};

export type { Env };

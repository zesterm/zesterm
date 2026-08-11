/**
 * The entrypoint, and the one route the relay has: `GET /v1/attach?host=<id>`.
 *
 * A browser arrives holding an attach ticket on `Sec-WebSocket-Protocol` and
 * asks to be joined to one host's room. This decides whether it may be, and
 * **decides it here rather than inside the Durable Object**: a refused attach
 * must not cost a room wake-up, or an unauthenticated caller can bill the
 * account by dialling garbage. The ticket is verified statelessly, so there is
 * nothing the object knows that this needs.
 *
 * What it does *not* do yet is join anything. The daemon's control link is a
 * later wave, so a ticket that verifies earns a `4404` — the room is real, the
 * holder is allowed in it, and there is nobody on the other side.
 *
 * **Every refusal is a WebSocket close code, not an HTTP status.** The browser
 * WebSocket API never surfaces the response status: a 401 reaches the page as
 * `onclose` with 1006, indistinguishable from the wifi dropping. A close code
 * survives, which is what lets `clients/web/packages/app/src/relay-dial.ts`
 * eventually tell "your ticket was refused" from "your Mac is asleep". It
 * treats both as a dropped dial today, and says so; the information is kept
 * because destroying it here would be permanent.
 *
 * The export that matters besides `fetch` is `RelayRoom`. A Durable Object
 * class must be exported from the script named by `main`, and the migration in
 * `wrangler.jsonc` names it by string — `test/wrangler-config.test.ts` imports
 * this module and checks the two agree, because nothing else does.
 */

import type { Env } from './env.ts';
import {
  RELAY_SUBPROTOCOL,
  ticketFromSubprotocols,
  ticketPublicKeys,
  verifyAttachTicket,
} from './ticket.ts';

export { RelayRoom } from './room.ts';

/** The only path this Worker answers. Versioned, because a second shape would be. */
export const ATTACH_PATH = '/v1/attach';

/**
 * The ticket was missing, malformed, expired, signed by a key we do not hold,
 * or minted for a different room. One code for all of them on purpose: telling
 * them apart is worth more to whoever is guessing than to the holder of a real
 * ticket, who has none of these problems.
 */
export const CLOSE_TICKET_REFUSED = 4401;

/** The room exists and the holder may enter it; the host is not there. */
export const CLOSE_HOST_ABSENT = 4404;

/**
 * What the edge should do with an attach request.
 *
 * A value rather than a `Response`, because `WebSocketPair` is a workerd global
 * with no standalone equivalent — so the whole decision is testable under
 * `node --test` and only the four lines that build the upgrade are not.
 * `room.ts` draws the same line and says so.
 */
export type AttachVerdict =
  | { readonly kind: 'http'; readonly status: number; readonly body: string }
  | { readonly kind: 'close'; readonly code: number; readonly reason: string };

export async function attachVerdict(
  request: Request,
  env: Env,
  now: number = Date.now(),
): Promise<AttachVerdict> {
  // Anything that is not an upgrade is a person or a probe, and an HTTP status
  // is the only thing either of them can read.
  if (request.headers.get('upgrade')?.toLowerCase() !== 'websocket') {
    return { kind: 'http', status: 426, body: 'expected a WebSocket upgrade' };
  }

  const ticket = ticketFromSubprotocols(request.headers.get('sec-websocket-protocol'));
  if (ticket === null) {
    return { kind: 'close', code: CLOSE_TICKET_REFUSED, reason: 'no attach ticket' };
  }

  // The room comes from the URL and the claim comes from the signed payload;
  // `verifyAttachTicket` refuses unless they are the same string. Neither is
  // trusted to be the other, which is the entire point of carrying both.
  const host = new URL(request.url).searchParams.get('host') ?? '';
  const verified = await verifyAttachTicket({
    text: ticket,
    host,
    keys: ticketPublicKeys(env.TICKET_PUBLIC_KEYS),
    now,
  });
  if (verified === null) {
    return { kind: 'close', code: CLOSE_TICKET_REFUSED, reason: 'attach ticket refused' };
  }

  return { kind: 'close', code: CLOSE_HOST_ABSENT, reason: 'no control link for this host' };
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    if (new URL(request.url).pathname !== ATTACH_PATH) {
      return new Response('not found', { status: 404 });
    }

    const verdict = await attachVerdict(request, env);
    if (verdict.kind === 'http') {
      return new Response(verdict.body, { status: verdict.status });
    }

    const { 0: client, 1: server } = new WebSocketPair();
    // `accept()`, not the room's `acceptWebSocket()`: this socket belongs to
    // the Worker and no Durable Object has been touched, which is the property
    // that keeps a refused attach from waking a room.
    server.accept();
    server.close(verdict.code, verdict.reason);

    // The selected subprotocol is echoed, and only the protocol name — never
    // the ticket token beside it. A server that selects nothing is legal, but
    // a client is entitled to abort on it, and the ticket must not appear in a
    // response header any intermediary might log.
    return new Response(null, {
      status: 101,
      webSocket: client,
      headers: { 'sec-websocket-protocol': RELAY_SUBPROTOCOL },
    });
  },
};

export type { Env };

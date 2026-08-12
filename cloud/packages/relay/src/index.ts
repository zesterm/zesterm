/**
 * The entrypoint, and the three routes the relay has.
 *
 * `GET /v1/control?host=<id>` is the daemon's. It has no edge-checkable
 * credential — the thing that proves a machine is itself is a signature over a
 * nonce the object has to have generated — so the room is woken and the
 * handshake happens inside it.
 *
 * That asymmetry is worth stating rather than leaving to be discovered: a
 * well-formed 64-hex `?host=` is free to invent, so anyone can wake a room by
 * dialling this route, and the syntax check below only stops the ids that name
 * no machine at all. The object refuses within one message and writes no
 * storage doing it, which bounds what each attempt costs but not how many
 * there can be. ADR-009 gives the relay a rate limit; it is not built, and this
 * is the route that will need it first.
 *
 * `GET /v1/attach?host=<id>` is the browser's. A browser arrives holding an
 * attach ticket on `Sec-WebSocket-Protocol`, and that ticket is verified
 * **here rather than inside the Durable Object**: a refused attach must not
 * cost a room wake-up, or an unauthenticated caller can bill the account by
 * dialling garbage. Everything a signature settles is settled statelessly, so
 * there is nothing the object knows that this needs. Only a ticket that
 * verifies reaches the room — where it is spent (a ticket admits one attach,
 * and the set of spent ids is the one thing only a single-instance object can
 * hold → `room/replay.ts`) and then either becomes a pipe or is told which of
 * the two things went wrong: nobody is home, or the host is home and did not
 * dial back.
 *
 * `GET /v1/pipe?host=<id>&pipe=<id>` is the daemon's second connection, made
 * because the room asked it to. The id is the credential and there is no other:
 * 128 bits, published once down an authenticated control link, dialable for ten
 * seconds. → `room/pipe.ts`.
 *
 * **Every refusal a browser can see is a WebSocket close code, not an HTTP
 * status.** The browser WebSocket API never surfaces the response status: a 401
 * reaches the page as `onclose` with 1006, indistinguishable from the wifi
 * dropping. A close code survives, which is what lets
 * `clients/web/packages/app/src/relay-dial.ts` eventually tell "your ticket was
 * refused" from "your Mac is asleep". It treats every close as a dropped dial
 * today, and says so; the information is kept because destroying it here would
 * be permanent.
 *
 * A malformed *request* is still a status, on both routes, exactly as `426`
 * already was — that is not a refusal, and whoever sent one is a probe, a
 * person, or a daemon, all of which can read it.
 *
 * The export that matters besides `fetch` is `RelayRoom`. A Durable Object
 * class must be exported from the script named by `main`, and the migration in
 * `wrangler.jsonc` names it by string — `test/wrangler-config.test.ts` imports
 * this module and checks the two agree, because nothing else does.
 */

import { fromHex } from '@zesterm/cloud-shared';

import type { Env } from './env.ts';
import { ATTACH_JTI_PARAM, ATTACH_PATH, CONTROL_PATH, PIPE_PATH, roomName } from './routes.ts';
import { HOST_ID_BYTES } from './room/control.ts';
import { pipeIdIsWellFormed } from './room/pipe.ts';
import {
  RELAY_SUBPROTOCOL,
  ticketFromSubprotocols,
  ticketPublicKeys,
  verifyAttachTicket,
} from './ticket.ts';

export { RelayRoom } from './room.ts';
export { ATTACH_PATH, CONTROL_PATH, PIPE_PATH } from './routes.ts';
// The room's refusals live with the room. Re-exported because every code a
// *browser* can be given belongs beside `CLOSE_TICKET_REFUSED` in anyone's head.
export { CLOSE_HOST_ABSENT } from './room/control.ts';
export {
  CLOSE_PIPE_DIAL_TIMEOUT,
  CLOSE_PIPE_FLOOD,
  CLOSE_PIPE_PEER_GONE,
  CLOSE_TICKET_REPLAYED,
} from './room/pipe.ts';

/**
 * The ticket was missing, malformed, expired, signed by a key we do not hold,
 * or minted for a different room. One code for all of them on purpose: telling
 * them apart is worth more to whoever is guessing than to the holder of a real
 * ticket, who has none of these problems.
 */
export const CLOSE_TICKET_REFUSED = 4401;

/**
 * What the edge should do with an attach request.
 *
 * A value rather than a `Response`, because `WebSocketPair` is a workerd global
 * with no standalone equivalent — so the whole decision is testable under
 * `node --test` and only the lines that build the upgrade are not. `room.ts`
 * draws the same line and says so.
 */
export type AttachVerdict =
  | { readonly kind: 'http'; readonly status: number; readonly body: string }
  | { readonly kind: 'close'; readonly code: number; readonly reason: string }
  /**
   * Verified. Only now may a room be named, let alone woken.
   *
   * `jti` travels with the host because the room needs it and must not have to
   * re-derive it: it is the ticket's replay key, and the room refuses a second
   * attach carrying one it has already seen. That check cannot be made here —
   * the edge has no state, and two colos would not see each other's — so this
   * is the one claim that is verified in one place and *spent* in another.
   */
  | { readonly kind: 'room'; readonly host: string; readonly jti: string };

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

  return { kind: 'room', host: verified.host, jti: verified.jti };
}

/**
 * Echo the subprotocol, and only if it was offered.
 *
 * The conditional is not caution, it is the close code. Selecting a
 * subprotocol the client did not offer is invalid (RFC 6455 §4.1) and a browser
 * is entitled to fail the connection over it — which discards whatever close
 * code the room wrote and leaves `event.code` as 1006. Every refusal a browser
 * sees is a close code precisely because it cannot read an HTTP status, so
 * mangling the handshake to announce a protocol nobody asked for would destroy
 * the one channel that tells a person whether their machine is asleep or their
 * ticket was refused.
 *
 * Only the protocol name is echoed — never the ticket token beside it, which
 * must not reach a response header any intermediary might log.
 */
function subprotocolHeaders(request: Request): Record<string, string> {
  const offered = (request.headers.get('sec-websocket-protocol') ?? '')
    .split(',')
    .map((token) => token.trim());
  return offered.includes(RELAY_SUBPROTOCOL)
    ? { 'sec-websocket-protocol': RELAY_SUBPROTOCOL }
    : {};
}

/**
 * The attach request as the room sees it: the same request, with the verified
 * ticket's `jti` on the query string.
 *
 * A query parameter, like `?host=` — which is already the only thing binding
 * the object's name to a machine — and read back the same way. Both are the
 * *Worker's* word rather than the caller's, which is what makes that safe: a
 * Durable Object is reachable only through its binding, so nothing outside this
 * Worker can address the room at all, let alone choose what these say.
 *
 * Handing the room one verified field rather than the ticket keeps the check in
 * one place: this file decides whether a ticket is real, the room decides
 * whether it has been spent, and there is no second verifier to disagree with
 * the first. The `jti` is a replay key and not a secret (the mint says so where
 * it makes one), so unlike the ticket it costs nothing to put in a URL that
 * never leaves the edge.
 *
 * Exported for its test, which is the only thing that can check it: the `fetch`
 * that calls it is the one function here that cannot be unit-tested.
 */
export function roomRequest(request: Request, jti: string): Request {
  const url = new URL(request.url);
  // `set`, not an interpolated string: a `jti` is whatever our own mint signed,
  // and percent-encoding it here is what makes `searchParams.get` in the room
  // hand back the same characters rather than a truncated or re-split value.
  // `set` also replaces any the caller invented, rather than appending beside
  // it — `searchParams.get` returns the first, which would be theirs.
  url.searchParams.set(ATTACH_JTI_PARAM, jti);
  // The upgrade this carries is the point of the whole request, and it is the
  // headers that carry it — so the request is rebuilt from the original rather
  // than constructed field by field.
  const forwarded = new Request(url, request);
  // ...minus the one header that is a credential.
  //
  // `sec-websocket-protocol` carries the whole attach ticket, and every comment
  // around here says the room never sees it — which was not true while the
  // header rode along. The room has no use for it: the edge has already
  // verified the signature, the audience, the expiry and the room, and passes
  // on the one field it decided, the `jti`.
  //
  // Removing it is not tidiness. A ticket that reaches a component with no
  // reason to hold it is a ticket that can be logged by one, and this one is a
  // bearer credential good for thirty seconds against a live shell.
  forwarded.headers.delete('sec-websocket-protocol');
  return forwarded;
}

/** A socket this Worker owns, opened only to say why it is closing. */
function closeAtTheEdge(
  code: number,
  reason: string,
  headers: Record<string, string> = {},
): Response {
  const { 0: client, 1: server } = new WebSocketPair();
  // `accept()`, not the room's `acceptWebSocket()`: this socket belongs to the
  // Worker and no Durable Object has been touched, which is the property that
  // keeps a refused attach from waking a room.
  server.accept();
  server.close(code, reason);
  return new Response(null, { status: 101, webSocket: client, headers });
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    const url = new URL(request.url);

    // Both daemon routes. Neither carries an edge-checkable credential — the
    // control link's is a signature the object has to have challenged for, and
    // the pipe's is an id only the object knows it issued — so both are
    // syntax-checked here and decided inside.
    if (url.pathname === CONTROL_PATH || url.pathname === PIPE_PATH) {
      if (request.headers.get('upgrade')?.toLowerCase() !== 'websocket') {
        return new Response('expected a WebSocket upgrade', { status: 426 });
      }
      // The cheap check the object would otherwise be woken to make. A host id
      // is 64 lowercase hex or it names no machine at all, and `idFromName`
      // would happily mint a room for anything else.
      const host = url.searchParams.get('host') ?? '';
      if (fromHex(host, HOST_ID_BYTES) === null) {
        return new Response('?host= must be 64 lowercase hex', { status: 400 });
      }
      // The same argument one level down: an id that is not 32 lowercase hex
      // can never match a pipe this relay issued, so refusing it here is one
      // fewer wake-up anyone can bill the account for by guessing.
      if (
        url.pathname === PIPE_PATH &&
        !pipeIdIsWellFormed(url.searchParams.get('pipe') ?? '')
      ) {
        return new Response('?pipe= must be 32 lowercase hex', { status: 400 });
      }
      return env.RELAY_ROOM.get(env.RELAY_ROOM.idFromName(roomName(host))).fetch(request);
    }

    if (url.pathname !== ATTACH_PATH) {
      return new Response('not found', { status: 404 });
    }

    const verdict = await attachVerdict(request, env);
    if (verdict.kind === 'http') {
      return new Response(verdict.body, { status: verdict.status });
    }
    if (verdict.kind === 'close') {
      return closeAtTheEdge(verdict.code, verdict.reason, subprotocolHeaders(request));
    }

    const response = await env.RELAY_ROOM.get(
      env.RELAY_ROOM.idFromName(roomName(verdict.host)),
    ).fetch(roomRequest(request, verdict.jti));
    const socket = response.webSocket;
    if (socket === null) return response;
    // Rebuilt rather than returned as-is, because the subprotocol is the
    // Worker's business and not the room's: the room never sees the ticket and
    // must not have to know what token it travelled under.
    return new Response(null, {
      status: 101,
      webSocket: socket,
      headers: subprotocolHeaders(request),
    });
  },
};

export type { Env };

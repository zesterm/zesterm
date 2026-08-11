/**
 * The relay leg of the `Dial` seam: mint a ticket, then open the socket.
 *
 * The browser may not open a relay socket until it holds a short-lived attach
 * ticket, and the ticket rides on `Sec-WebSocket-Protocol` rather than the
 * query string — a secret in a URL lands in referrers, edge logs and history
 * (ADR-009). So a relay dial is asynchronous where `wsDial` is not.
 *
 * **`Dial` stays synchronous, and this is the argument, because the next
 * person here will want to make it a promise.** The shell below is returned
 * immediately and the socket appears behind it a round trip later. Nothing
 * above calls `send()` before `onOpen()`, so there is nothing to queue and no
 * second state machine to write. And a mint that fails calls `onClose()`,
 * which `SessionClient` cannot tell from a connection that dropped — that is
 * "one code path with reconnect, deliberately", and its 200ms→5s ladder
 * already retries this exact case. An async `Dial` would buy a distinction
 * nothing above wants, at the price of changing the seam every other platform
 * implements.
 *
 * **`mintTicket` is a parameter on purpose, not because the caller varies.**
 * There is one caller — `relay-access.ts`, which posts to `/api/relay/ticket` —
 * and injecting the mint is what let this land and be reviewed before that
 * endpoint existed. It stays injected because it is what makes every test here
 * possible with no network at all.
 *
 * **A v1 cut, named so it reads as one:** `ByteLinkHandlers` has no error
 * channel by design, so a 4401 (the ticket was refused) and a 4404 (the host
 * is not connected to its room) arrive here as the same `onClose()` and are
 * both retried. Telling them apart means widening the seam for every
 * transport; until a user is actually confused by a fleet member that is
 * merely asleep, retrying both is the honest behaviour.
 */

import type { ByteLink, ByteLinkHandlers, Dial } from '@zesterm/client';

/**
 * The subprotocol pair: the protocol name, then the ticket as a second token.
 * Two entries rather than one joined string, because a WebSocket subprotocol
 * token may not contain the separators a ticket might.
 */
const RELAY_SUBPROTOCOL = 'zesterm.relay.v1';

/**
 * The room to dial, from an `https://`/`http://` origin.
 *
 * `cloud/packages/relay/src/index.ts` is the other end and answers this exact
 * path. Versioned, because the relay has no other HTTP surface to put a version
 * in and a second attach shape would need one. The shape follows ADR-009's
 * one-object-per-host: the host id selects the room, and nothing secret is in
 * the URL — the ticket is a subprotocol, for the reason at the top of this file.
 */
function relayUrl(relayOrigin: string, hostId: string): string {
  const url = new URL('/v1/attach', relayOrigin);
  url.searchParams.set('host', hostId);
  if (url.protocol === 'https:') url.protocol = 'wss:';
  else if (url.protocol === 'http:') url.protocol = 'ws:';
  return url.toString();
}

export function relayDial(
  relayOrigin: string,
  hostId: string,
  mintTicket: () => Promise<string>,
): Dial {
  return (handlers: ByteLinkHandlers): ByteLink => {
    let socket: WebSocket | null = null;
    let closed = false;

    void mintTicket()
      .then((ticket) => {
        // The owner hung up while the mint was in flight. Opening the socket
        // to close it again would still cost a connection at the edge and a
        // room wake-up, so it is never opened.
        if (closed) return;
        const ws = new WebSocket(relayUrl(relayOrigin, hostId), [
          RELAY_SUBPROTOCOL,
          `ticket.${ticket}`,
        ]);
        // arraybuffer, not blob: a blob costs an async read per message, and a
        // delta's whole point is to be applied the moment it lands.
        ws.binaryType = 'arraybuffer';
        ws.onopen = () => handlers.onOpen();
        ws.onmessage = (e: MessageEvent) => {
          if (e.data instanceof ArrayBuffer) handlers.onMessage(new Uint8Array(e.data));
        };
        ws.onclose = () => handlers.onClose();
        // `close` always follows `error`; routing both would double the redial.
        ws.onerror = () => {};
        socket = ws;
      })
      .catch(() => {
        // `.catch` chained, not `.then(f, g)`. A second argument only covers a
        // *rejected* mint; it does not cover the success handler throwing, and
        // that handler contains two constructors that do. `new URL` throws on a
        // malformed `relayOrigin`, and `new WebSocket` throws `SyntaxError` if
        // any subprotocol value is not an RFC 7230 token. The mint encodes
        // unpadded base64url precisely so a ticket is one; standard base64
        // (`=`, `/`) is not, and would land here.
        //
        // Uncaught, the rejection was silent and terminal: no `onOpen`, no
        // `onClose`, so `SessionClient` never schedules a redial and the tab
        // hangs for ever rather than reconnecting. That is the one outcome this
        // file's whole design is meant to make impossible.
        if (!closed) {
          closed = true;
          handlers.onClose();
        }
      });

    return {
      // Not a queue: `SessionClient` sends nothing before `onOpen()`, which is
      // what lets the shell exist before its socket does.
      send: (bytes: Uint8Array) => socket?.send(bytes),
      close: () => {
        if (closed) return;
        closed = true;
        // `onClose` is terminal per dial and must fire exactly once. With a
        // socket, its own `onclose` reports it; without one, nothing ever will,
        // because the pending mint is now discarded.
        if (socket !== null) socket.close();
        else handlers.onClose();
      },
    };
  };
}

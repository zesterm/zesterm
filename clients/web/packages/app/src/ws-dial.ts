/**
 * The browser's leg of the `Dial` seam: the daemon's binary WebSocket.
 *
 * The daemon guarantees whole zest-proto frames per WebSocket message, but
 * the client still runs its `FrameReader` — the seam is a byte stream on
 * every platform, and a transport that happens to preserve boundaries is a
 * detail, not a contract the client leans on.
 */

import type { ByteLinkHandlers, Dial } from '@zesterm/client';

export function wsDial(url: string): Dial {
  return (handlers: ByteLinkHandlers) => {
    const ws = new WebSocket(url);
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
    return {
      send: (bytes: Uint8Array) => ws.send(bytes),
      close: () => ws.close(),
    };
  };
}

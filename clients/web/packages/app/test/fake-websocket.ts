/**
 * A `WebSocket` the tests hold the other end of.
 *
 * The `Dial` implementations here reach for the global constructor, exactly as
 * a browser would, so the seam the tests take is the global itself rather than
 * an injected factory nothing in production would ever pass. `@zesterm/client`'s
 * own harness cannot serve: its `FakeLink` stands in for a `ByteLink`, which is
 * the layer *above* the thing under test here.
 */

/**
 * Armed by `throwOnConstruct`. The real `WebSocket` constructor throws
 * synchronously on a subprotocol that is not an RFC 7230 token, and a `Dial`
 * that does not survive that wedges the layer above it.
 *
 * Module-local: only `throwOnConstruct` and `restore` touch it.
 */
let thrown: Error | null = null;

export class FakeSocket {
  /** Every socket constructed since the last `installFakeWebSocket()`. */
  static created: FakeSocket[] = [];

  readonly url: string;
  readonly protocols: string[];
  binaryType = 'blob';
  readonly sent: unknown[] = [];
  closeCalls = 0;
  onopen: (() => void) | null = null;
  onmessage: ((e: { data: unknown }) => void) | null = null;
  onclose: (() => void) | null = null;
  onerror: (() => void) | null = null;

  constructor(url: string, protocols?: string | string[]) {
    if (thrown !== null) throw thrown;
    this.url = url;
    this.protocols = typeof protocols === 'string' ? [protocols] : (protocols ?? []);
    FakeSocket.created.push(this);
  }

  send(data: unknown): void {
    this.sent.push(data);
  }

  /** Like the real one, `close()` is what produces `onclose`. */
  close(): void {
    this.closeCalls += 1;
    this.onclose?.();
  }
}

export interface FakeWebSockets {
  /** Live: the array grows as sockets are constructed. */
  readonly created: FakeSocket[];
  /** The newest socket, or a failure naming what did not happen. */
  latest(): FakeSocket;
  /**
   * Arm the constructor to throw, as the real one does on a subprotocol that
   * is not an RFC 7230 token. Cleared by `restore`.
   */
  throwOnConstruct(error: Error): void;
  restore(): void;
}

export function installFakeWebSocket(): FakeWebSockets {
  const real = globalThis.WebSocket;
  FakeSocket.created = [];
  const created = FakeSocket.created;
  globalThis.WebSocket = FakeSocket as unknown as typeof WebSocket;
  return {
    created,
    latest(): FakeSocket {
      const socket = created.at(-1);
      if (!socket) throw new Error('no socket was constructed');
      return socket;
    },
    /** Make the next construction throw, as the real one does on a bad subprotocol. */
    throwOnConstruct(error: Error): void {
      thrown = error;
    },
    restore(): void {
      thrown = null;
      globalThis.WebSocket = real;
    },
  };
}

/**
 * Let the mint's promise and everything chained behind it settle.
 *
 * `setImmediate` rather than one microtask tick, for the reason the client
 * harness gives: a macrotask boundary drains the whole queue however deep it
 * got, and a real mint is a `fetch` with more than one `then` behind it.
 */
export function flush(): Promise<void> {
  return new Promise((resolve) => setImmediate(resolve));
}

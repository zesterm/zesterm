/**
 * A control-flavoured connection: the session *list*, not a session.
 *
 * What the sidecar runs against the daemon's loopback socket — handshake with
 * `watch_sessions: true`, then `Sessions` pushes for as long as the link
 * holds, with the same bounded-backoff redial as `SessionClient`. It can also
 * carry create/close requests; it never carries grid deltas, which keeps the
 * control plane's traffic shaped like control traffic.
 */

import type { ClientSigner } from '@zesterm/auth';
import {
  decode,
  encodeClientMessageBody,
  encodeFrame,
  FrameReader,
  isErrorMessage,
  isSessions,
  parseHostMessage,
  type ClientMessage,
  type SessionAddr,
  type SessionInfo,
} from '@zesterm/proto';

import { type Clock, systemClock, type TimerHandle } from './clock.ts';
import { HandshakeDriver, type HandshakeState } from './handshake.ts';
import type { ByteLink, Dial } from './link.ts';
import { REDIAL_MAX_MS, REDIAL_MIN_MS, type ConnectionState } from './session-client.ts';

export interface ConnectionEvents {
  onSessions?(sessions: readonly SessionInfo[], created: bigint | null): void;
  onConnection?(state: ConnectionState): void;
  onError?(message: string): void;
}

export interface ConnectionClientOptions {
  readonly dial: Dial;
  /** Seed-backed or WebCrypto-backed; this layer cannot tell and must not care. */
  readonly signer: ClientSigner;
  readonly label: string;
  readonly events?: ConnectionEvents;
  readonly clock?: Clock;
  readonly expectedHost?: string;
}

export class ConnectionClient {
  #options: ConnectionClientOptions;
  #clock: Clock;
  #events: ConnectionEvents;

  #link: ByteLink | null = null;
  #handshake: HandshakeDriver | null = null;
  #frames = new FrameReader();
  #connected = false;
  #closed = false;
  #redialAttempt = 0;
  #redialTimer: TimerHandle | null = null;

  /**
   * Handshake frames held behind an in-flight signature, and which dial they
   * belong to. Same two hazards `SessionClient` documents at length: a host
   * that pipelines two handshake messages must not have the second handled
   * while the first is still signing, and a `crypto.subtle` signature that
   * settles after its connection dropped must not be replayed onto the next
   * one, where it answers a challenge that was never asked.
   */
  #stalled: Uint8Array[] = [];
  #signing = false;
  #dialSeq = 0;

  constructor(options: ConnectionClientOptions) {
    this.#options = options;
    this.#clock = options.clock ?? systemClock;
    this.#events = options.events ?? {};
  }

  connect(): void {
    this.#dial();
  }

  close(): void {
    this.#closed = true;
    if (this.#redialTimer !== null) this.#clock.cancel(this.#redialTimer);
    this.#link?.close();
    this.#link = null;
  }

  get connected(): boolean {
    return this.#connected;
  }

  listSessions(): void {
    if (this.#connected) this.#send({ t: 'list_sessions' });
  }

  createSession(opts: { command: string; cwd: string; cols: number; rows: number }): void {
    if (this.#connected) this.#send({ t: 'create_session', ...opts });
  }

  closeSession(session: SessionAddr): void {
    if (this.#connected) this.#send({ t: 'close_session', session });
  }

  #dial(): void {
    if (this.#closed) return;
    this.#frames = new FrameReader();
    this.#dialSeq += 1;
    this.#handshake = new HandshakeDriver({
      signer: this.#options.signer,
      label: this.#options.label,
      // The entire point of this connection: pushes when anyone, anywhere,
      // changes the list.
      watchSessions: true,
      ...(this.#options.expectedHost === undefined
        ? {}
        : { expectedHost: this.#options.expectedHost }),
    });
    this.#events.onConnection?.(
      this.#redialAttempt === 0
        ? { phase: 'connecting' }
        : { phase: 'reconnecting', attempt: this.#redialAttempt },
    );
    this.#link = this.#options.dial({
      onOpen: () => {
        const h = this.#handshake;
        if (h) this.#send(h.hello());
      },
      onMessage: (bytes) => this.#onBytes(bytes),
      onClose: () => this.#onClose(),
    });
  }

  #onClose(): void {
    this.#connected = false;
    this.#link = null;
    this.#stalled.length = 0;
    this.#signing = false;
    if (this.#closed) return;
    const state = this.#handshake?.state;
    if (state?.phase === 'failed' && !state.retryable) {
      this.#events.onConnection?.({
        phase: 'failed',
        reason: state.reason,
        message: state.message,
      });
      return;
    }
    this.#redialAttempt += 1;
    const delay = Math.min(REDIAL_MIN_MS * 2 ** (this.#redialAttempt - 1), REDIAL_MAX_MS);
    this.#events.onConnection?.({ phase: 'reconnecting', attempt: this.#redialAttempt });
    this.#redialTimer = this.#clock.schedule(() => {
      this.#redialTimer = null;
      this.#dial();
    }, delay);
  }

  #onBytes(bytes: Uint8Array): void {
    this.#frames.feed(bytes);
    for (;;) {
      let body: Uint8Array | undefined;
      try {
        body = this.#frames.next();
      } catch (e) {
        this.#events.onError?.(`framing lost: ${String(e)}`);
        this.#link?.close();
        return;
      }
      if (body === undefined) return;
      if (this.#signing) this.#stalled.push(body);
      else this.#onMessage(body);
    }
  }

  /** Resume the frames a pending signature held up, one handshake at a time. */
  #drainStalled(): void {
    while (!this.#signing && this.#stalled.length > 0) {
      const body = this.#stalled.shift();
      if (body === undefined) return;
      this.#onMessage(body);
    }
  }

  #onMessage(sealed: Uint8Array): void {
    // Opened here rather than in `#onBytes`, and the difference is load-bearing:
    // a frame arriving while a signature is pending is stalled, and at *that*
    // moment the channel does not exist yet -- it comes into being when the
    // challenge is answered. `#stalled` is a FIFO drained in order, so opening
    // at processing time still counts records in arrival order, which is what
    // the nonce schedule requires.
    //
    // A frame that will not open ends the connection rather than being skipped:
    // the counter has already advanced, so there is no position to resume from,
    // and reading on would decrypt every later frame under the wrong nonce.
    let body = sealed;
    const channel = this.#handshake?.channel;
    if (channel) {
      try {
        body = channel.open(sealed);
      } catch (e) {
        this.#events.onError?.(`a sealed frame did not open: ${String(e)}`);
        this.#link?.close();
        return;
      }
    }
    const msg = parseHostMessage(decode(body));
    const handshake = this.#handshake;

    // Guarded on `#connected`, not on the handshake's own state: TypeScript
    // narrows repeated property reads, and `onMessage` moving the state is
    // exactly what the narrowing cannot see.
    if (!this.#connected && handshake) {
      const seq = this.#dialSeq;
      this.#signing = true;
      void handshake.onMessage(msg).then((replies) => {
        // Three ways this continuation can be stale, and only one of them used
        // to be checked. A redial bumps the seq; `close()` does not, and nor
        // does a link that has already been torn down -- so a `welcome` in
        // flight when the caller closed would still emit a connection event
        // and write into a null link. Structurally impossible while the
        // handshake was synchronous, which is why the guard was written for
        // the redial case alone.
        //
        // `#signing` is cleared on every path out, or the stall queue never
        // drains again and every later frame waits forever, silently.
        //
        // Deliberately untested, and worth saying why rather than shipping a
        // green test that proves nothing: the close case has no observable
        // effect today. `#send` is already `#link?.send(...)`, so a write after
        // close is a silent no-op, and no connection event is emitted on this
        // path. What is left is `#signing` and `#stalled` on an object nobody
        // holds. This is defence-in-depth against a future path that re-dials
        // without closing — at which point a stuck `#signing` stalls every
        // frame forever, and the failure is silent.
        if (seq !== this.#dialSeq || this.#closed || this.#link === null) {
          this.#signing = false;
          this.#stalled.length = 0;
          return;
        }
        this.#signing = false;
        for (const reply of replies) this.#send(reply);
        const state: HandshakeState = handshake.state;
        if (state.phase === 'welcomed') {
          this.#connected = true;
          this.#redialAttempt = 0;
          this.#events.onConnection?.({ phase: 'connected' });
          // Prime the list; pushes keep it fresh from here.
          this.#send({ t: 'list_sessions' });
        } else if (state.phase === 'awaiting-approval') {
          this.#events.onConnection?.({ phase: 'awaiting-approval', code: state.code });
        } else if (state.phase === 'failed') {
          this.#link?.close();
        }
        this.#drainStalled();
      });
      return;
    }

    if (isSessions(msg)) {
      this.#events.onSessions?.(msg.sessions, msg.created);
      return;
    }
    if (isErrorMessage(msg)) {
      this.#events.onError?.(msg.message);
    }
  }

  #send(msg: ClientMessage): void {
    const body = encodeClientMessageBody(msg);
    // The positional switch: the channel exists from the moment the challenge
    // is answered, and the `auth` that answers it is itself the first sealed
    // frame. `hello` goes out before there is a channel at all, so it is
    // plaintext without needing to be named as an exception.
    const channel = this.#handshake?.channel;
    this.#link?.send(encodeFrame(channel ? channel.seal(body) : body));
  }
}

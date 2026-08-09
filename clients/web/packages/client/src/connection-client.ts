/**
 * A control-flavoured connection: the session *list*, not a session.
 *
 * What the sidecar runs against the daemon's loopback socket — handshake with
 * `watch_sessions: true`, then `Sessions` pushes for as long as the link
 * holds, with the same bounded-backoff redial as `SessionClient`. It can also
 * carry create/close requests; it never carries grid deltas, which keeps the
 * control plane's traffic shaped like control traffic.
 */

import type { ClientIdentity } from '@zesterm/auth';
import {
  decode,
  encodeClientMessage,
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
  readonly identity: ClientIdentity;
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
    this.#handshake = new HandshakeDriver({
      identity: this.#options.identity,
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
      this.#onMessage(body);
    }
  }

  #onMessage(body: Uint8Array): void {
    const msg = parseHostMessage(decode(body));
    const handshake = this.#handshake;

    // Guarded on `#connected`, not on the handshake's own state: TypeScript
    // narrows repeated property reads, and `onMessage` moving the state is
    // exactly what the narrowing cannot see.
    if (!this.#connected && handshake) {
      for (const reply of handshake.onMessage(msg)) this.#send(reply);
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
    this.#link?.send(encodeClientMessage(msg));
  }
}

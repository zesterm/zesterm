/**
 * One session, kept alive across a lossy transport.
 *
 * A behavioural port of `crates/zest-app/src/remote.rs` (`RemoteSession`),
 * which is the client that has already paid for its lessons. The four that
 * matter, mirrored deliberately:
 *
 * - **Acks coalesce at 16 ms.** One ack per delta would double the message
 *   count on a busy session for information the host needs approximately.
 * - **An update whose `base` is not what this client holds is discarded** and
 *   a keyframe requested. Applying it anyway would render a grid the host
 *   never showed.
 * - **Reconnect adopts, never recreates**: redial with bounded backoff
 *   (200 ms → 5 s), fresh handshake — the host challenges every connection —
 *   then re-`attach` to *the same* session. The `GridView` on screen is kept;
 *   the keyframe the host sends on attach resyncs it in place.
 * - **Input typed while disconnected is dropped, and only the newest resize
 *   is kept.** Replaying thirty seconds of queued keystrokes is how a
 *   reconnect runs a command the user abandoned.
 */

import {
  encodeClientMessage,
  FrameReader,
  GridView,
  decode,
  parseHostMessage,
  isKeyframe,
  isUpdate,
  isExited,
  isErrorMessage,
  isScrollback,
  type ClientMessage,
  type SessionAddr,
} from '@zesterm/proto';
import type { ClientIdentity } from '@zesterm/auth';

import { type Clock, systemClock, type TimerHandle } from './clock.ts';
import type { ByteLink, Dial } from './link.ts';
import { HandshakeDriver, type HandshakeState } from './handshake.ts';

/** remote.rs's `ACK_INTERVAL`. */
export const ACK_INTERVAL_MS = 16;
/** remote.rs's `REDIAL_MIN` / `REDIAL_MAX`. */
export const REDIAL_MIN_MS = 200;
export const REDIAL_MAX_MS = 5000;

export type ConnectionState =
  | { readonly phase: 'connecting' }
  | { readonly phase: 'awaiting-approval'; readonly code: string }
  | { readonly phase: 'connected' }
  | { readonly phase: 'reconnecting'; readonly attempt: number }
  | { readonly phase: 'failed'; readonly reason: string; readonly message: string };

/** Which rows changed, for a renderer that repaints by row. */
export type DirtyRows = ReadonlySet<number> | 'all';

export interface SessionEvents {
  onChange?(dirty: DirtyRows): void;
  onBlocksChanged?(): void;
  onTitle?(title: string): void;
  onModes?(modes: number): void;
  onConnection?(state: ConnectionState): void;
  onExited?(code: number | null): void;
  onError?(message: string): void;
}

export interface SessionClientOptions {
  readonly dial: Dial;
  readonly identity: ClientIdentity;
  readonly label: string;
  readonly session: SessionAddr;
  readonly cols: number;
  readonly rows: number;
  readonly events?: SessionEvents;
  readonly clock?: Clock;
  readonly expectedHost?: string;
}

export class SessionClient {
  readonly grid = new GridView();

  #options: SessionClientOptions;
  #clock: Clock;
  #events: SessionEvents;

  #link: ByteLink | null = null;
  #handshake: HandshakeDriver | null = null;
  #frames = new FrameReader();
  #connected = false;
  #closed = false;

  /** The last sequence applied — the truth `Update.base` is checked against. */
  #appliedSeq = -1n;
  /** Set after a bad base until the healing keyframe lands, so a burst of
   * stale updates asks once instead of once per update. */
  #awaitingKeyframe = false;

  #pendingAck: bigint | null = null;
  #lastAckAt = 0;
  #ackTimer: TimerHandle | null = null;

  #cols: number;
  #rows: number;
  /** The newest size seen while disconnected; replayed via re-attach. */
  #resizeDirty = false;

  #redialAttempt = 0;
  #redialTimer: TimerHandle | null = null;

  constructor(options: SessionClientOptions) {
    this.#options = options;
    this.#clock = options.clock ?? systemClock;
    this.#events = options.events ?? {};
    this.#cols = options.cols;
    this.#rows = options.rows;
  }

  /** Open the first connection. One code path with reconnect, deliberately. */
  connect(): void {
    this.#dial();
  }

  close(): void {
    this.#closed = true;
    if (this.#ackTimer !== null) this.#clock.cancel(this.#ackTimer);
    if (this.#redialTimer !== null) this.#clock.cancel(this.#redialTimer);
    this.#link?.close();
    this.#link = null;
  }

  get connected(): boolean {
    return this.#connected;
  }

  /** Encoded terminal bytes. Dropped while disconnected — see the module doc. */
  input(bytes: Uint8Array): void {
    if (!this.#connected || bytes.length === 0) return;
    this.#send({ t: 'input', session: this.#options.session, bytes });
  }

  resize(cols: number, rows: number): void {
    if (cols === this.#cols && rows === this.#rows) return;
    this.#cols = cols;
    this.#rows = rows;
    if (this.#connected) {
      this.#send({ t: 'resize', session: this.#options.session, cols, rows });
    } else {
      // Only the newest, and only sizes: re-attach carries current dims, so
      // "replay" is simply attaching at the size the client is now.
      this.#resizeDirty = true;
    }
  }

  requestScrollback(fromLine: bigint, count: number): void {
    if (!this.#connected) return;
    this.#send({
      t: 'request_scrollback',
      session: this.#options.session,
      from_line: fromLine,
      count,
    });
  }

  // -------------------------------------------------------------------------

  #dial(): void {
    if (this.#closed) return;
    this.#frames = new FrameReader();
    this.#handshake = new HandshakeDriver({
      identity: this.#options.identity,
      label: this.#options.label,
      watchSessions: false,
      ...(this.#options.expectedHost === undefined
        ? {}
        : { expectedHost: this.#options.expectedHost }),
    });
    this.#emitConnection(
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
      // `Denied` must never be retried: hammering a host that said no is how
      // the rate limiter ends up punishing the user for the client's manners.
      this.#emitConnection({ phase: 'failed', reason: state.reason, message: state.message });
      return;
    }

    this.#redialAttempt += 1;
    const delay = Math.min(REDIAL_MIN_MS * 2 ** (this.#redialAttempt - 1), REDIAL_MAX_MS);
    this.#emitConnection({ phase: 'reconnecting', attempt: this.#redialAttempt });
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
        // Framing is broken; the stream position cannot be trusted. Closing
        // and redialling is the only honest response — same as the daemon's.
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
      switch (state.phase) {
        case 'awaiting-approval':
          this.#emitConnection({ phase: 'awaiting-approval', code: state.code });
          return;
        case 'welcomed':
          this.#afterWelcome();
          return;
        case 'failed':
          // The link will close (either end); #onClose decides on retry.
          this.#link?.close();
          return;
        default:
          return;
      }
    }

    if (isKeyframe(msg)) {
      this.grid.applyKeyframe(msg);
      this.#appliedSeq = msg.seq;
      this.#awaitingKeyframe = false;
      this.#queueAck(msg.seq);
      this.#events.onTitle?.(this.grid.title);
      this.#events.onModes?.(this.grid.modes);
      this.#events.onBlocksChanged?.();
      this.#events.onChange?.('all');
      return;
    }
    if (isUpdate(msg)) {
      if (msg.base !== this.#appliedSeq) {
        // Never applied out of order. One request per gap, not per update: a
        // burst of stale deltas after a hiccup should heal once.
        if (!this.#awaitingKeyframe) {
          this.#awaitingKeyframe = true;
          this.#send({ t: 'request_keyframe', session: this.#options.session });
        }
        return;
      }
      if (this.#awaitingKeyframe) return;

      const before = this.grid.title;
      const modesBefore = this.grid.modes;
      const dirty = dirtyRowsOf(msg.delta, this.grid.rows.length);
      this.grid.applyDelta(msg.delta);
      this.#appliedSeq = msg.seq;
      this.#queueAck(msg.seq);
      if (this.grid.title !== before) this.#events.onTitle?.(this.grid.title);
      if (this.grid.modes !== modesBefore) this.#events.onModes?.(this.grid.modes);
      if (msg.delta.blocks.length > 0) this.#events.onBlocksChanged?.();
      this.#events.onChange?.(dirty);
      return;
    }
    if (isScrollback(msg)) {
      // Prepend history: the host answers `request_scrollback` oldest-first.
      for (const a of msg.attrs) this.grid.attrs.set(a.id, a);
      this.grid.scrollback.unshift(...msg.rows_data);
      this.#events.onChange?.('all');
      return;
    }
    if (isExited(msg)) {
      this.#events.onExited?.(msg.code);
      return;
    }
    if (isErrorMessage(msg)) {
      this.#events.onError?.(msg.message);
      return;
    }
    // Anything else — session listings, pairing pushes — is not this
    // client's business; it watches one session.
  }

  #afterWelcome(): void {
    this.#connected = true;
    this.#redialAttempt = 0;
    this.#resizeDirty = false;
    // Attach at the size the client is *now* — which is also what replays
    // the newest disconnected-time resize, by construction.
    this.#send({
      t: 'attach',
      session: this.#options.session,
      cols: this.#cols,
      rows: this.#rows,
    });
    this.#emitConnection({ phase: 'connected' });
  }

  /**
   * Latch the highest applied sequence and flush on the 16 ms cadence.
   * A trailing timer covers a stream that goes quiet right after a delta —
   * the host should not wait a whole next-delta for its ack.
   */
  #queueAck(seq: bigint): void {
    this.#pendingAck = seq;
    const now = this.#clock.now();
    if (now - this.#lastAckAt >= ACK_INTERVAL_MS) {
      this.#flushAck();
      return;
    }
    if (this.#ackTimer === null) {
      this.#ackTimer = this.#clock.schedule(
        () => {
          this.#ackTimer = null;
          this.#flushAck();
        },
        ACK_INTERVAL_MS - (now - this.#lastAckAt),
      );
    }
  }

  #flushAck(): void {
    if (this.#pendingAck === null || !this.#connected) return;
    this.#send({ t: 'ack', session: this.#options.session, seq: this.#pendingAck });
    this.#pendingAck = null;
    this.#lastAckAt = this.#clock.now();
  }

  #send(msg: ClientMessage): void {
    this.#link?.send(encodeClientMessage(msg));
  }

  #emitConnection(state: ConnectionState): void {
    this.#events.onConnection?.(state);
  }
}

/**
 * Which viewport rows a delta touches, for row-scoped repaints.
 *
 * Conservative on purpose: a scroll dirties its whole region and a keyframe
 * dirties everything. Precision here buys nothing — the painter clips per
 * row anyway — and an optimistic answer paints a stale line.
 */
export function dirtyRowsOf(
  delta: {
    readonly ops: readonly { readonly op: string; readonly [k: string]: unknown }[];
  },
  rowCount: number,
): DirtyRows {
  const dirty = new Set<number>();
  for (const op of delta.ops) {
    switch (op.op) {
      case 'row':
        dirty.add(op['row'] as number);
        break;
      case 'scroll':
      case 'erase': {
        const top = op['top'] as number;
        const bottom = Math.min(op['bottom'] as number, rowCount - 1);
        for (let r = top; r <= bottom; r++) dirty.add(r);
        break;
      }
      case 'cursor':
        // Old and new cursor rows are unknown here; the caller repaints the
        // cursor layer every frame, so nothing to add.
        break;
      case 'alt_screen':
        return 'all';
      default:
        break;
    }
  }
  return dirty;
}

/**
 * A scripted daemon on the other end of the `Dial` seam.
 *
 * Speaks real bytes — host messages are built as ordered maps, encoded with
 * the proto package's own msgpack encoder and length prefix — so the client
 * under test runs its full decode path, not a shortcut. The host side of the
 * handshake signs with the same primitives the Rust host uses.
 */

import * as ed from '@noble/ed25519';
import { sha512 } from '@noble/hashes/sha2.js';
import { generateIdentity, preimage, authTranscript, type ClientIdentity } from '@zesterm/auth';
import {
  decode,
  encode,
  encodeFrame,
  FrameReader,
  bytesToHex,
  hexToBytes,
} from '@zesterm/proto';
import type { ByteLink, ByteLinkHandlers, Clock, TimerHandle } from '../src/index.ts';

ed.hashes.sha512 = sha512;

/** Deterministic time the tests advance by hand. */
export class FakeClock implements Clock {
  #now = 1_000_000;
  #timers = new Map<number, { at: number; fn: () => void }>();
  #nextId = 1;

  now(): number {
    return this.#now;
  }

  schedule(fn: () => void, ms: number): TimerHandle {
    const id = this.#nextId++;
    this.#timers.set(id, { at: this.#now + ms, fn });
    return id;
  }

  cancel(handle: TimerHandle): void {
    this.#timers.delete(handle as number);
  }

  /** Advance, firing due timers in time order — one quantum at a time. */
  advance(ms: number): void {
    const target = this.#now + ms;
    for (;;) {
      const due = [...this.#timers.entries()]
        .filter(([, t]) => t.at <= target)
        .sort((a, b) => a[1].at - b[1].at)[0];
      if (!due) break;
      const [id, timer] = due;
      this.#timers.delete(id);
      this.#now = timer.at;
      timer.fn();
    }
    this.#now = target;
  }

  get pendingTimers(): number {
    return this.#timers.size;
  }

  /** The delay until the next timer fires, for backoff assertions. */
  nextTimerIn(): number | undefined {
    const ts = [...this.#timers.values()].map((t) => t.at - this.#now);
    return ts.length === 0 ? undefined : Math.min(...ts);
  }
}

/** One scripted connection. `FakeDaemon.dial` hands these out. */
export class FakeLink implements ByteLink {
  readonly sent: Array<Record<string, unknown>> = [];
  closed = false;
  #handlers: ByteLinkHandlers;
  #frames = new FrameReader();

  constructor(handlers: ByteLinkHandlers) {
    this.#handlers = handlers;
  }

  open(): void {
    this.#handlers.onOpen();
  }

  send(bytes: Uint8Array): void {
    this.#frames.feed(bytes);
    for (;;) {
      const body = this.#frames.next();
      if (body === undefined) return;
      this.sent.push(decode(body) as Record<string, unknown>);
    }
  }

  close(): void {
    if (this.closed) return;
    this.closed = true;
    this.#handlers.onClose();
  }

  /** Deliver one host message, framed and encoded like the daemon would. */
  deliver(wireShape: Record<string, unknown>): void {
    this.#handlers.onMessage(encodeFrame(encode(wireShape)));
  }

  /** Messages of one tag, for targeted assertions. */
  ofType(t: string): Array<Record<string, unknown>> {
    return this.sent.filter((m) => m['t'] === t);
  }

  lastOfType(t: string): Record<string, unknown> | undefined {
    return this.ofType(t).at(-1);
  }
}

export const HOST_SEED = '7a'.repeat(32);

/**
 * The daemon side: hands out links, answers hellos with a real signed
 * challenge, welcomes a signed auth.
 */
export class FakeDaemon {
  readonly links: FakeLink[] = [];
  readonly host = generateIdentity(HOST_SEED);
  readonly hostNonce = '9b'.repeat(32);

  get dial() {
    return (handlers: ByteLinkHandlers): ByteLink => {
      const link = new FakeLink(handlers);
      this.links.push(link);
      return link;
    };
  }

  get current(): FakeLink {
    const link = this.links.at(-1);
    if (!link) throw new Error('nothing has dialled');
    return link;
  }

  /** Run the host's half of the handshake over the newest link. */
  completeHandshake(): void {
    const link = this.current;
    link.open();
    const hello = link.lastOfType('hello');
    if (!hello) throw new Error('the client did not say hello');
    const transcript = {
      version: hello['version'] as number,
      host: this.host.clientId,
      client: hello['client'] as string,
      hostNonce: this.hostNonce,
      clientNonce: hello['nonce'] as string,
      hostLabel: 'fake-daemon',
      clientLabel: hello['label'] as string,
    };
    const bytes = authTranscript(transcript);
    const signature = bytesToHex(ed.sign(preimage('host', 'auth', bytes), hexToBytes(HOST_SEED)));
    link.deliver({
      t: 'challenge',
      version: 2,
      host: this.host.clientId,
      label: 'fake-daemon',
      nonce: this.hostNonce,
      signature,
    });
    const auth = link.lastOfType('auth');
    if (!auth) throw new Error('the client did not answer the challenge');
    // The FakeDaemon does not re-verify the client: what these tests hold to
    // account is the client's behaviour, and the signature's correctness is
    // the auth package's golden-pinned business.
    link.deliver({ t: 'welcome', version: 2, host: this.host.clientId, label: 'fake-daemon' });
  }
}

export const CLIENT_SEED = '5e'.repeat(32);

export function testIdentity(): ClientIdentity {
  return generateIdentity(CLIENT_SEED);
}

export const ADDR = { host: '2e'.repeat(32), session: 1n };

/** A minimal keyframe wire shape. */
export function keyframe(seq: number, rows: string[] = ['hello']): Record<string, unknown> {
  return {
    t: 'keyframe',
    session: { host: ADDR.host, session: 1 },
    seq,
    cols: 20,
    rows: rows.length,
    rows_data: rows.map((text, i) => ({
      line: i,
      runs: [{ attr: 0, cells: text.length, text }],
      wrapped: false,
    })),
    attrs: [{ id: 0, fg: 'Default', bg: 'Default', flags: 0 }],
    cursor: { row: 0, col: 0, visible: true, shape: 0 },
    modes: 0,
  };
}

/** An update writing `text` into row 0. */
export function update(base: number, seq: number, text: string): Record<string, unknown> {
  return {
    t: 'update',
    session: { host: ADDR.host, session: 1 },
    base,
    seq,
    delta: {
      attrs: [],
      ops: [
        {
          op: 'row',
          row: 0,
          payload: { line: 0, runs: [{ attr: 0, cells: text.length, text }], wrapped: false },
        },
      ],
    },
  };
}

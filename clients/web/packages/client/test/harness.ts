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
import {
  generateIdentity,
  preimage,
  authTranscript,
  seedSigner,
  type ClientIdentity,
  type ClientSigner,
} from '@zesterm/auth';
import { ephemeralFromSeed, type SecureChannel } from '@zesterm/auth/secure';
import {
  decode,
  encode,
  encodeFrame,
  FrameReader,
  bytesToHex,
  hexToBytes,
} from '@zesterm/proto';
import { PROTOCOL_VERSION } from '../src/index.ts';
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
  /**
   * The host's half of this connection's channel, once there is one.
   *
   * The fake daemon plays the seal switch exactly as the real one does, and
   * that is the point: a harness that stayed in plaintext would let the client
   * seal nothing, seal the wrong things, or seal in the wrong order, and every
   * test here would still pass.
   */
  channel: SecureChannel | null = null;
  /** Outgoing seals only once the challenge itself has gone out. */
  #sealOut = false;

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
      // Incoming is sealed from the client's `auth` onwards, which is to say
      // from the moment this fake issued a challenge.
      const plain = this.channel ? this.channel.open(body) : body;
      this.sent.push(decode(plain) as Record<string, unknown>);
    }
  }

  close(): void {
    if (this.closed) return;
    this.closed = true;
    this.#handlers.onClose();
  }

  /** Deliver one host message, framed, sealed and encoded like the daemon would. */
  deliver(wireShape: Record<string, unknown>): void {
    const body = encode(wireShape);
    const out = this.#sealOut && this.channel ? this.channel.seal(body) : body;
    this.#handlers.onMessage(encodeFrame(out));
    // The challenge carries the host's DH key, so it is the last plaintext
    // frame out. Flipped *after* sending, or it would be sealed under a key
    // the client cannot have yet.
    if (wireShape['t'] === 'challenge') this.#sealOut = true;
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
/** The fake host's ephemeral, fixed so a failing test reproduces. */
export const HOST_DH_SEED = '3c'.repeat(32);

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

  /**
   * Run the host's half of the handshake over the newest link.
   *
   * Async because the client's is: a device key that cannot be read out signs
   * through `crypto.subtle`, so the answer to a challenge arrives a
   * microtask later even when the seed path resolves immediately. Every
   * settling point below is a place a real daemon would also have waited.
   */
  async completeHandshake(): Promise<void> {
    const link = this.current;
    link.open();
    await flush();
    link.deliver(this.challengeFor(link));
    await flush();
    const auth = link.lastOfType('auth');
    if (!auth) throw new Error('the client did not answer the challenge');
    // The FakeDaemon does not re-verify the client: what these tests hold to
    // account is the client's behaviour, and the signature's correctness is
    // the auth package's golden-pinned business.
    link.deliver(this.welcome);
    await flush();
  }

  /**
   * The signed challenge this daemon answers a hello with.
   *
   * Separate from `completeHandshake` so a test can deliver it and the
   * welcome in the *same task*, which is what a host pipelining two handshake
   * messages into one segment looks like from here.
   */
  challengeFor(link: FakeLink): Record<string, unknown> {
    const hello = link.lastOfType('hello');
    if (!hello) throw new Error('the client did not say hello');
    // A fixed seed, so a failing test is reproducible. Real hosts generate.
    const ephemeral = ephemeralFromSeed(hexToBytes(HOST_DH_SEED));
    const transcript = {
      version: hello['version'] as number,
      host: this.host.clientId,
      client: hello['client'] as string,
      hostNonce: this.hostNonce,
      clientNonce: hello['nonce'] as string,
      hostLabel: 'fake-daemon',
      clientLabel: hello['label'] as string,
      hostDh: ephemeral.publicKey,
      clientDh: hello['dh'] as string,
    };
    const bytes = authTranscript(transcript);
    // Armed now, not on delivery: the client's `auth` is sealed and arrives
    // before anything else is sent, so the receiving half has to be ready the
    // moment the challenge is built.
    link.channel = ephemeral.agree(hello['dh'] as string, transcript, 'host');
    return {
      t: 'challenge',
      version: PROTOCOL_VERSION,
      host: this.host.clientId,
      label: 'fake-daemon',
      nonce: this.hostNonce,
      dh: ephemeral.publicKey,
      signature: bytesToHex(ed.sign(preimage('host', 'auth', bytes), hexToBytes(HOST_SEED))),
    };
  }

  get welcome(): Record<string, unknown> {
    return { t: 'welcome', version: PROTOCOL_VERSION, host: this.host.clientId, label: 'fake-daemon' };
  }
}

/**
 * Let every pending microtask settle.
 *
 * `setImmediate` rather than `Promise.resolve()`: the handshake awaits
 * through several promises, and one microtask tick would drain only the
 * first. A macrotask boundary drains the whole queue however deep it got.
 */
export function flush(): Promise<void> {
  return new Promise((resolve) => setImmediate(resolve));
}

export const CLIENT_SEED = '5e'.repeat(32);

export function testIdentity(): ClientIdentity {
  return generateIdentity(CLIENT_SEED);
}

export function testSigner(): ClientSigner {
  return seedSigner(testIdentity());
}

/**
 * A signer that will not answer until the test says so.
 *
 * Stands in for `crypto.subtle`, whose promise settles on a later task rather
 * than a microtask — long enough for a second host message, or a dropped
 * connection, to land first. The seed path is too fast to expose either.
 */
export function gatedSigner(): ClientSigner & {
  release(): void;
  fail(reason: Error): void;
  readonly asked: number;
} {
  const inner = testSigner();
  const waiting: Array<{ resolve: (sig: string) => void; reject: (e: Error) => void; work: Promise<string> }> = [];
  let asked = 0;
  return {
    clientId: inner.clientId,
    sign(purpose, message) {
      asked += 1;
      return new Promise<string>((resolve, reject) => {
        waiting.push({ resolve, reject, work: inner.sign(purpose, message) });
      });
    },
    release(): void {
      for (const w of waiting.splice(0)) void w.work.then(w.resolve, w.reject);
    },
    fail(reason: Error): void {
      for (const w of waiting.splice(0)) w.reject(reason);
    },
    get asked(): number {
      return asked;
    },
  };
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

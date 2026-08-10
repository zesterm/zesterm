/**
 * An in-memory Durable Object platform: enough of `RoomState` and `Sock` to
 * drive the room under `node --test`, with no workerd and no miniflare.
 *
 * Two things make it worth more than a bag of stubs.
 *
 * **It enforces the platform's limits.** Ten tags per socket, 256 characters
 * each, 16,384 bytes of serialized attachment — all three throw here exactly
 * where they throw at the edge. A fake that is more permissive than the
 * platform is a fake that reports success on code which cannot deploy, and the
 * caps are the kind of thing nobody looks up until an attachment grows one
 * field too many at 3am.
 *
 * **It models eviction.** `state()` hands out a *fresh* `RoomState` over the
 * same durable data, which is what the runtime leaves behind between messages.
 * Constructing a new room from it before every handler call is how the
 * hibernation bug ADR-009 names — a `Map<WebSocket, WebSocket>` in an instance
 * field — is caught, because that bug passes every test written against a
 * single long-lived instance.
 *
 * The numbers were measured against real workerd rather than read off a doc
 * page; `src/room/limits.ts` records what and how.
 */

import { serialize } from 'node:v8';

import {
  MAX_ATTACHMENT_BYTES,
  MAX_TAGS_PER_SOCKET,
  MAX_TAG_LENGTH,
} from '../src/room/limits.ts';
import type { RoomState, RoomStorage, Sock } from '../src/room/state.ts';

export class FakeSock implements Sock {
  /** For test messages; the platform has no such notion. */
  readonly label: string;
  readonly sent: Array<string | ArrayBuffer> = [];
  closed: { code: number | undefined; reason: string | undefined } | null = null;
  /**
   * How many times an attachment was written. Attachments are the room's only
   * per-pipe memory, so a count that grows with traffic means the data path is
   * doing bookkeeping it should not be.
   */
  attachmentWrites = 0;

  #attachment: unknown = null;

  constructor(label = 'sock') {
    this.label = label;
  }

  send(message: string | ArrayBuffer): void {
    if (this.closed !== null) {
      throw new Error(`${this.label}: send after close`);
    }
    this.sent.push(message);
  }

  close(code?: number, reason?: string): void {
    this.closed = { code, reason };
  }

  serializeAttachment(value: unknown): void {
    // Sized by serializing, because the platform bounds the *serialized* form:
    // a 16,384-character string is 16,390 bytes and is refused. `node:v8`'s
    // serializer is the same structured-clone serializer V8 gives workerd, and
    // it was checked to agree byte for byte at this exact boundary — a string
    // and an object either side of 16,384 accepted and rejected identically in
    // both runtimes.
    const bytes = serialize(value).byteLength;
    if (bytes > MAX_ATTACHMENT_BYTES) {
      // workerd's wording, verbatim including the missing space, so a grep for
      // the production error lands here.
      throw new Error(
        `A WebSocket 'attachment' cannot be larger than ${MAX_ATTACHMENT_BYTES} bytes.` +
          `'attachment' was ${bytes} bytes.`,
      );
    }
    // A snapshot, not a reference. The platform stores serialized bytes, so
    // mutating the value afterwards changes nothing; a fake that kept the
    // reference would let an implementation which relies on that mutation pass
    // here and lose it in production.
    this.#attachment = structuredClone(value);
    this.attachmentWrites += 1;
  }

  deserializeAttachment(): unknown {
    // Cloned on the way out for the same reason: the platform deserializes
    // afresh each call, so two reads are never the same object and mutating
    // one is not visible to the next.
    return structuredClone(this.#attachment);
  }
}

export class FakeRoomStorage implements RoomStorage {
  /**
   * `put` and `delete`. ADR-009's cost model rests on never writing storage on
   * the data path — a write there costs a request per message and keeps the
   * object awake, which turns the dominant cost term from zero into
   * continuous. Counting is what makes that assertable instead of aspirational.
   */
  writes = 0;
  reads = 0;

  readonly #kv = new Map<string, unknown>();

  async get<T>(key: string): Promise<T | undefined> {
    this.reads += 1;
    if (!this.#kv.has(key)) return undefined;
    return structuredClone(this.#kv.get(key)) as T;
  }

  async put(key: string, value: unknown): Promise<void> {
    this.writes += 1;
    this.#kv.set(key, structuredClone(value));
  }

  async delete(key: string): Promise<boolean> {
    this.writes += 1;
    return this.#kv.delete(key);
  }
}

/**
 * Everything that outlives an object instance: the storage, the accepted
 * sockets and their tags.
 *
 * The split is the whole point. A room is disposable; this is not.
 */
export class FakePlatform {
  readonly storage = new FakeRoomStorage();

  /** How many `RoomState`s have been handed out — one per simulated eviction. */
  states = 0;

  // Insertion-ordered, because `getWebSockets` returns accept order and a room
  // that happens to work only when the pairing arrives in one order should
  // fail somewhere a test can see it.
  readonly #tags = new Map<Sock, string[]>();

  /**
   * A fresh `RoomState` over the same durable data — what eviction leaves
   * behind.
   *
   * Call it once for a suite that models a busy object, and once per handler
   * call for a suite that models an idle one. The second is the one that
   * catches state held in instance fields.
   */
  state(): RoomState {
    this.states += 1;
    return {
      storage: this.storage,
      acceptWebSocket: (ws, tags) => this.#accept(ws, tags),
      getWebSockets: (tag) => this.#get(tag),
      getTags: (ws) => this.#getTags(ws),
    };
  }

  /** Every accepted socket, whatever its tags. */
  get sockets(): FakeSock[] {
    return [...this.#tags.keys()] as FakeSock[];
  }

  /** Attachment writes across every socket, for the "no bookkeeping" assertion. */
  get attachmentWrites(): number {
    return this.sockets.reduce((n, s) => n + s.attachmentWrites, 0);
  }

  #accept(ws: Sock, tags: string[] = []): void {
    // Distinct, not total: workerd counts distinct tags, and a room that passes
    // the same tag twice is within the cap rather than one over it.
    const distinct = [...new Set(tags)];
    if (distinct.length > MAX_TAGS_PER_SOCKET) {
      throw new Error(
        `a Hibernatable WebSocket cannot have more than ${MAX_TAGS_PER_SOCKET} tags`,
      );
    }
    for (const tag of distinct) {
      if (tag.length > MAX_TAG_LENGTH) {
        throw new Error(
          `"${tag}" is longer than the max tag length (${MAX_TAG_LENGTH} characters).`,
        );
      }
    }
    this.#tags.set(ws, distinct);
  }

  #get(tag?: string): Sock[] {
    const all = [...this.#tags.entries()];
    if (tag === undefined) return all.map(([ws]) => ws);
    return all.filter(([, tags]) => tags.includes(tag)).map(([ws]) => ws);
  }

  #getTags(ws: Sock): string[] {
    const tags = this.#tags.get(ws);
    if (tags === undefined) {
      throw new Error(
        "you must call 'acceptWebSocket()' before attempting to access the tags of a WebSocket.",
      );
    }
    return [...tags];
  }
}

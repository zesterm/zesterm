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
 *
 * `FakeDb` is here for the same reason and holds the line the same way: it
 * answers the two statements the control link issues and throws on anything
 * else, so a query that appears on the data path fails a test rather than
 * merely costing money.
 */

import { deserialize, serialize } from 'node:v8';

import type { D1Binding, D1PreparedStatement } from '../src/env.ts';
import {
  MAX_ATTACHMENT_BYTES,
  MAX_TAGS_PER_SOCKET,
  MAX_TAG_LENGTH,
} from '../src/room/limits.ts';
import type { AutoResponsePair, RoomState, RoomStorage, Sock } from '../src/room/state.ts';

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

  /** The serialized bytes, as the platform holds them -- never the live value. */
  #attachment: Buffer | null = null;

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
    // Stored as the very bytes that were just measured, rather than a
    // `structuredClone` of the same value. The two agree today, but they are
    // two implementations of one thing, and the moment they disagree this fake
    // would accept a size it does not store — which is precisely the property
    // it exists to pin. Measuring and storing are now one operation, so they
    // cannot drift apart.
    this.#attachment = serialize(value);
    this.attachmentWrites += 1;
  }

  deserializeAttachment(): unknown {
    // Deserialized afresh each call, because the platform is: two reads are
    // never the same object, and mutating one is not visible to the next.
    return this.#attachment === null ? null : deserialize(this.#attachment);
  }
}

/**
 * The two statements `room/hosts.ts` issues, and nothing else.
 *
 * A fake D1 that answered any query would let a future edit put the fleet's
 * state on the attach path and still pass — so an unrecognised statement throws
 * by name here. The `hosts` row is modelled as a set and a map rather than as
 * SQL, because what the tests are about is *how many* queries the connect path
 * makes, not what SQLite does with them.
 */
class FakeStatement implements D1PreparedStatement {
  readonly #db: FakeDb;
  readonly #query: string;
  readonly #values: unknown[];

  constructor(db: FakeDb, query: string, values: unknown[]) {
    this.#db = db;
    this.#query = query;
    this.#values = values;
  }

  bind(...values: unknown[]): D1PreparedStatement {
    return new FakeStatement(this.#db, this.#query, [...this.#values, ...values]);
  }

  async first<T>(): Promise<T | null> {
    if (!this.#query.includes('SELECT 1 AS ok FROM hosts')) {
      throw new Error(`the relay does not read this: ${this.#query}`);
    }
    this.#db.selects += 1;
    const [id] = this.#values;
    return this.#db.enrolled.has(String(id)) ? ({ ok: 1 } as T) : null;
  }

  async run(): Promise<unknown> {
    if (!this.#query.includes('UPDATE hosts SET last_seen_at')) {
      throw new Error(`the relay does not write this: ${this.#query}`);
    }
    this.#db.updates += 1;
    const [at, id] = this.#values;
    this.#db.lastSeen.set(String(id), Number(at));
    return {};
  }
}

export class FakeDb implements D1Binding {
  /** Host ids with a live, unrevoked row. */
  readonly enrolled = new Set<string>();
  readonly lastSeen = new Map<string, number>();

  /** Counted apart, because only one of the two is cached. */
  selects = 0;
  updates = 0;

  prepare(query: string): D1PreparedStatement {
    return new FakeStatement(this, query, []);
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

  /**
   * How many times the pairing was re-derived from the platform.
   *
   * The structural half of the eviction guard, and it pins *where* an answer
   * came from rather than only that the answer was right. That is not a
   * refinement: a room that answered "is a host present" from an instance
   * field, set when the `hello` was accepted, gives the correct answer in a
   * run against one live object — first `false`, then `true` — and every value
   * assertion passes. This counter is the only thing that fails it there.
   * (Measured by writing exactly that bug; the evicting run caught it too, the
   * live one caught it here alone.)
   */
  getWebSocketsCalls = 0;

  /** The keepalive pair the room last configured, or `null` if it never did. */
  autoResponse: AutoResponsePair | null = null;

  /** How many times it was configured. Object-wide and idempotent, so this may repeat. */
  autoResponseCalls = 0;

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
      setWebSocketAutoResponse: (pair) => {
        this.autoResponse = pair ?? null;
        this.autoResponseCalls += 1;
      },
    };
  }

  /**
   * Every accepted socket, whatever its tags.
   *
   * Narrowed rather than cast. `acceptWebSocket` takes any `Sock`, so a test
   * that hands over a bare stub is legal — and the old `as FakeSock[]` made
   * that stub look like a `FakeSock` right up to the point `attachmentWrites`
   * read `undefined` off it and the sum came out `NaN`. A silently wrong
   * counter is the worst outcome for an accessor whose entire job is to make
   * "no bookkeeping on the data path" assertable.
   */
  get sockets(): FakeSock[] {
    return [...this.#tags.keys()].filter((ws) => ws instanceof FakeSock);
  }

  /** Attachment writes across every socket, for the "no bookkeeping" assertion. */
  get attachmentWrites(): number {
    const all = [...this.#tags.keys()];
    const mine = this.sockets;
    if (mine.length !== all.length) {
      throw new Error(
        `attachmentWrites counts FakeSock only, and ${all.length - mine.length} of ` +
          `${all.length} accepted sockets are not one — the total would silently undercount`,
      );
    }
    return mine.reduce((n, s) => n + s.attachmentWrites, 0);
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
    this.getWebSocketsCalls += 1;
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

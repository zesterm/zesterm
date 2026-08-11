/**
 * The pipe: what a browser attaching causes to come into existence.
 *
 * ```
 * browser  GET /v1/attach?host=…   (ticket on Sec-WebSocket-Protocol)
 * DO   ->  {"t":"open","v":1,"pipe":"<32 hex>","exp":<ms>}   down the control link
 * daemon   GET /v1/pipe?host=…&pipe=<32 hex>
 * DO   ->  101 to the daemon, then 101 to the browser
 * ```
 *
 * After that the object is a byte pump and nothing else. → ADR-009.
 *
 * # The pipe id is the host leg's only credential
 *
 * The browser leg is authorized by the attach ticket, verified at the edge.
 * The host leg is authorized by **knowing the id**, which is 128 bits from
 * `crypto.getRandomValues`, is published exactly once down a control link that
 * has already proved possession of the host key, and stops being dialable
 * `PIPE_DIAL_TIMEOUT_MS` later. That is the whole check, and it is deliberate:
 * the alternative is a second signature exchange on a path where the daemon has
 * already authenticated, to bind a socket that no longer exists by the time
 * anyone could have guessed at it.
 *
 * Shorter would be wrong. A 64-bit id inside a ten-second window is a number a
 * motivated attacker with an open control channel could plausibly search;
 * 128 bits is not, and the id costs 32 characters of tag.
 *
 * # Everything here is derived from tags, every single time
 *
 * `pipePeer` re-reads `getWebSockets(tag)` on every message. A
 * `Map<WebSocket, WebSocket>` in an instance field is the hibernation bug
 * ADR-009 names by name: it passes every test written against one live object
 * and drops sessions in production after the first idle gap. There is no
 * pairing state in this file at all — only functions over what the platform
 * holds.
 *
 * The one thing that *is* instance memory is `PipeBudgets`, and the reason it
 * is allowed there is at its own doc comment.
 */

import { hex, randomBytes } from '@zesterm/cloud-shared';

import { CONTROL_VERSION } from './control.ts';
import {
  PIPE_BYTE_BURST_BYTES,
  PIPE_BYTE_RATE_PER_SECOND,
  PIPE_MESSAGE_BURST,
  PIPE_MESSAGE_RATE_PER_SECOND,
} from './limits.ts';
import type { RoomState, Sock } from './state.ts';

/** 128 bits. See the header: this is the host leg's only credential. */
export const PIPE_ID_BYTES = 16;

/** As hex. Two characters a byte, which is what `pipeIdIsWellFormed` checks. */
export const PIPE_ID_CHARS = PIPE_ID_BYTES * 2;

/**
 * The namespace pipe tags live in.
 *
 * A constant rather than two spellings of `'pipe:'`, because `pipeTag` writes
 * it and `pipeMembership` reads it back off a socket the platform hands over —
 * two halves of one fact, and a tag that is written under one prefix and looked
 * for under another pairs nothing with anything.
 */
const PIPE_TAG_PREFIX = 'pipe:';

/**
 * The tag both ends of one pipe carry. `getWebSockets(pipeTag(id))` **is** the
 * pairing — there is nowhere else it is written down.
 */
export function pipeTag(id: string): string {
  return `${PIPE_TAG_PREFIX}${id}`;
}

/**
 * Which end of the pipe a socket is.
 *
 * Prefixed like `TAG_CONTROL`, and not bare, because tags share one flat
 * namespace with `pipe:<id>` — the same reason `roomName` prefixes a host id
 * rather than using it raw. A bare `host` tag is the kind of string a later
 * feature keys by accident.
 */
export const ROLE_HOST = 'role:host';
export const ROLE_CLIENT = 'role:client';

export type PipeRole = typeof ROLE_HOST | typeof ROLE_CLIENT;

/** A fresh pipe id. */
export function newPipeId(): string {
  return hex(randomBytes(PIPE_ID_BYTES));
}

/**
 * Cheap enough to run at the edge, before a room is woken.
 *
 * A dial for a syntactically impossible id can never match a pending pipe, so
 * refusing it here is one fewer object wake-up an unauthenticated caller can
 * bill the account for — the same argument `index.ts` makes about `?host=`.
 */
export function pipeIdIsWellFormed(id: string): boolean {
  return id.length === PIPE_ID_CHARS && /^[0-9a-f]+$/.test(id);
}

// --- the control link's half of it -----------------------------------------

/**
 * "Dial back for this pipe, until `exp`."
 *
 * Here rather than in `room/control.ts` because it is the pipe's wire shape
 * that happens to travel down the control link, and the id and the deadline are
 * this file's to explain. The version is the control link's, since that is what
 * carries it.
 *
 * `exp` is absolute epoch milliseconds rather than a duration: the daemon is
 * deciding whether a dial is still worth making after a queue or a suspend, and
 * a duration would need it to remember when the frame arrived.
 */
export function openMessage(args: { pipe: string; exp: number }): string {
  return JSON.stringify({ t: 'open', v: CONTROL_VERSION, pipe: args.pipe, exp: args.exp });
}

// --- close codes -----------------------------------------------------------

/**
 * The ticket was already spent.
 *
 * Distinct from `4401`, and the usual "one code for every refusal" argument
 * does not apply: reaching this at all requires a ticket that verified, so it
 * tells a guesser nothing they did not already hold. What it tells the holder
 * is the difference between "your ticket is bad" and "your client dialled twice
 * with one ticket", which are fixed in different files.
 *
 * **Nothing sends it yet.** The `jti` replay set that would is its own change;
 * the number is reserved here so that change does not have to pick one out of a
 * range whose other members are already spoken for.
 */
export const CLOSE_TICKET_REPLAYED = 4402;

/**
 * The host was asked to dial back and did not, inside `PIPE_DIAL_TIMEOUT_MS`.
 *
 * This is what `4404` cannot say: the control link is parked, so the platform
 * believes the machine is present, and it still did not answer. A daemon that
 * has been suspended with its socket half-alive looks exactly like a connected
 * one until it is asked to do something.
 */
export const CLOSE_PIPE_DIAL_TIMEOUT = 4502;

/**
 * The other end of this pipe is gone.
 *
 * A pipe with one end is not a pipe, and leaving the survivor open would leave
 * a daemon holding a socket for a browser tab that closed hours ago.
 */
export const CLOSE_PIPE_PEER_GONE = 4410;

/**
 * Past the rate cap. `1009` — "message too big" — is the closest thing RFC 6455
 * has, and unlike a 4xxx it is a code a browser's `event.code` reports with a
 * meaning anyone can look up.
 */
export const CLOSE_PIPE_FLOOD = 1009;

// --- who is at the other end -----------------------------------------------

export interface PipeMembership {
  /** `pipe:<id>` — the tag, not the id, because that is what lookups take. */
  readonly tag: string;
  readonly role: PipeRole;
}

/**
 * Is this socket one end of a pipe, and which end?
 *
 * From the platform's tags rather than from an attachment: tags are fixed at
 * `acceptWebSocket` and can never be rewritten, which is exactly right for a
 * fact that is decided once and must not drift. Attachments are for state that
 * changes.
 */
export function pipeMembership(state: RoomState, ws: Sock): PipeMembership | null {
  const tags = state.getTags(ws);
  const tag = tags.find((t) => t.startsWith(PIPE_TAG_PREFIX));
  if (tag === undefined) return null;
  if (tags.includes(ROLE_HOST)) return { tag, role: ROLE_HOST };
  if (tags.includes(ROLE_CLIENT)) return { tag, role: ROLE_CLIENT };
  // A pipe tag with no role is a socket this build did not accept. It cannot be
  // pumped in either direction, so it is not a member.
  return null;
}

/**
 * The other end, looked up **on every single message**.
 *
 * Never memoised. See the header — and note that memoising this within one
 * instance is invisible to any test that only reads the answer, which is why
 * `FakePlatform.getWebSocketsCalls` exists.
 */
export function pipePeer(state: RoomState, member: PipeMembership): Sock | null {
  const want = member.role === ROLE_HOST ? ROLE_CLIENT : ROLE_HOST;
  for (const peer of state.getWebSockets(member.tag)) {
    if (state.getTags(peer).includes(want)) return peer;
  }
  return null;
}

// --- the rate cap ----------------------------------------------------------

/**
 * Two token buckets for one pipe leg: messages, and bytes.
 *
 * **This is instance memory, and that is deliberate rather than an oversight.**
 * A bucket that is lost is a bucket rebuilt *full*, and hibernation only
 * follows a real idle gap — during which a bucket that survived would have
 * refilled to full anyway. So the reconstruction is not merely safe, it is the
 * same answer. The alternative is a storage write per message, which is the one
 * thing ADR-009's cost model forbids outright, to defend a limit whose only
 * lost state is state that would have expired.
 *
 * Contrast the *pairing*, which cannot be reconstructed and therefore lives in
 * tags, and a cumulative *lifetime* volume counter, which also cannot be
 * reconstructed — and which this file deliberately does not keep. A shell
 * session's lifetime volume is legitimately unbounded (a `tail -f` left running
 * for a day, a scrollback dump), so a lifetime cap would cut the honest user
 * long before it inconvenienced anyone flooding at a rate this bucket already
 * refuses. What costs money is rate.
 */
export class PipeBudget {
  #messages = PIPE_MESSAGE_BURST;
  #bytes = PIPE_BYTE_BURST_BYTES;
  #at: number;

  constructor(now: number) {
    this.#at = now;
  }

  /**
   * Spend one message of `size` bytes, or refuse.
   *
   * Refusing spends nothing, so a leg that is over its cap does not dig itself
   * deeper — it is about to be closed anyway, and a bucket that goes negative
   * would misreport how far over the limit a *recovering* peer is.
   */
  admit(size: number, now: number): boolean {
    // Monotonic in `now`: a clock that jumps backwards must neither withdraw
    // tokens nor move the mark back, or the interval it jumped over is handed
    // out a second time when the clock catches up.
    const elapsed = Math.max(0, now - this.#at) / 1000;
    this.#at = Math.max(this.#at, now);
    this.#messages = Math.min(
      PIPE_MESSAGE_BURST,
      this.#messages + elapsed * PIPE_MESSAGE_RATE_PER_SECOND,
    );
    this.#bytes = Math.min(PIPE_BYTE_BURST_BYTES, this.#bytes + elapsed * PIPE_BYTE_RATE_PER_SECOND);

    if (this.#messages < 1 || this.#bytes < size) return false;
    this.#messages -= 1;
    this.#bytes -= size;
    return true;
  }
}

/**
 * Every live leg's buckets, keyed by tag and role.
 *
 * **Keyed by string, never by `Sock`.** After a hibernation gap the platform
 * hands back a different JavaScript object for the same connection, so a map
 * keyed by socket identity would miss every time and hand out a full bucket per
 * message — a rate limit that silently does not limit, which is the worst of
 * the three possible outcomes.
 *
 * It is instance memory for the reason `PipeBudget` gives, with one consequence
 * worth stating: `forget` may run on a different instance from the one that
 * filled the map, in which case it does nothing and the map it would have
 * pruned no longer exists. Both outcomes are the empty map, which is why this
 * needs no coordination.
 */
export class PipeBudgets {
  readonly #legs = new Map<string, PipeBudget>();

  /** How many legs are tracked. For the test that pins where the buckets live. */
  get size(): number {
    return this.#legs.size;
  }

  admit(member: PipeMembership, size: number, now: number): boolean {
    const key = `${member.tag}/${member.role}`;
    let budget = this.#legs.get(key);
    if (budget === undefined) {
      budget = new PipeBudget(now);
      this.#legs.set(key, budget);
    }
    return budget.admit(size, now);
  }

  /**
   * Both legs of a pipe that has ended.
   *
   * Called when either end closes, because a pipe with one end is over. Without
   * it a long-lived busy object accumulates one bucket per pipe it ever carried
   * — small, but unbounded, and unbounded is the property that matters in a
   * process that is meant to run for days.
   */
  forget(tag: string): void {
    this.#legs.delete(`${tag}/${ROLE_HOST}`);
    this.#legs.delete(`${tag}/${ROLE_CLIENT}`);
  }
}

/**
 * Reused rather than constructed per message: this is the data path, and a
 * fresh encoder per frame is allocation the pump does not need to make.
 */
const UTF8 = new TextEncoder();

/** How many bytes a relayed message costs its budget. */
export function messageSize(message: string | ArrayBuffer): number {
  // A string's cost is its UTF-8 length, not its code-unit count — the pipes
  // carry binary `zest-proto` frames, so this only ever runs for a peer that is
  // speaking something unexpected, and undercounting there is the wrong way to
  // be wrong.
  return typeof message === 'string' ? UTF8.encode(message).byteLength : message.byteLength;
}

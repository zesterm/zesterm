/**
 * The control link's handshake: three messages, and the state that survives
 * between them.
 *
 * ```
 * DO -> {"t":"challenge","v":1,"nonce":"<64 hex>","relay_key":"<64 hex>"}
 * D  -> {"t":"hello","v":1,"host":"<64 hex>","label":"andy-mac","sig":"<128 hex>"}
 * DO -> {"t":"ready","v":1}   |   {"t":"error","v":1,"code":"…"}
 * ```
 *
 * `sig` is an ordinary `Role::Host` + `Purpose::Auth` signature over the nonce
 * *bytes*, under the `zesterm-sig-v1` preimage `@zesterm/cloud-shared` already
 * carries — the same construction `zest-mesh`'s pairing uses, which is why
 * there is nothing new for a daemon to implement and why
 * `test/control-golden.test.ts` can pin a signature Rust actually produced.
 *
 * **Text, not binary.** The object's free keepalive is
 * `WebSocketRequestResponsePair('ping','pong')`, whose members are strings, so
 * the control link is a text channel. That is a cost worth naming: JSON and hex
 * are wider than the bytes they carry, and it buys an idle host that costs
 * *nothing* rather than a request every thirty seconds forever. The pipes this
 * link later opens carry `zest-proto` frames as binary and are unaffected.
 *
 * # The nonce lives in the attachment, and this is the one people get wrong
 *
 * The gap between sending `challenge` and receiving `hello` is a network round
 * trip, and the object is evicted between messages — so a nonce in an instance
 * field is a nonce that is gone by the time the answer arrives. Every host
 * authentication then fails after the first idle gap, and passes every test
 * written against a single live instance. It is not the pairing table people
 * think of when they read ADR-009's rule; it is the handshake, and it is the
 * shorter-lived of the two, which is exactly why it looks safe.
 */

import { fromHex, hex, randomBytes, signingPreimage } from '@zesterm/cloud-shared';
import { verifyAsync } from '@noble/ed25519';

import type { RoomState, Sock } from './state.ts';

/** The only control-protocol version this build speaks. */
export const CONTROL_VERSION = 1;

/**
 * Every control link carries this tag, authenticated or not.
 *
 * Tags are fixed at `acceptWebSocket` and cannot be changed afterwards, so they
 * say what *kind* of socket this is and never what state it reached. Whether a
 * link has authenticated lives in its attachment, which can be rewritten.
 */
export const TAG_CONTROL = 'role:control';

/** `zest_mesh::identity::NONCE_LEN`. */
export const NONCE_BYTES = 32;

/** `zest_mesh::identity::SIGNATURE_LEN`. */
export const SIGNATURE_BYTES = 64;

/** A `HostId` is the machine's Ed25519 public key verbatim (ADR-006). */
export const HOST_ID_BYTES = 32;

/**
 * How long a challenge is answerable for.
 *
 * The daemon answers in one round trip, so this covers a slow link and nothing
 * else. It is enforced from the timestamp in the attachment rather than by an
 * alarm, because an alarm is a storage write and a wake-up per unanswered dial
 * — which is precisely what an unauthenticated caller would then be able to
 * bill the account for.
 */
export const CONTROL_HANDSHAKE_TTL_MS = 30_000;

/**
 * A label is diagnostic, and bounded because it reaches an attachment.
 *
 * `hosts.label` is far shorter in practice; the cap is here so a daemon cannot
 * push the attachment towards `MAX_ATTACHMENT_BYTES`, where the *next* field
 * anyone adds starts throwing.
 */
export const MAX_LABEL_CHARS = 128;

/**
 * The longest control message this build will parse.
 *
 * A `hello` over real ids measures 249 characters, and 369 with a label at
 * `MAX_LABEL_CHARS` — `host` and `sig` are hex and dominate it. Both numbers
 * are pinned by `test/control-golden.test.ts` rather than left as prose that
 * quietly stops being true.
 *
 * The bound comes before `JSON.parse` because the bytes are unauthenticated at
 * that point: the signature inside them is exactly what has not been checked yet.
 */
export const MAX_CONTROL_MESSAGE_CHARS = 1024;

/**
 * The keepalive pair, which the object answers **without waking**.
 *
 * ADR-009's "an idle host costs nothing" is this line and nothing else. The
 * strings are the platform's to compare literally, so they are constants rather
 * than anything computed.
 */
export const KEEPALIVE_REQUEST = 'ping';
export const KEEPALIVE_RESPONSE = 'pong';

// --- close codes -----------------------------------------------------------

/**
 * The daemon spoke something this build does not: not a `hello`, a version from
 * the future, a stale challenge.
 */
export const CLOSE_CONTROL_PROTOCOL = 4400;

/** The key did not sign the nonce, or this machine is not enrolled here. */
export const CLOSE_CONTROL_REFUSED = 4401;

/** A newer control link for this host authenticated; this one is superseded. */
export const CLOSE_CONTROL_REPLACED = 4409;

/** The relay holds no key of its own, so there is nothing for a daemon to pin. */
export const CLOSE_CONTROL_UNCONFIGURED = 4503;

/**
 * The room exists and the holder may enter it; no host is connected.
 *
 * Distinct from `CLOSE_PIPE_DIAL_TIMEOUT`, which is the *other* way an attach
 * can fail with the account in perfect order: "your Mac is asleep" and "your
 * Mac is connected and did not answer" are different problems and only one of
 * them is the user's.
 */
export const CLOSE_HOST_ABSENT = 4404;

/**
 * Why a control link was refused.
 *
 * Sent to the daemon as `{"t":"error","code":…}` before the close, because the
 * far end here is `zest-daemon` and not a browser: a machine that cannot park
 * its link needs to know whether to re-enrol, re-check its clock, or upgrade.
 * That is the opposite trade-off from the attach path, where every refusal
 * collapses to one code precisely because the far end may be a stranger with a
 * guess.
 */
export type ControlErrorCode =
  | 'malformed'
  | 'version'
  | 'stale'
  | 'wrong-room'
  | 'bad-signature'
  | 'unknown-host'
  | 'unconfigured';

/** The close code that goes with each refusal. The frame carries the detail. */
export function closeCodeFor(code: ControlErrorCode): number {
  switch (code) {
    case 'bad-signature':
    case 'unknown-host':
    case 'wrong-room':
      return CLOSE_CONTROL_REFUSED;
    case 'unconfigured':
      return CLOSE_CONTROL_UNCONFIGURED;
    default:
      return CLOSE_CONTROL_PROTOCOL;
  }
}

// --- messages --------------------------------------------------------------

export function challengeMessage(args: { nonce: string; relayKey: string }): string {
  // `relay_key` is snake_case because the whole control protocol is: the other
  // implementation of it is Rust with `serde`, and a mixed convention is a
  // rename attribute somebody forgets on the one field that gets added later.
  return JSON.stringify({
    t: 'challenge',
    v: CONTROL_VERSION,
    nonce: args.nonce,
    relay_key: args.relayKey,
  });
}

export function readyMessage(): string {
  return JSON.stringify({ t: 'ready', v: CONTROL_VERSION });
}

export function errorMessage(code: ControlErrorCode): string {
  return JSON.stringify({ t: 'error', v: CONTROL_VERSION, code });
}

/** What a daemon claims, once it is well-formed. Nothing here is verified. */
export interface Hello {
  readonly host: string;
  /**
   * **Not signed**, and therefore never authoritative.
   *
   * The signature covers the nonce alone, so the label is whatever the far end
   * typed. It is carried for a log line and for a future "which machine is
   * this" in an admin view, and it must not be written to `hosts.label` or
   * reported to anyone as fact — the account already holds the label a *person*
   * approved at enrolment, and that is the one that means something.
   */
  readonly label: string;
  readonly sig: Uint8Array;
}

export type HelloResult =
  | { readonly ok: true; readonly hello: Hello }
  | { readonly ok: false; readonly error: ControlErrorCode };

/**
 * Field by field, never a cast.
 *
 * These are the bytes an authorization decision is about to be made from, and
 * every malformed shape has to become a refusal rather than a throw: the first
 * daemon that sends a truncated frame must not be a 500 in the object.
 */
export function parseHello(message: string | ArrayBuffer): HelloResult {
  // Binary is not a protocol error the daemon can learn from any other way:
  // the control link is text because the free keepalive is defined over
  // strings, and a daemon sending binary has missed exactly that.
  if (typeof message !== 'string') return { ok: false, error: 'malformed' };
  if (message.length === 0 || message.length > MAX_CONTROL_MESSAGE_CHARS) {
    return { ok: false, error: 'malformed' };
  }

  let value: unknown;
  try {
    value = JSON.parse(message);
  } catch {
    return { ok: false, error: 'malformed' };
  }
  if (typeof value !== 'object' || value === null || Array.isArray(value)) {
    return { ok: false, error: 'malformed' };
  }

  const m = value as Record<string, unknown>;
  if (m['t'] !== 'hello') return { ok: false, error: 'malformed' };
  // Checked after `t`, so a daemon that speaks a later protocol is told
  // `version` rather than `malformed` — the difference between "upgrade the
  // relay" and "you have a bug".
  if (m['v'] !== CONTROL_VERSION) return { ok: false, error: 'version' };

  const host = m['host'];
  const label = m['label'];
  const sig = m['sig'];

  // Lowercase hex only, and the strictness is `fromHex`'s: `hosts.id` is stored
  // as `hex()` writes it, so an uppercase spelling of a key is a different
  // primary key rather than the same machine.
  if (typeof host !== 'string' || fromHex(host, HOST_ID_BYTES) === null) {
    return { ok: false, error: 'malformed' };
  }
  if (typeof label !== 'string' || label.length > MAX_LABEL_CHARS) {
    return { ok: false, error: 'malformed' };
  }
  if (typeof sig !== 'string') return { ok: false, error: 'malformed' };
  const signature = fromHex(sig, SIGNATURE_BYTES);
  if (signature === null) return { ok: false, error: 'malformed' };

  return { ok: true, hello: { host, label, sig: signature } };
}

/**
 * Did the key this `hello` names sign this nonce?
 *
 * `zip215: false` for the reason `ticket.ts` gives at greater length: noble's
 * default accepts small-order keys, which verify almost anything, and dalek's
 * `verify_strict` on the Rust side does not. A control link admitted under
 * permissive rules would be a host id that answers for signatures it never made.
 *
 * `false` for every refusal and never a throw — noble raises on a point that
 * does not decode, and roughly half of all 32-byte strings are not points.
 */
export async function helloSignatureIsValid(hello: Hello, nonce: Uint8Array): Promise<boolean> {
  const key = fromHex(hello.host, HOST_ID_BYTES);
  if (key === null) return false;
  try {
    return await verifyAsync(hello.sig, signingPreimage('host', 'auth', nonce), key, {
      zip215: false,
    });
  } catch {
    return false;
  }
}

// --- what survives eviction ------------------------------------------------

/**
 * A control link's whole memory. **The only place its state is kept.**
 *
 * Short field names because this is serialized on every write and bounded by
 * `MAX_ATTACHMENT_BYTES`; `host` is repeated in the `ready` shape rather than
 * looked up, because after eviction there is nowhere to look it up from.
 */
export type ControlAttachment =
  | {
      readonly s: 'challenged';
      /** The room's host, from the URL the Worker addressed this object with. */
      readonly host: string;
      readonly nonce: string;
      /** When the challenge was sent, for `CONTROL_HANDSHAKE_TTL_MS`. */
      readonly at: number;
    }
  | {
      readonly s: 'ready';
      readonly host: string;
      /** Unsigned. See `Hello['label']`. */
      readonly label: string;
      readonly at: number;
    };

/**
 * Narrowed, never cast.
 *
 * An attachment is bytes the platform wrote for us, so in principle it is
 * trusted — but it is also the *only* thing that survives, and a shape that
 * changed between deploys arrives here as an object from the previous version
 * of this file. `null` then means "not a control link this build understands",
 * which the caller turns into a hang-up rather than into `undefined.host`.
 */
export function readControlAttachment(ws: Sock): ControlAttachment | null {
  const value = ws.deserializeAttachment();
  if (typeof value !== 'object' || value === null) return null;
  const a = value as Record<string, unknown>;

  if (typeof a['host'] !== 'string' || typeof a['at'] !== 'number') return null;
  if (a['s'] === 'challenged' && typeof a['nonce'] === 'string') {
    return { s: 'challenged', host: a['host'], nonce: a['nonce'], at: a['at'] };
  }
  if (a['s'] === 'ready' && typeof a['label'] === 'string') {
    return { s: 'ready', host: a['host'], label: a['label'], at: a['at'] };
  }
  return null;
}

/**
 * Every authenticated control link, re-derived from the platform every call.
 *
 * Never memoised, and not only as a matter of taste: an instance field standing
 * in for this is the hibernation bug ADR-009 names, and it can give the *right*
 * answer against a single live object — first no host, then a host — so a test
 * that only reads the answer does not see it. `test/control.test.ts` runs twice
 * and asserts the platform was actually consulted, which is what does.
 */
export function readyControlLinks(state: RoomState): Sock[] {
  return state
    .getWebSockets(TAG_CONTROL)
    .filter((ws) => readControlAttachment(ws)?.s === 'ready');
}

/**
 * The one link a dial-back can go down, or `null`.
 *
 * Singular by construction rather than by hope: `webSocketMessage` hangs up on
 * every older parked link before it marks a new one ready, precisely so "dial
 * back through which" is a question with an answer. Taking the first of the
 * list here rather than asserting the length, because a second one appearing
 * would be a bug in that path, and refusing every attach in the fleet until it
 * is found is a worse failure than picking one.
 */
export function readyControlLink(state: RoomState): Sock | null {
  return readyControlLinks(state)[0] ?? null;
}

/** A fresh challenge nonce, as hex. */
export function newNonce(): string {
  return hex(randomBytes(NONCE_BYTES));
}

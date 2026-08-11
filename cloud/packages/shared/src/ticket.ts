/**
 * The attach ticket: what the browser shows the relay to prove a host is its
 * own.
 *
 * ```
 * payload  = {"v":1,"jti":…,"aud":"relay","host":…,"user":…,"dev":…,"iat":…,"exp":…}
 * preimage = "zesterm-sig-v1" \0 "relay" \0 "attach-ticket" \0 payload
 * ticket   = base64url(payload) "." base64url(signature)
 * ```
 *
 * Bytes only. The web Worker signs, the relay Worker verifies, and **neither
 * half of Ed25519 is here** — this package has no runtime dependencies, which
 * is what lets every one of its properties be pinned by `node --test` rather
 * than by a deployed Worker. The two Workers each carry `@noble/ed25519`
 * already.
 *
 * **Unpadded base64url is load-bearing, not tidiness.** The ticket rides on
 * `Sec-WebSocket-Protocol`, whose values are HTTP tokens: `/` and `=` are not
 * token characters, so a standard-base64 ticket is a malformed header that
 * some intermediaries pass through and others reject. It would work under
 * `wrangler dev` and fail through a CDN. `bytes.ts`'s `toBase64Url` strips the
 * padding and `fromBase64Url` refuses anything outside the base64url alphabet,
 * so a padded ticket is rejected here rather than mangled in transit.
 *
 * **Verification is over the bytes that arrived, never over a
 * re-serialization.** `decodeAttachTicket` hands back the exact payload bytes
 * it decoded, and that is what the signature covers. Re-encoding the parsed
 * object instead would make verification depend on two JSON writers agreeing
 * about key order, number formatting and escaping — a disagreement that shows
 * up as "the signature is invalid" and names nothing. The field order in
 * `attachTicketPayload` is therefore a convenience for goldens and for a
 * deterministic mint, not a security property.
 */

import { fromBase64Url, fromHex, toBase64Url, utf8 } from './bytes.ts';
import { signingPreimage } from './sig.ts';

/** The only version this build mints or accepts. */
export const ATTACH_TICKET_VERSION = 1;

/**
 * Who the ticket is for. Checked on the relay side, so a ticket minted for
 * some later audience cannot be spent as an attach.
 */
export const ATTACH_TICKET_AUDIENCE = 'relay';

/**
 * Thirty seconds.
 *
 * The ticket is minted and spent in one user gesture — a `POST` and the
 * WebSocket open that immediately follows it — so the window only has to cover
 * a slow round trip, not a session. Long enough that a phone on a bad train
 * connection still lands; short enough that a ticket captured from a log is
 * worthless by the time anyone reads it.
 *
 * **The window is the whole defence today.** Nothing yet remembers a spent
 * `jti`, so a ticket captured inside its thirty seconds can be presented more
 * than once — and what that buys is a second pipe to a host that will still run
 * the `zest-proto` handshake and still refuse an unpaired client. When the
 * replay set lands in the room's storage it need only remember `jti` for this
 * long, which is why the constant is here rather than there.
 */
export const ATTACH_TICKET_TTL_MS = 30_000;

/** An Ed25519 signature. Always 64 bytes; a short one is an error, never a pad. */
export const ATTACH_TICKET_SIGNATURE_LEN = 64;

/** A `HostId` is the machine's Ed25519 public key verbatim (ADR-006), so 32 bytes. */
export const HOST_ID_BYTES = 32;

/**
 * How long a ticket may be before it is refused unread.
 *
 * The header is attacker-supplied and reaches `JSON.parse` before any
 * signature is checked, so the cheap bound comes first. A ticket over real ids
 * measures 462 characters — `host`, `dev` and `user` are hex and dominate it —
 * so a kilobyte is twice over any of them growing, and far short of anything
 * worth parsing.
 */
export const MAX_ATTACH_TICKET_CHARS = 1024;

/**
 * The claims, all of them.
 *
 * `dev` is the *session* the ticket was minted for — `sessions.id`, which is
 * `sha256(cookie)` and therefore not a credential. There is no session-to-
 * device link in the schema, so this is the closest thing the cookie path has
 * to "which browser asked", and it is carried for attribution: the relay never
 * authorizes on it. `user` and `host` are what a pipe is allowed to be.
 */
export interface AttachTicket {
  readonly v: typeof ATTACH_TICKET_VERSION;
  /** Unique per mint. Carried for the replay set; nothing reads it yet. */
  readonly jti: string;
  readonly aud: typeof ATTACH_TICKET_AUDIENCE;
  /** The host id, 64 lowercase hex — the room this ticket admits its holder to. */
  readonly host: string;
  readonly user: string;
  readonly dev: string;
  readonly iat: number;
  readonly exp: number;
}

/** The canonical payload bytes for `ticket`. */
export function attachTicketPayload(ticket: AttachTicket): Uint8Array {
  // Written out field by field rather than `JSON.stringify(ticket)`: the order
  // is then this file's rather than the caller's object literal's, so a mint
  // built from a spread of some larger record cannot silently change the bytes
  // — or ship a field that record happened to carry.
  return utf8(
    JSON.stringify({
      v: ticket.v,
      jti: ticket.jti,
      aud: ticket.aud,
      host: ticket.host,
      user: ticket.user,
      dev: ticket.dev,
      iat: ticket.iat,
      exp: ticket.exp,
    }),
  );
}

/**
 * The bytes the account service's key is asked to sign.
 *
 * Takes the payload *bytes*, not the ticket, because the verifier only ever
 * has bytes — passing it an object would be the invitation to re-serialize
 * that this file's header rules out.
 */
export function attachTicketPreimage(payload: Uint8Array): Uint8Array {
  return signingPreimage('relay', 'attach-ticket', payload);
}

/** `base64url(payload) "." base64url(signature)`. */
export function encodeAttachTicket(payload: Uint8Array, signature: Uint8Array): string {
  return `${toBase64Url(payload)}.${toBase64Url(signature)}`;
}

export interface DecodedAttachTicket {
  readonly ticket: AttachTicket;
  /** The payload exactly as it arrived — the bytes the signature covers. */
  readonly payload: Uint8Array;
  readonly signature: Uint8Array;
}

/**
 * Split, decode and narrow — and **verify nothing**.
 *
 * The split is deliberate: this file has no Ed25519 and the caller does, so
 * "is it well formed" and "did our key sign it" are two functions in two
 * packages. Every malformed shape collapses to one `null`, because a caller
 * distinguishing them would report which, and "that was a real ticket, one
 * second late" is worth more to whoever is guessing than to anyone else.
 */
export function decodeAttachTicket(text: string): DecodedAttachTicket | null {
  if (text.length === 0 || text.length > MAX_ATTACH_TICKET_CHARS) return null;

  const dot = text.indexOf('.');
  if (dot === -1) return null;
  // Exactly one separator. `.` is outside the base64url alphabet, so a second
  // one would be caught below anyway — but rejecting it here means the two
  // halves are unambiguous rather than "the first split that happens to parse".
  if (text.indexOf('.', dot + 1) !== -1) return null;

  const payload = fromBase64Url(text.slice(0, dot));
  const signature = fromBase64Url(text.slice(dot + 1));
  if (payload === null || signature === null) return null;
  if (signature.length !== ATTACH_TICKET_SIGNATURE_LEN) return null;

  let value: unknown;
  try {
    value = JSON.parse(new TextDecoder().decode(payload));
  } catch {
    return null;
  }

  const ticket = narrowTicket(value);
  return ticket === null ? null : { ticket, payload, signature };
}

/**
 * Field by field, never a cast.
 *
 * This is the one place bytes from the network become claims an authorization
 * decision is made from, so an `exp` that is a string — which compares `>`
 * against a number in ways nobody intends — must not get past here.
 */
function narrowTicket(value: unknown): AttachTicket | null {
  if (typeof value !== 'object' || value === null || Array.isArray(value)) return null;
  const t = value as Record<string, unknown>;

  if (t['v'] !== ATTACH_TICKET_VERSION) return null;
  if (t['aud'] !== ATTACH_TICKET_AUDIENCE) return null;

  const jti = t['jti'];
  const host = t['host'];
  const user = t['user'];
  const dev = t['dev'];
  const iat = t['iat'];
  const exp = t['exp'];

  if (typeof jti !== 'string' || jti.length === 0) return null;
  // Lowercase hex only, and the strictness is `fromHex`'s: `hosts.id` is stored
  // as `hex()` writes it, so an uppercase spelling of a key is a different
  // primary key rather than the same machine — and a ticket the relay compared
  // case-insensitively would admit its holder to a room the mint never named.
  if (typeof host !== 'string' || fromHex(host, HOST_ID_BYTES) === null) return null;
  if (typeof user !== 'string' || user.length === 0) return null;
  if (typeof dev !== 'string' || dev.length === 0) return null;
  if (!isMillis(iat) || !isMillis(exp)) return null;

  return { v: ATTACH_TICKET_VERSION, jti, aud: ATTACH_TICKET_AUDIENCE, host, user, dev, iat, exp };
}

/**
 * A whole number of milliseconds, and narrowing.
 *
 * `Number.isSafeInteger` alone answers the question but does not narrow
 * `unknown`, and the cast that would follow it is exactly the one this file
 * refuses to make about claims off the network.
 */
function isMillis(value: unknown): value is number {
  return typeof value === 'number' && Number.isSafeInteger(value);
}

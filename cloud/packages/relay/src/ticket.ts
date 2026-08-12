/**
 * Verifying an attach ticket, statelessly.
 *
 * The relay holds no session, reads no cookie and — on this path — makes no
 * database round trip. It is handed bytes and a signature, and either our own
 * key signed them for this room within the last thirty seconds or it did not.
 * That is the whole authorization decision, and it is deliberately the whole
 * of it: ADR-009 rejects letting the ticket substitute for pairing, so what
 * this admits is a *pipe*, never a shell. The host runs the `zest-proto`
 * handshake afterwards and decides that separately.
 *
 * The layout is `@zesterm/cloud-shared`'s, so nothing here rebuilds it — the
 * mint and the verify share one copy of the signed bytes precisely because two
 * copies is a disagreement that surfaces as "the ticket is invalid" and names
 * neither side.
 *
 * **Everything runs before the Durable Object is touched.** A refused attach
 * must not cost a room wake-up, or an unauthenticated caller can bill the
 * account by dialling garbage.
 */

import {
  ATTACH_TICKET_TTL_MS,
  attachTicketPreimage,
  decodeAttachTicket,
  fromHex,
  type AttachTicket,
} from '@zesterm/cloud-shared';
import { verifyAsync } from '@noble/ed25519';

/** An Ed25519 public key. Always 32 bytes. */
export const TICKET_KEY_LEN = 32;

/**
 * The ticket was missing, malformed, expired, signed by a key we do not hold,
 * or minted for a different room. One code for all of them on purpose: telling
 * them apart is worth more to whoever is guessing than to the holder of a real
 * ticket, who has none of these problems.
 *
 * Here rather than in `index.ts`, where it sat until the entrypoint turned out
 * to be unable to export a constant at all without stopping the Worker from
 * starting — that module's header has the failure. This is the file the
 * refusal is decided in, so it is where the code belongs anyway.
 */
export const CLOSE_TICKET_REFUSED = 4401;

/**
 * The subprotocol the browser offers first; the ticket is the second entry.
 *
 * Two tokens rather than one joined string, because a WebSocket subprotocol
 * token may not contain the separators a ticket might.
 * `clients/web/packages/app/src/relay-dial.ts` is the other end.
 */
export const RELAY_SUBPROTOCOL = 'zesterm.relay.v1';

const TICKET_PREFIX = 'ticket.';

/**
 * The ticket out of a `Sec-WebSocket-Protocol` header, or `null`.
 *
 * The header is a comma-separated list of offers and the browser sends it
 * exactly as `new WebSocket(url, protocols)` was given it. Order is not relied
 * on: an offer list is the client's preference, and a verifier that assumed
 * position would break the day anything inserts one.
 */
export function ticketFromSubprotocols(header: string | null): string | null {
  if (header === null) return null;
  for (const raw of header.split(',')) {
    const offer = raw.trim();
    if (offer.startsWith(TICKET_PREFIX)) return offer.slice(TICKET_PREFIX.length);
  }
  return null;
}

/**
 * The public keys a ticket may be signed by, from the comma-separated binding.
 *
 * **A list, not one key, and that is a decision.** With a single key, rotating
 * it means deploying the web Worker and the relay Worker atomically — which is
 * not a thing two `wrangler deploy`s can be. With a list, the new key is added
 * here first, the mint switches to it, and the old one is removed later, with
 * no window in which a freshly minted ticket meets a relay that has never
 * heard of the key that signed it.
 *
 * A malformed entry is dropped rather than fatal: a typo in one entry of a
 * rotation list should cost that key, not every attach in the fleet. If every
 * entry is malformed the list is empty and nothing verifies, which is the
 * correct outcome and an obvious one.
 */
export function ticketPublicKeys(binding: string | undefined): Uint8Array[] {
  if (binding === undefined) return [];
  const keys: Uint8Array[] = [];
  for (const raw of binding.split(',')) {
    const key = fromHex(raw.trim(), TICKET_KEY_LEN);
    if (key !== null) keys.push(key);
  }
  return keys;
}

/**
 * Did we sign this ticket, for this room, and is it still alive?
 *
 * `null` for every refusal, and never a throw. Noble raises on a malformed
 * point or a wrong-sized input, and a caller reasoning about "is this holder
 * allowed in" must not also have to reason about an exception.
 *
 * `zip215: false` is not a detail. Noble's default is ZIP-215 semantics, which
 * accept non-canonical encodings and small-order public keys; a small-order
 * key verifies almost anything, so one that reached `TICKET_PUBLIC_KEYS` by
 * accident would admit every caller to every room. `packages/web/src/enroll/
 * preimage.ts` sets it for the same reason and matches dalek's `verify_strict`.
 */
export async function verifyAttachTicket(args: {
  text: string;
  host: string;
  keys: readonly Uint8Array[];
  now: number;
}): Promise<AttachTicket | null> {
  const { text, host, keys, now } = args;

  const decoded = decodeAttachTicket(text);
  if (decoded === null) return null;

  // Before the signature, because both are cheap and this one is cheaper: a
  // ticket for another room is refused without an Ed25519 verification at all.
  if (decoded.ticket.host !== host) return null;
  if (decoded.ticket.exp <= now) return null;
  // The relay bounds the lifetime as well as the expiry, and it is the only
  // party that can. `exp` is chosen by the mint; a bug there — a stray unit, a
  // configurable that grew — would issue signed bearer tickets good for hours,
  // and every one of them would verify here. Nothing downstream would notice,
  // because a valid signature is exactly what a long-lived ticket has.
  if (decoded.ticket.exp - decoded.ticket.iat > ATTACH_TICKET_TTL_MS) return null;

  // No clock-skew allowance, and none wanted: the mint and this run on
  // Cloudflare's clock, and an `iat` tolerance is a window in which a ticket
  // minted for later is spendable now.
  if (decoded.ticket.iat > now) return null;

  // Built from the bytes that arrived, never from a re-serialization of the
  // claims — see `ticket.ts` in `@zesterm/cloud-shared` for why that would make
  // verification depend on two JSON writers agreeing.
  const preimage = attachTicketPreimage(decoded.payload);

  for (const key of keys) {
    try {
      if (await verifyAsync(decoded.signature, preimage, key, { zip215: false })) {
        return decoded.ticket;
      }
    } catch {
      // A key that is not a point, or a signature that is not one. The next
      // key in a rotation list still deserves its turn.
    }
  }
  return null;
}

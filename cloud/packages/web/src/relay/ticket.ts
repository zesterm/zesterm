/**
 * Minting an attach ticket: the only place the account service's signing key
 * is used.
 *
 * The format is `@zesterm/cloud-shared`'s, so this file is the Ed25519 half and
 * nothing else — the byte layout lives in one package precisely because the
 * relay Worker rebuilds it to verify and two copies of a signed layout is a
 * disagreement waiting to surface as "the ticket is invalid".
 *
 * **The ticket says what may be attached, not what may be run.** It admits its
 * holder to one host's room at the relay for thirty seconds. The host still
 * runs the `zest-proto` handshake and still decides whether this client is
 * paired; ADR-009 names letting the ticket substitute for pairing as rejected
 * in so many words. The relay authorizes transport; the host authorizes shells.
 */

import {
  ATTACH_TICKET_TTL_MS,
  attachTicketPayload,
  attachTicketPreimage,
  encodeAttachTicket,
  randomBytes,
  toBase64Url,
  type AttachTicket,
} from '@zesterm/cloud-shared';
import { signAsync } from '@noble/ed25519';

/** An Ed25519 seed. Always 32 bytes. */
export const SIGNING_KEY_LEN = 32;

/**
 * 128 bits of `jti`.
 *
 * It is a replay key, not a secret: the relay's room records it and refuses the
 * second attach that carries it, so one mint buys exactly one pipe.
 *
 * Unguessable rather than a counter, which matters *because* something consumes
 * it now: an id an attacker can predict lets them pre-burn one a future mint
 * would use, turning a replay defence into a way to deny a user their own
 * machine. Short enough, too, that the whole ticket stays a comfortable header
 * value.
 */
export const JTI_BYTES = 16;

export interface MintedTicket {
  /** `<base64url(payload)>.<base64url(signature)>` — what the browser presents. */
  readonly ticket: string;
  readonly expiresAt: number;
}

/**
 * Sign a ticket for `host`, on behalf of `user` in session `dev`.
 *
 * Takes the key as bytes rather than as the env string, so the "is this
 * deployment configured" question is answered once by the route — where a
 * missing secret can become an honest 503 — instead of inside a function whose
 * only failure mode would otherwise be a throw on the attach path.
 */
export async function mintAttachTicket(args: {
  signingKey: Uint8Array;
  host: string;
  user: string;
  dev: string;
  now: number;
}): Promise<MintedTicket> {
  const { signingKey, host, user, dev, now } = args;
  if (signingKey.length !== SIGNING_KEY_LEN) {
    throw new RangeError(`a signing key is ${SIGNING_KEY_LEN} bytes, this one is ${signingKey.length}`);
  }

  const expiresAt = now + ATTACH_TICKET_TTL_MS;
  const claims: AttachTicket = {
    v: 1,
    jti: toBase64Url(randomBytes(JTI_BYTES)),
    aud: 'relay',
    host,
    user,
    dev,
    iat: now,
    exp: expiresAt,
  };

  const payload = attachTicketPayload(claims);
  const signature = await signAsync(attachTicketPreimage(payload), signingKey);
  return { ticket: encodeAttachTicket(payload, signature), expiresAt };
}

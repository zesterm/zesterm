/**
 * `@zesterm/cloud-shared` — the pure half of the cloud Workers.
 *
 * Everything here is standard JS plus WebCrypto: no bindings, no runtime
 * globals beyond what Node and workerd both have. That is what lets the
 * security-shaped code — cookie signing, constant-time compare, token
 * hashing — be covered by `node --test` rather than only exercised through a
 * deployed Worker.
 */

export {
  toBase64Url,
  fromBase64Url,
  utf8,
  concat,
  hex,
  fromHex,
  timingSafeEqual,
  randomBytes,
  sha256Base64Url,
} from './bytes.ts';

export { signingPreimage, type Role, type Purpose } from './sig.ts';

export {
  CONTROL_SEEN_BOUND_MS,
  CONTROL_SEEN_REFRESH_MS,
  controlLinkIsLive,
} from './presence.ts';

export {
  ATTACH_TICKET_AUDIENCE,
  ATTACH_TICKET_SIGNATURE_LEN,
  ATTACH_TICKET_TTL_MS,
  ATTACH_TICKET_VERSION,
  MAX_ATTACH_TICKET_CHARS,
  attachTicketPayload,
  attachTicketPreimage,
  decodeAttachTicket,
  encodeAttachTicket,
  type AttachTicket,
  type DecodedAttachTicket,
} from './ticket.ts';

export {
  ATTESTATION_KEY_LEN,
  ATTESTATION_SIGNATURE_LEN,
  ATTESTATION_TTL_MS,
  ATTESTATION_VERSION,
  MAX_ATTESTATION_CHARS,
  attestationMessage,
  decodeAttestation,
  encodeAttestation,
  type AttestationFields,
  type DecodedAttestation,
} from './attestation.ts';

export {
  SESSION_TOKEN_BYTES,
  newSessionToken,
  sessionIdOf,
  looksLikeSessionToken,
  MACHINE_TOKEN_BYTES,
  newMachineToken,
  looksLikeMachineToken,
} from './tokens.ts';

export {
  SESSION_COOKIE,
  OAUTH_COOKIE,
  readCookie,
  setCookie,
  clearCookie,
  sign,
  unsign,
  oauthStateCookie,
  type CookieOptions,
  type OAuthState,
} from './cookies.ts';

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
  SESSION_TOKEN_BYTES,
  newSessionToken,
  sessionIdOf,
  looksLikeSessionToken,
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

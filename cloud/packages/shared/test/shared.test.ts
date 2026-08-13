import { test } from 'node:test';
import assert from 'node:assert/strict';

import {
  clearCookie,
  fromBase64Url,
  fromHex,
  hex,
  looksLikeMachineToken,
  looksLikeSessionToken,
  newMachineToken,
  newSessionToken,
  OAUTH_COOKIE,
  oauthStateCookie,
  readCookie,
  SESSION_COOKIE,
  SESSION_TOKEN_BYTES,
  sessionIdOf,
  setCookie,
  sign,
  timingSafeEqual,
  toBase64Url,
  unsign,
  type OAuthState,
} from '../src/index.ts';

const SECRET = 'test-mac-key-not-a-real-one';

test('base64url survives bytes that plain base64 would mangle', () => {
  // 0xfb/0xff produce '+' and '/' in standard base64 -- exactly the characters
  // that get escaped or truncated somewhere in a cookie or a header.
  const bytes = new Uint8Array([0xfb, 0xff, 0xfe, 0x00, 0x01, 0x7f]);
  const encoded = toBase64Url(bytes);
  assert.doesNotMatch(encoded, /[+/=]/, 'encoded form must be URL- and cookie-safe');
  assert.deepEqual(fromBase64Url(encoded), bytes);
});

test('malformed base64url is rejected rather than decoded to garbage', () => {
  for (const bad of ['not base64!', 'a+b', 'a/b', 'aa==']) {
    assert.equal(fromBase64Url(bad), null, `${bad} should not decode`);
  }
});

test('timingSafeEqual agrees with === on the answer', () => {
  // It cannot be tested for *timing* here without being flaky, so what is
  // pinned is correctness -- including the length cases, where an early return
  // would be the easy mistake.
  assert.ok(timingSafeEqual('abc', 'abc'));
  assert.ok(timingSafeEqual('', ''));
  assert.ok(!timingSafeEqual('abc', 'abd'));
  assert.ok(!timingSafeEqual('abc', 'ab'), 'shorter');
  assert.ok(!timingSafeEqual('ab', 'abc'), 'longer');
  assert.ok(!timingSafeEqual('abc', ''), 'empty against non-empty');
});

test('session tokens are opaque, unique, and never stored as themselves', async () => {
  const a = newSessionToken();
  const b = newSessionToken();
  assert.notEqual(a, b);
  assert.equal(fromBase64Url(a)?.length, SESSION_TOKEN_BYTES);

  const id = await sessionIdOf(a);
  assert.match(id, /^[0-9a-f]{64}$/, 'sessions.id is a hex sha256');
  assert.notEqual(id, a, 'the row key must not be the cookie value');
  assert.equal(await sessionIdOf(a), id, 'hashing is stable');
  assert.notEqual(await sessionIdOf(b), id);
});

test('a malformed token is rejected before any database round trip', () => {
  // The point is that junk costs nothing: an unauthenticated flood of bad
  // cookies must not become a way to load D1.
  assert.ok(looksLikeSessionToken(newSessionToken()));
  assert.ok(!looksLikeSessionToken(''));
  assert.ok(!looksLikeSessionToken('short'));
  assert.ok(!looksLikeSessionToken(toBase64Url(new Uint8Array(16))), 'wrong length');
  assert.ok(!looksLikeSessionToken('!'.repeat(64)), 'not base64url');
});

test('machine tokens carry the greppable prefix, and neither shape passes for the other', () => {
  const machine = newMachineToken();
  assert.ok(machine.startsWith('zt1_'), 'the prefix is what secret scanners key on');
  assert.ok(looksLikeMachineToken(machine));
  assert.ok(!looksLikeMachineToken(machine.slice(4)), 'the prefix is part of the shape');
  assert.ok(!looksLikeMachineToken(`zt1_${toBase64Url(new Uint8Array(16))}`), 'wrong length');
  assert.ok(
    !looksLikeSessionToken(machine),
    'a machine token in a cookie jar must fail shape, not resolve',
  );
  assert.ok(
    !looksLikeMachineToken(newSessionToken()),
    'and a stolen cookie value is not a bearer credential',
  );
});

test('a signed payload round-trips, and a tampered one does not', async () => {
  const payload = { p: 'github', s: 'state-value', v: 'verifier', n: '/hosts', e: 42 };
  const signed = await sign(payload, SECRET);
  assert.deepEqual(await unsign<OAuthState>(signed, SECRET), payload);

  const [body, mac] = signed.split('.') as [string, string];
  const otherBody = toBase64Url(new TextEncoder().encode(JSON.stringify({ ...payload, n: '/evil' })));

  assert.equal(await unsign(`${otherBody}.${mac}`, SECRET), null, 'body swapped under the MAC');
  assert.equal(await unsign(`${body}.${mac.slice(0, -2)}xy`, SECRET), null, 'MAC edited');
  assert.equal(await unsign(signed, 'a-different-secret'), null, 'signed by someone else');
});

test('an unsignable shape returns null instead of throwing', async () => {
  // Boot and callback paths both feed attacker-controlled cookie text in here;
  // a throw would become a 500 where a redirect to login is correct.
  for (const bad of ['', '.', 'nodot', 'a.', '.b', 'a.b', '!!!.???']) {
    assert.equal(await unsign(bad, SECRET), null, `${JSON.stringify(bad)} should not parse`);
  }
});

test('cookies carry every attribute the __Host- prefix requires', async () => {
  // The browser refuses a __Host- cookie missing any of these, and the failure
  // is silent -- the Set-Cookie is simply ignored and the user never logs in.
  const cookie = setCookie(SESSION_COOKIE, 'value', { maxAge: 60 });
  assert.match(cookie, /^__Host-zt_session=value;/);
  for (const attr of ['Path=/', 'HttpOnly', 'Secure', 'SameSite=Lax', 'Max-Age=60']) {
    assert.ok(cookie.includes(attr), `missing ${attr}`);
  }
  assert.ok(!/Domain=/i.test(cookie), '__Host- forbids Domain, and sibling Workers are same-site');
});

test('the OAuth state cookie is Lax, because Strict silently breaks the callback', async () => {
  // The callback is a top-level cross-site GET from the provider. Strict
  // withholds the cookie on exactly that request, and the login then fails as
  // a state mismatch -- which reads as an attack rather than a bug.
  const state: OAuthState = { p: 'github', s: 'x', v: 'y', n: '/', e: 0 };
  const cookie = await oauthStateCookie(state, SECRET, 600);
  assert.ok(cookie.startsWith(`${OAUTH_COOKIE}=`));
  assert.ok(cookie.includes('SameSite=Lax'));
  assert.ok(!cookie.includes('SameSite=Strict'));
});

test('clearing a cookie keeps the attributes a browser matches on', () => {
  const cleared = clearCookie(SESSION_COOKIE);
  assert.ok(cleared.includes('Max-Age=0'));
  assert.ok(cleared.includes('Path=/'), 'a differing Path leaves the original cookie in place');
  assert.ok(cleared.includes('Secure'));
});

test('one cookie is read out of a header carrying several', () => {
  const header = `other=1; ${SESSION_COOKIE}=wanted; ${OAUTH_COOKIE}=also`;
  assert.equal(readCookie(header, SESSION_COOKIE), 'wanted');
  assert.equal(readCookie(header, OAUTH_COOKIE), 'also');
  assert.equal(readCookie(header, 'absent'), null);
  assert.equal(readCookie(null, SESSION_COOKIE), null);
});

test('a cookie whose name merely ends with ours is not mistaken for it', () => {
  // `evil___Host-zt_session=` contains our name as a suffix; a substring match
  // would hand the attacker's value to the session lookup.
  assert.equal(readCookie(`evil${SESSION_COOKIE}=attacker`, SESSION_COOKIE), null);
  assert.equal(readCookie(`${SESSION_COOKIE}_x=attacker`, SESSION_COOKIE), null);
});

test('hex decodes only what hex() could have written', () => {
  // Public keys and signatures arrive as hex from a daemon, and the decoder
  // below this (`@noble/ed25519`) takes a `Uint8Array` of any length and then
  // throws -- so a lenient parser turns "31 bytes arrived" into an exception
  // inside code that was reasoning about cryptography.
  const bytes = new Uint8Array([0x00, 0x0f, 0xff, 0xa5]);
  assert.deepEqual(fromHex(hex(bytes), 4), bytes, 'hex() and fromHex() must round-trip');

  for (const [why, text, len] of [
    ['short', '00', 4],
    ['long', '000f00ffa5', 4],
    ['odd', '000', 2],
    ['not hex at all', 'zzzz', 2],
    ['whitespace', '00 0f', 2],
    // Uppercase is refused rather than folded: these values are stored as, and
    // compared against, `hex()`'s lowercase output, so two spellings of one key
    // would be two rows.
    ['uppercase', 'AABB', 2],
  ] as Array<[string, string, number]>) {
    assert.equal(fromHex(text, len), null, why);
  }
});

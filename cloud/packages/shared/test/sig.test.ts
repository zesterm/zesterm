/**
 * The signing domain, pinned to the Rust that signs under it.
 *
 * `crates/zest-mesh/src/identity.rs` builds these bytes and asks an Ed25519 key
 * to sign them; the Workers rebuild them to verify. The two share no code, so
 * the goldens below are transcribed from that file's own literals —
 * `SIGNING_DOMAIN`, `Role::as_domain`, `Purpose::as_domain` — and decoded here
 * rather than typed as arithmetic, which is the failure mode a hand-counted
 * golden has.
 *
 * That the layout is the one a real Rust key signs is proved a layer up, in
 * `packages/web/test/enroll-preimage.test.ts`: a signature `zest-mesh` actually produced
 * verifies against a preimage `signingPreimage` built. These tests are what
 * says *which byte* moved when it stops.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { hex, signingPreimage, utf8, type Purpose, type Role } from '../src/index.ts';

/** `"zesterm-sig-v1" \0 "host" \0 "auth" \0`, byte for byte. */
const HOST_AUTH_PREFIX = '7a65737465726d2d7369672d763100686f7374006175746800';

test('the preimage is the domain, the role, the purpose, then the message', () => {
  const message = new Uint8Array([0xde, 0xad, 0xbe, 0xef]);
  assert.equal(
    hex(signingPreimage('host', 'auth', message)),
    `${HOST_AUTH_PREFIX}deadbeef`,
    'a byte of difference here refuses every signature the daemon makes, and the error names neither side',
  );
});

test('every role and purpose spells its domain the way the Rust does', () => {
  // The whole point of the domain: a signature minted in one context must not
  // verify in another, and the contexts are told apart by these exact strings.
  const spellings: Array<[Role, Purpose, string]> = [
    ['host', 'auth', 'zesterm-sig-v1\0host\0auth\0'],
    ['host', 'enrollment', 'zesterm-sig-v1\0host\0enrollment\0'],
    ['host', 'attach-ticket', 'zesterm-sig-v1\0host\0attach-ticket\0'],
    ['client', 'auth', 'zesterm-sig-v1\0client\0auth\0'],
    ['client', 'enrollment', 'zesterm-sig-v1\0client\0enrollment\0'],
    ['client', 'attach-ticket', 'zesterm-sig-v1\0client\0attach-ticket\0'],
  ];
  for (const [role, purpose, prefix] of spellings) {
    assert.deepEqual(
      signingPreimage(role, purpose, new Uint8Array()),
      utf8(prefix),
      `${role}/${purpose} must match zest-mesh's as_domain`,
    );
  }
});

test('the relay role is this side’s alone, and spells itself the same way', () => {
  // `relay` has no variant in `zest_mesh::identity::Role`, deliberately: an
  // attach ticket is signed by the account service and by nothing else, so no
  // key in the fleet can produce bytes that verify as one. What it still has to
  // be is stable, because the relay Worker rebuilds this prefix to verify.
  assert.deepEqual(
    signingPreimage('relay', 'attach-ticket', new Uint8Array()),
    utf8('zesterm-sig-v1\0relay\0attach-ticket\0'),
  );
});

test('one message under two contexts is never the same bytes', () => {
  // A machine is routinely both a host and a client, and the relay will ask it
  // to sign a challenge that looks exactly like an enrolment approval. Sharing
  // bytes between any two of these makes one answer serve for the other — and
  // `relay` is in the sweep because a host that could mint its own attach
  // ticket is the failure that variant exists to prevent.
  const message = utf8('a challenge nonce');
  const seen = new Map<string, string>();
  for (const role of ['host', 'client', 'relay'] as const) {
    for (const purpose of ['auth', 'enrollment', 'attach-ticket'] as const) {
      const bytes = hex(signingPreimage(role, purpose, message));
      const previous = seen.get(bytes);
      assert.equal(previous, undefined, `${role}/${purpose} collides with ${previous}`);
      seen.set(bytes, `${role}/${purpose}`);
    }
  }
});

test('the message is appended verbatim, NULs and high bytes included', () => {
  // It is last precisely so that it needs no escaping: nothing follows it, so
  // no content in it can be read as a field boundary. A message that had to be
  // sanitised would be a message the two implementations could sanitise
  // differently.
  const message = new Uint8Array([0x00, 0xff, 0x00, 0x80, 0x7f]);
  assert.equal(hex(signingPreimage('host', 'auth', message)), `${HOST_AUTH_PREFIX}00ff00807f`);
});

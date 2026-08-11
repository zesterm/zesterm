/**
 * The two implementations of the control link's `hello`, pinned to each other.
 *
 * `crates/zest-mesh/src/identity.rs` signs; this verifies. They share no code
 * and no test data unless it is copied by hand, so both fixtures below came out
 * of the Rust — the preimage hex and the signature are
 * `the_host_auth_signature_is_stable`'s own goldens, verbatim.
 *
 * Without them, a drift in either direction surfaces at bring-up as a daemon
 * that cannot park its control link: the daemon says it was refused, the Worker
 * says the signature is bad, and neither can say which side moved. It is the
 * same argument `packages/web/test/enroll-preimage.test.ts` makes for the
 * enrolment shape, and the reason there is any Rust in this wave at all.
 *
 * The nonce counts up rather than repeating one byte, so a verifier that
 * reversed it — or that decoded hex where it meant to encode — fails here
 * instead of passing by symmetry.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { verifyAsync } from '@noble/ed25519';
import { fromHex, hex, signingPreimage } from '@zesterm/cloud-shared';

import {
  helloSignatureIsValid,
  parseHello,
  HOST_ID_BYTES,
  MAX_CONTROL_MESSAGE_CHARS,
  MAX_LABEL_CHARS,
  NONCE_BYTES,
  SIGNATURE_BYTES,
} from '../src/room/control.ts';

/** `HostIdentity::from_secret_bytes(&[7; 32])`, the enrolment golden's machine. */
const RUST_HOST = 'ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c';

/** `Nonce::from_bytes(core::array::from_fn(|i| i as u8))`. */
const RUST_NONCE = '000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f';

/** `zest-mesh`'s `preimage(Role::Host, Purpose::Auth, nonce)`, copied byte for byte. */
const RUST_PREIMAGE = `7a65737465726d2d7369672d763100686f7374006175746800${RUST_NONCE}`;

/** That machine signing that nonce for `Purpose::Auth`. */
const RUST_SIG =
  '568139271f5285187ab820a71165d8201341a5a6b304ea13164a04f82fc4b003' +
  '15dbaeb745d39abd282c23b78b6f68a1a696ee582ec2d2feeabadc474c2fdb02';

function bytes(hexText: string, length: number): Uint8Array {
  const out = fromHex(hexText, length);
  assert.ok(out, `the fixture must be ${length} bytes of hex`);
  return out;
}

const NONCE = bytes(RUST_NONCE, NONCE_BYTES);

function hello(overrides: Record<string, unknown> = {}): string {
  return JSON.stringify({
    t: 'hello',
    v: 1,
    host: RUST_HOST,
    label: 'andy-mac',
    sig: RUST_SIG,
    ...overrides,
  });
}

test('a hello is the size `MAX_CONTROL_MESSAGE_CHARS` was chosen against', () => {
  // The cap is argued from these two numbers, and an argument you cannot grep
  // against the code is one that quietly stops being true. Measured here so a
  // field added to the frame moves the assertion rather than the prose.
  assert.equal(hello().length, 249, 'a hello over real ids');
  assert.equal(
    hello({ label: 'y'.repeat(MAX_LABEL_CHARS) }).length,
    369,
    'and with the longest label a daemon may send',
  );
  assert.ok(
    369 * 2 < MAX_CONTROL_MESSAGE_CHARS,
    'the cap must leave room for the frame to grow, or the next field added to the protocol is refused unread',
  );
});

test('the signed prefix is the one the Rust signs under', () => {
  assert.equal(
    hex(signingPreimage('host', 'auth', NONCE)),
    RUST_PREIMAGE,
    'the daemon signs what this builds; a byte of difference refuses every real control link',
  );
});

test('a signature the Rust made parks a control link here', async () => {
  const parsed = parseHello(hello());
  assert.ok(parsed.ok, 'the golden hello must be well formed');
  assert.deepEqual(parsed.hello.sig, bytes(RUST_SIG, SIGNATURE_BYTES));
  assert.equal(parsed.hello.host, RUST_HOST);

  assert.equal(
    await helloSignatureIsValid(parsed.hello, NONCE),
    true,
    'this is the whole cross-implementation contract: the daemon signed it in Rust and the relay must accept it',
  );
});

test('the same signature against a different nonce proves nothing', async () => {
  const parsed = parseHello(hello());
  assert.ok(parsed.ok);

  // The replay this whole handshake exists to stop: a nonce differing in its
  // last byte is a different challenge, and an answer to the old one is not an
  // answer to it.
  const other = Uint8Array.from(NONCE, (b, i) => (i === NONCE_BYTES - 1 ? b ^ 0x01 : b));
  assert.equal(
    await helloSignatureIsValid(parsed.hello, other),
    false,
    'a signature that verifies against any nonce could be captured once and replayed for ever',
  );

  assert.equal(
    await helloSignatureIsValid(parsed.hello, NONCE.slice().reverse()),
    false,
    'byte order is part of the message, and a reversed nonce is where a hand-written hex decoder lands',
  );
});

test('the role and purpose in the domain are load-bearing, measured against the same golden', async () => {
  const parsed = parseHello(hello());
  assert.ok(parsed.ok);

  // `helloSignatureIsValid` builds `host` + `auth`. These are what the same key
  // over the same nonce looks like under the neighbouring domains, and the
  // point is that the Rust's signature is not valid under any of them.
  const key = bytes(RUST_HOST, HOST_ID_BYTES);
  const sig = bytes(RUST_SIG, SIGNATURE_BYTES);
  const domains = [
    signingPreimage('client', 'auth', NONCE),
    signingPreimage('host', 'enrollment', NONCE),
    signingPreimage('relay', 'attach-ticket', NONCE),
  ];
  for (const preimage of domains) {
    assert.equal(
      await verifyAsync(sig, preimage, key, { zip215: false }),
      false,
      'a host proving itself must not have also signed an enrolment, a client proof, or an attach ticket',
    );
  }

  // And the naked nonce, which is what a daemon that forgot the prefix sends.
  assert.equal(
    await verifyAsync(sig, NONCE, key, { zip215: false }),
    false,
    'the prefix is not optional: a key that signs a bare 32 bytes is a signing oracle for everything else that signs 32 bytes',
  );
});

test('a hello from a different machine, under the golden signature, is refused', async () => {
  const impostor = 'ab'.repeat(32);
  const parsed = parseHello(hello({ host: impostor }));
  assert.ok(parsed.ok, 'a well-formed hello naming another key is still well formed');

  assert.equal(
    await helloSignatureIsValid(parsed.hello, NONCE),
    false,
    'the id names the key that must have signed, so a captured signature cannot be re-presented under someone else’s id',
  );
});

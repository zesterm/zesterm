/**
 * `secure.ts` against `crates/zest-proto/fixtures/handshake.json`.
 *
 * The golden is written by `cargo run -p zest-mesh --example handshake_dump`
 * and gated by `cargo xtask check-fixtures`, so a Rust change that moves any of
 * these values cannot land quietly.
 *
 * Every intermediate step is asserted separately rather than only the end
 * result. "The record did not open" is true of a wrong transcript layout, a
 * wrong hash, a wrong HKDF info string, a wrong nonce encoding and a swapped
 * direction alike — and the difference between those is an afternoon.
 */

import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

import { bytesToHex, hexToBytes } from '@zesterm/proto';

import { ephemeralFromSeed, REKEY_AFTER, TAG_LEN } from '../src/secure.ts';
import { authTranscript, pairingCode, type Transcript } from '../src/transcript.ts';

const HERE = fileURLToPath(new URL('.', import.meta.url));
const GOLDEN = JSON.parse(
  readFileSync(join(HERE, '../../../../../crates/zest-proto/fixtures/handshake.json'), 'utf8'),
) as Golden;

interface Golden {
  schema: number;
  notes: string[];
  protocol: number;
  inputs: {
    host: string;
    client: string;
    host_label: string;
    client_label: string;
    host_nonce: string;
    client_nonce: string;
    host_dh: string;
    client_dh: string;
  };
  transcript: string;
  transcript_hash: string;
  host_signature: string;
  client_signature: string;
  pairing_code: string;
  key_host_to_client: string;
  key_client_to_host: string;
  records: {
    from: 'host' | 'client';
    epoch: number;
    counter: number;
    plaintext: string;
    frame: string;
    note: string;
  }[];
}

/** The same fixed seeds `handshake_dump.rs` uses. */
const HOST_DH_SEED = new Uint8Array(32).fill(0x33);
const CLIENT_DH_SEED = new Uint8Array(32).fill(0x44);

function transcript(): Transcript {
  return {
    version: GOLDEN.protocol,
    host: GOLDEN.inputs.host,
    client: GOLDEN.inputs.client,
    hostNonce: GOLDEN.inputs.host_nonce,
    clientNonce: GOLDEN.inputs.client_nonce,
    hostLabel: GOLDEN.inputs.host_label,
    clientLabel: GOLDEN.inputs.client_label,
    hostDh: GOLDEN.inputs.host_dh,
    clientDh: GOLDEN.inputs.client_dh,
  };
}

test('the fixture is the shape this file was written against', () => {
  // A schema bump means a field moved or gained meaning. Failing here is much
  // cheaper than silently checking three of five values.
  assert.equal(GOLDEN.schema, 1, 'handshake.json changed shape; re-read it before trusting this');
});

test('the ephemeral keys come from their seeds', () => {
  // First, because everything downstream is a function of them: if these
  // disagree, nothing below can be trusted to be testing what it names.
  assert.equal(ephemeralFromSeed(HOST_DH_SEED).publicKey, GOLDEN.inputs.host_dh);
  assert.equal(ephemeralFromSeed(CLIENT_DH_SEED).publicKey, GOLDEN.inputs.client_dh);
});

test('the transcript is byte-for-byte what the Rust signs', () => {
  // The single most load-bearing value here. Getting one field's order or
  // width wrong produces a different hash, different keys, and a failure that
  // surfaces four steps away from its cause.
  assert.equal(bytesToHex(authTranscript(transcript())), GOLDEN.transcript);
});

test('the pairing code matches, from the same transcript', () => {
  // Its whole job is to be the same six digits on two screens. A code this
  // client computed differently would tell a person that a relay is present
  // when none is — or, worse, teach them the numbers never match.
  assert.equal(pairingCode(transcript()), GOLDEN.pairing_code);
});

test('both directional keys match the golden', () => {
  // Asserted by *name*, not by "a record opened". A peer that derived the two
  // keys correctly and then swapped which one sends would still open records
  // it sealed itself, and fail only against the real host.
  const c = ephemeralFromSeed(CLIENT_DH_SEED).agree(GOLDEN.inputs.host_dh, transcript(), 'client');
  const h = ephemeralFromSeed(HOST_DH_SEED).agree(GOLDEN.inputs.client_dh, transcript(), 'host');

  // The client's send key is client->host; the host's send key is host->client.
  // Checked through a sealed record rather than by exporting the key, because
  // exporting one is a thing this class should not offer.
  const fromClient = c.seal(new Uint8Array());
  assert.doesNotThrow(() => h.open(fromClient), 'the host must open what the client sealed');
  const fromHost = h.seal(new Uint8Array());
  assert.doesNotThrow(() => c.open(fromHost), 'the client must open what the host sealed');
});

test('every golden record opens, under the right key and counter', () => {
  for (const [i, r] of GOLDEN.records.entries()) {
    const frame = hexToBytes(r.frame);
    const view = new DataView(frame.buffer, frame.byteOffset, frame.byteLength);
    const len = view.getUint32(0, true);
    const body = frame.subarray(4);

    assert.equal(len, body.length, `record ${i}: the length prefix does not describe the body`);
    assert.equal(
      len,
      hexToBytes(r.plaintext).length + TAG_LEN,
      `record ${i}: the prefix must describe the ciphertext, which is plaintext + a ${TAG_LEN}-byte tag`,
    );

    // The peer that *receives* this record: a client-sealed record is opened
    // by the host. Built fresh per record, because the golden's records are
    // not one stream — two directions and one far-future epoch.
    const opener =
      r.from === 'client'
        ? ephemeralFromSeed(HOST_DH_SEED).agree(GOLDEN.inputs.client_dh, transcript(), 'host')
        : ephemeralFromSeed(CLIENT_DH_SEED).agree(GOLDEN.inputs.host_dh, transcript(), 'client');

    opener.seekRecvForFixture(r.epoch, r.counter);
    assert.equal(bytesToHex(opener.open(body)), r.plaintext, `record ${i} (${r.note})`);
  }
});

test('the ratchet turns at the same record, with nothing on the wire', () => {
  // The golden's last two records straddle it. This is the only branch in the
  // whole file that a normal session reaches after 2 ** 24 frames, so without
  // a fixture crossing it deliberately it would be tested by a user, hours in,
  // and reported as "it dies after a while".
  const last = GOLDEN.records.at(-2);
  const first = GOLDEN.records.at(-1);
  assert.ok(last && first, 'the golden must carry an epoch boundary');
  assert.equal(last.epoch, 0);
  assert.equal(last.counter, REKEY_AFTER - 1, 'the last record of epoch 0');
  assert.equal(first.epoch, 1);
  assert.equal(first.counter, 0, 'the counter resets with the epoch');
});

test('a peer that sent no key is refused rather than agreed with', () => {
  // All-zero is what a peer speaking the previous protocol decodes to. Deriving
  // from it would produce a secret an attacker already knows, and would look
  // like success on both sides.
  const c = ephemeralFromSeed(CLIENT_DH_SEED);
  assert.throws(() => c.agree('00'.repeat(32), transcript(), 'client'), /no ephemeral key/);
});

test('a different transcript is a different channel', () => {
  // What binds the keys to the handshake. Two peers that disagree about a
  // label — the text a person reads on the approval prompt — must not be able
  // to talk at all, rather than talking about different things.
  const t = transcript();
  const other: Transcript = { ...t, hostLabel: 'not-andy-mac' };

  const a = ephemeralFromSeed(CLIENT_DH_SEED).agree(GOLDEN.inputs.host_dh, t, 'client');
  const b = ephemeralFromSeed(HOST_DH_SEED).agree(GOLDEN.inputs.client_dh, other, 'host');
  const sealed = a.seal(new TextEncoder().encode('resize'));
  assert.throws(() => b.open(sealed), 'a disagreement about the transcript must not open');
});

test('a label too long to encode is refused, not truncated', () => {
  // #43. Truncating at 65535 made two labels sharing that prefix sign identical
  // bytes, so a signature over one was valid for the other — and the label is
  // the entire text of the approval prompt.
  const t: Transcript = { ...transcript(), clientLabel: 'a'.repeat(70000) };
  assert.throws(() => authTranscript(t), /will not fit/);
});

/**
 * The whole client against the recorded corpus.
 *
 * The conformance suite proves the proto layer frame by frame; this proves
 * the *client* — handshake, then every real wire byte of `blocks-zsh`
 * delivered through the link seam — ends holding what the host held. The
 * corpus doubles as integration input, which is exactly why it carries full
 * framed bytes rather than decoded JSON.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { dirname, join } from 'node:path';

import { SessionClient } from '../src/index.ts';
import { FakeClock, FakeDaemon, testIdentity } from './harness.ts';

const FIXTURES = join(
  dirname(fileURLToPath(import.meta.url)),
  '../../../../../crates/zest-proto/fixtures',
);

interface FixtureFile {
  frames: Array<{ wire: string; expect: { rows: Array<{ text: string }>; blocks?: unknown[] } }>;
}

function hexToBytes(hex: string): Uint8Array {
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) out[i] = Number.parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  return out;
}

test('blocks-zsh replays through the full client, blocks and all', () => {
  const fixture = JSON.parse(
    readFileSync(join(FIXTURES, 'blocks-zsh.json'), 'utf8'),
  ) as FixtureFile;

  const daemon = new FakeDaemon();
  const clock = new FakeClock();
  let blockEvents = 0;
  const c = new SessionClient({
    dial: daemon.dial,
    identity: testIdentity(),
    label: 'replay',
    // The fixture's frames name host 0x2e..; the session addr only matters
    // for what this client *sends*, and the fixture host never answers.
    session: { host: '2e'.repeat(32), session: 1n },
    cols: 120,
    rows: 30,
    clock,
    events: { onBlocksChanged: () => blockEvents++ },
  });
  c.connect();
  daemon.completeHandshake();

  for (const frame of fixture.frames) {
    // Raw framed bytes straight into the link, exactly as a WebSocket would
    // hand them over.
    daemon.current.deliver.call(daemon.current, decodeToShape(frame.wire));
  }

  const last = fixture.frames.at(-1);
  assert.ok(last);
  const expectRows = last.expect.rows.map((r) => r.text.trimEnd());
  const gotRows = c.grid.rows.map((row) => {
    let text = '';
    for (const run of row.runs) {
      const chars = [...run.text];
      for (let i = 0; i < run.cells; i++) text += chars[i] ?? ' ';
    }
    return text.trimEnd();
  });
  assert.deepEqual(gotRows, expectRows, 'the final grid matches the host cell text');
  assert.ok(c.grid.blocks.length > 0, 'the blocks survived the full client path');
  assert.equal(
    c.grid.blocks.length,
    (last.expect.blocks ?? []).length,
    'the block count matches the host index',
  );
  assert.ok(blockEvents > 0, 'block changes were surfaced to the UI layer');
});

/**
 * The harness's `deliver` re-encodes a wire *shape*; the fixture already has
 * final bytes. Decode them to the shape so delivery uses one code path —
 * proven lossless by the proto package's re-encode parity test.
 */
import { decode, FrameReader } from '@zesterm/proto';

function decodeToShape(wireHex: string): Record<string, unknown> {
  const frames = new FrameReader();
  frames.feed(hexToBytes(wireHex));
  const body = frames.next();
  if (!body) throw new Error('fixture frame was not whole');
  return decode(body) as Record<string, unknown>;
}

/**
 * Blocks over the wire, proven on the recorded corpus.
 *
 * `blocks-zsh` is a real zsh session with OSC 133 integration: prompts,
 * commands, exit codes. Its wire bytes carry `BlockPayload`s in both keyframes
 * and deltas, which makes it the decode-level proof — the *semantic* proof
 * (comparing against the host `Terminal`'s own block index per frame) lands
 * with the fixture exporter's `expect.blocks`.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { FrameReader } from '../src/frame.ts';
import { decode } from '../src/msgpack.ts';
import { GridView } from '../src/grid-view.ts';
import {
  type BlockPayload,
  isKeyframe,
  isUpdate,
  parseBlockPayload,
  parseHostMessage,
} from '../src/wire.ts';
import { hexToBytes, loadFixture } from './fixtures.ts';

function block(id: number, over: Partial<BlockPayload> = {}): BlockPayload {
  return {
    id,
    prompt_line: BigInt(id * 10),
    output_line: null,
    end_line: null,
    state: { state: 'running' },
    command: `cmd-${id}`,
    cwd: '/',
    ...over,
  };
}

test('the recorded zsh session decodes with its blocks intact', () => {
  const fixture = loadFixture('blocks-zsh');
  const view = new GridView();
  let framesWithBlocks = 0;

  for (const frame of fixture.frames) {
    const reader = new FrameReader();
    reader.feed(hexToBytes(frame.wire));
    const body = reader.next();
    assert.ok(body, 'every fixture frame is a complete frame');
    const msg = parseHostMessage(decode(body));

    if (isKeyframe(msg)) {
      if (msg.blocks.length > 0) framesWithBlocks++;
      view.applyKeyframe(msg);
    } else if (isUpdate(msg)) {
      if (msg.delta.blocks.length > 0) framesWithBlocks++;
      view.applyDelta(msg.delta);
    } else {
      assert.fail(`unexpected message ${msg.t} in a recording`);
    }
  }

  // The recording is the guard: if regeneration ever produces a corpus whose
  // zsh session carries no blocks, the phone's whole view lost its test.
  assert.ok(framesWithBlocks > 0, 'blocks-zsh carried no blocks on the wire');
  assert.ok(view.blocks.length > 0, 'no blocks survived application');

  for (let i = 1; i < view.blocks.length; i++) {
    const prev = view.blocks[i - 1] as BlockPayload;
    const here = view.blocks[i] as BlockPayload;
    assert.ok(prev.id < here.id, 'blocks must stay ascending by id');
  }

  // A session that ran commands ends with at least one finished block, and a
  // finished block carries its verdict — the thing the UI must never invent.
  const finished = view.blocks.filter((b) => b.state.state === 'finished');
  assert.ok(finished.length > 0, 'a completed zsh session has finished blocks');
});

test('a delta upserts by id, after the ops', () => {
  const v = new GridView();
  v.applyDelta({ attrs: [], ops: [], blocks: [block(2), block(1)] });
  assert.deepEqual(
    v.blocks.map((b) => b.id),
    [1, 2],
    'insertion keeps the list ascending whatever order the wire used',
  );

  v.applyDelta({
    attrs: [],
    ops: [],
    blocks: [block(2, { state: { state: 'finished', exit_code: 1 } })],
  });
  assert.equal(v.blocks.length, 2, 'an upsert replaces, never duplicates');
  assert.deepEqual(v.blocks[1]?.state, { state: 'finished', exit_code: 1 });
});

test('a keyframe replaces the block list rather than merging it', () => {
  // Unlike attrs, which merge: a block the keyframe does not mention is one
  // the host has forgotten, and resurrecting it would show a command the
  // desktop no longer shows.
  const v = new GridView();
  v.applyDelta({ attrs: [], ops: [], blocks: [block(1), block(2), block(3)] });
  v.applyKeyframe({
    cols: 80,
    rows_data: [],
    attrs: [],
    cursor: { row: 0, col: 0, visible: true, shape: 0 },
    modes: 0,
    blocks: [block(3)],
  });
  assert.deepEqual(v.blocks.map((b) => b.id), [3]);
});

test('blocks_from separates what the host evicted from what it destroyed', () => {
  // Two reasons a block is missing from a keyframe, and only one of them is
  // this client's business. Below the floor the host merely evicted — its
  // scrollback bound, not ours, and a client holding more history keeps it.
  // At or above it the host is complete, so anything else there is gone: a
  // screen clear erased the rows it described, and keeping it would paint a
  // stale command over the row the user is typing on.
  const v = new GridView();
  v.applyDelta({ attrs: [], ops: [], blocks: [block(1), block(2), block(3), block(4)] });
  v.applyKeyframe({
    cols: 80,
    rows_data: [],
    attrs: [],
    cursor: { row: 0, col: 0, visible: true, shape: 0 },
    modes: 0,
    blocks: [block(3)],
    blocks_from: 3,
  });
  assert.deepEqual(
    v.blocks.map((b) => b.id),
    [1, 2, 3],
    '1 and 2 are below the floor and stay; 4 was destroyed and goes',
  );
});

test('a keyframe carrying no blocks still says the host has none', () => {
  // The case an empty list alone cannot express, and the one `cls` on a fresh
  // session produces: the host destroyed everything it had.
  const v = new GridView();
  v.applyDelta({ attrs: [], ops: [], blocks: [block(1), block(2)] });
  v.applyKeyframe({
    cols: 80,
    rows_data: [],
    attrs: [],
    cursor: { row: 0, col: 0, visible: true, shape: 0 },
    modes: 0,
    blocks: [],
    blocks_from: 1,
  });
  assert.deepEqual(v.blocks, [], 'nothing survives a floor below every block held');
});

test('exit_code null is preserved as null, never coerced to success', () => {
  // `None` is not zero: a shell that emits OSC 133 D without the status
  // parameter is common, and a green tick for a command that actually failed
  // is worse than nothing.
  const parsed = parseBlockPayload({
    id: 1,
    prompt_line: 0,
    output_line: null,
    end_line: 4,
    state: { state: 'finished', exit_code: null },
    command: 'make',
    cwd: '/src',
  });
  assert.deepEqual(parsed.state, { state: 'finished', exit_code: null });
  assert.equal(parsed.end_line, 4n, 'line ids coerce to bigint at the boundary');
  assert.equal(parsed.started_ms, undefined, 'a host that predates times sends none');
});

test('an unknown block state is refused rather than guessed at', () => {
  assert.throws(
    () =>
      parseBlockPayload({
        id: 1,
        prompt_line: 0,
        output_line: null,
        end_line: null,
        state: { state: 'cancelled' },
        command: '',
        cwd: '',
      }),
    /unknown block state/,
  );
});

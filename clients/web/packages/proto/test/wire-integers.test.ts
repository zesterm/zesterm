/**
 * The decoded integers are the type the bindings promise: `number` (#14).
 *
 * `rmp_serde` writes the narrowest encoding that fits, so `Seq`, `SessionId`
 * and every line id reach JavaScript as plain numbers for every value a real
 * session produces. The bindings used to say `bigint` and `wire.ts` coerced to
 * match them; now the bindings say `number` and this file pins the decoder to
 * it — through the real MessagePack path, not on pre-decoded objects, because
 * the integer-width behaviour under test lives in that layer.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { encode } from '../src/msgpack-encode.ts';
import { decode } from '../src/msgpack.ts';
import { NO_LINE } from '../src/grid-view.ts';
import { WireError, isKeyframe, isUpdate, parseHostMessage } from '../src/wire.ts';

const HOST = 'ab'.repeat(32);
const CURSOR = { row: 0, col: 0, visible: true, shape: 0 };

/** Encode → decode → parse: the path a live frame body actually takes. */
function roundTrip(msg: Record<string, unknown>): ReturnType<typeof parseHostMessage> {
  return parseHostMessage(decode(encode(msg)));
}

test('seq, session id and line ids decode as plain numbers', () => {
  const msg = roundTrip({
    t: 'update',
    session: { host: HOST, session: 7 },
    base: 2,
    seq: 3,
    delta: {
      attrs: [],
      ops: [
        {
          op: 'row',
          row: 0,
          payload: { line: 42, runs: [{ attr: 0, cells: 2, text: 'hi' }], wrapped: false },
        },
      ],
      blocks: [
        {
          id: 1,
          prompt_line: 41,
          output_line: 42,
          end_line: null,
          state: { state: 'running' },
          command: 'ls',
          cwd: '/',
        },
      ],
    },
  });
  assert.ok(isUpdate(msg));
  // The bindings say `number`; a decoder handing out `bigint` would send every
  // consumer into mixed-type arithmetic that throws at runtime.
  assert.equal(typeof msg.seq, 'number', 'Seq is number in the bindings and on the wire');
  assert.equal(typeof msg.base, 'number');
  assert.equal(typeof msg.session.session, 'number', 'SessionId is number in the bindings');
  const rowOp = msg.delta.ops[0];
  assert.ok(rowOp !== undefined && rowOp.op === 'row');
  assert.equal(typeof rowOp.payload.line, 'number', 'RowPayload.line is number in the bindings');
  const block = msg.delta.blocks[0];
  assert.ok(block !== undefined);
  assert.equal(typeof block.prompt_line, 'number', 'block line ids compare against row line ids');
});

test('a keyframe padded with i64::MIN blank rows still decodes', () => {
  // The one line id outside ±2^53 a host actually sends: the encoder pads a
  // grid that has not filled its height with blank rows at `i64::MIN`
  // (`encode.rs`), and MessagePack delivers that as a bigint. It is a power of
  // two, so it converts to a double exactly — refusing it would make every
  // fresh session's first keyframe undecodable.
  const msg = roundTrip({
    t: 'keyframe',
    session: { host: HOST, session: 1 },
    seq: 0,
    cols: 80,
    rows: 2,
    rows_data: [
      { line: 0, runs: [{ attr: 0, cells: 1, text: '$' }], wrapped: false },
      { line: -(2n ** 63n), runs: [], wrapped: false },
    ],
    attrs: [],
    cursor: CURSOR,
  });
  assert.ok(isKeyframe(msg));
  const blank = msg.rows_data[1];
  assert.ok(blank !== undefined);
  assert.equal(typeof blank.line, 'number', 'the pad sentinel is exact as a double');
  assert.equal(blank.line, NO_LINE, 'and it is the NO_LINE every consumer compares against');
});

test('a line id that a number cannot hold exactly is refused, not rounded', () => {
  // Nothing real sends one — that is the whole argument for `number` — so the
  // honest failure is a loud refusal, never a silently off-by-one id that
  // files rows into the wrong block.
  assert.throws(
    () =>
      roundTrip({
        t: 'keyframe',
        session: { host: HOST, session: 1 },
        seq: 0,
        cols: 80,
        rows: 1,
        rows_data: [{ line: 2n ** 53n + 1n, runs: [], wrapped: false }],
        attrs: [],
        cursor: CURSOR,
      }),
    WireError,
  );
});

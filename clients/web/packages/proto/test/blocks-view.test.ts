/**
 * The block→rows selector and the span renderer, proven on the corpus.
 *
 * `blocks-zsh` is the real recording — a zsh session with OSC 133 integration
 * that ends mid-command, so it exercises finished blocks, a zero-output block
 * (`false`, whose `end_line` sits *before* its `output_line`), and an open
 * block that must render to the bottom. `astral` is the synthetic recording
 * that exists because CJK cannot catch the UTF-16 trap: it puts code points
 * past U+FFFF next to `WIDE` spacers, which is where both a `text.length`
 * count and an unsuppressed spacer become visible.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { FrameReader } from '../src/frame.ts';
import { decode } from '../src/msgpack.ts';
import { GridView, NO_LINE } from '../src/grid-view.ts';
import {
  isKeyframe,
  isUpdate,
  parseHostMessage,
  type BlockPayload,
  type RowPayload,
} from '../src/wire.ts';
import { expandRow, rowText } from '../src/cells.ts';
import { sliceBlocks, outputLineCount } from '../src/blocks-view.ts';
import { rowSpans } from '../src/spans.ts';
import { CellFlags, hasFlag } from '../src/flags.ts';
import { hexToBytes, loadFixture } from './fixtures.ts';

function replay(name: string): GridView {
  const fixture = loadFixture(name);
  const view = new GridView();
  for (const frame of fixture.frames) {
    const reader = new FrameReader();
    reader.feed(hexToBytes(frame.wire));
    const body = reader.next();
    assert.ok(body, 'every fixture frame is a complete frame');
    const msg = parseHostMessage(decode(body));
    if (isKeyframe(msg)) view.applyKeyframe(msg);
    else if (isUpdate(msg)) view.applyDelta(msg.delta);
    else assert.fail(`unexpected message ${msg.t} in a recording`);
  }
  return view;
}

/** The row list `sliceBlocks` walks, reproduced independently. */
function walked(view: GridView): RowPayload[] {
  return [...view.scrollback, ...view.rows].filter((r) => r.line !== NO_LINE);
}

function text(view: GridView, row: RowPayload): string {
  return rowText(expandRow(row, view.cols, view.attrs));
}

test("every block's command appears in its prompt rows", () => {
  const view = replay('blocks-zsh');
  const { slices } = sliceBlocks(view);
  assert.ok(slices.length > 0, 'the recording ran commands, so it must slice into blocks');

  for (const s of slices) {
    if (s.block.command === '') continue;
    const prompt = s.promptRows.map((r) => text(view, r)).join('\n');
    assert.ok(
      prompt.includes(s.block.command),
      `block ${s.block.id}: prompt rows must show ${JSON.stringify(s.block.command)} — ` +
        `a header whose rows hold someone else's command means the selector is off by a line`,
    );
  }
});

test('the layout tiles the walked rows: contiguous, non-overlapping, nothing lost', () => {
  const view = replay('blocks-zsh');
  const { preamble, slices, tail } = sliceBlocks(view);

  // Flattening in layout order and comparing to the walk proves all three at
  // once: a dropped row shortens it, a doubly-assigned row lengthens it, and
  // an out-of-order slice reorders it.
  const flat = [
    ...preamble,
    ...slices.flatMap((s) => [...s.promptRows, ...s.outputRows]),
    ...tail,
  ];
  assert.deepEqual(
    flat.map((r) => r.line),
    walked(view).map((r) => r.line),
    'preamble + slices + tail must be exactly the walked rows, in order',
  );

  for (const s of slices) {
    for (const r of [...s.promptRows, ...s.outputRows]) {
      assert.ok(
        r.line >= s.block.prompt_line &&
          (s.block.end_line === null || r.line <= s.block.end_line),
        `block ${s.block.id} was handed line ${r.line}, outside [prompt_line, end_line]`,
      );
    }
  }
});

test('outputLineCount is the fold count, and a silent command folds to zero', () => {
  const view = replay('blocks-zsh');
  const { slices } = sliceBlocks(view);

  const echo = slices.find((s) => s.block.command.startsWith('echo'));
  assert.ok(echo, 'the recording ran an echo');
  assert.equal(outputLineCount(echo), 1, 'echo printed one line, so its folded header says "1 lines"');

  // `false` prints nothing, and the wire encodes that as end_line *before*
  // output_line — the count must read that as zero, not as a negative range
  // that drags in someone else's rows.
  const silent = slices.find((s) => s.block.command === 'false');
  assert.ok(silent, 'the recording ran false');
  assert.equal(outputLineCount(silent), 0, 'a command that printed nothing folds to "0 lines"');
});

test('a destroying keyframe leaves no orphaned slices', () => {
  const view = replay('blocks-zsh');
  const before = sliceBlocks(view);
  const destroyed = (before.slices[before.slices.length - 1] as { block: { id: number } }).block;

  // The `cls` shape: the keyframe is authoritative from the destroyed block's
  // id up and does not mention it, so the client must drop it.
  view.applyKeyframe({
    cols: view.cols,
    rows_data: view.rows,
    attrs: [],
    cursor: view.cursor,
    modes: view.modes,
    blocks: [],
    blocks_from: destroyed.id,
  });

  const after = sliceBlocks(view);
  assert.ok(
    !after.slices.some((s) => s.block.id === destroyed.id),
    'a slice for a destroyed block would paint a command the host no longer shows',
  );
  const held = new Set(view.blocks.map((b) => b.id));
  for (const s of after.slices) {
    assert.ok(held.has(s.block.id), `slice for block ${s.block.id} which the view no longer holds`);
  }

  // The rows the destroyed block covered are the host's business to erase,
  // not the selector's: until a delta rewrites them they render in the tail
  // rather than vanishing.
  const flat = [
    ...after.preamble,
    ...after.slices.flatMap((s) => [...s.promptRows, ...s.outputRows]),
    ...after.tail,
  ];
  assert.equal(flat.length, walked(view).length, 'destroying a block must not lose rows');
});

test('the open block covers the last non-blank row', () => {
  const view = replay('blocks-zsh');
  const { slices, tail } = sliceBlocks(view);
  const last = slices[slices.length - 1];
  assert.ok(last, 'the recording holds blocks');
  assert.ok(last.open, 'the recording ends mid-command, so its last block has no end yet');
  assert.equal(
    tail.length,
    0,
    'an open block renders to the bottom — a row after it has nowhere sensible to draw',
  );

  const rows = walked(view);
  const bottom = rows[rows.length - 1] as RowPayload;
  assert.ok(
    [...last.promptRows, ...last.outputRows].some((r) => r.line === bottom.line),
    'the open block must reach the bottom row, or a long build stops rendering while it runs',
  );
});

// --- reflow: a width change renumbers line ids -----------------------------
//
// `zest_core`'s reflow (grid/mod.rs) renumbers every line when the width
// changes and reanchors the blocks; the keyframe that follows carries both
// under the *new* numbering, with no mapping for what the client kept under
// the old one. These tests replay that shape synthetically — no recording has
// a resize, which is exactly how the misjoin shipped unnoticed.

function synthRow(line: bigint, text: string): RowPayload {
  return { line, runs: [{ attr: 0, cells: [...text].length, text, marks: [] }], wrapped: false };
}

function synthBlock(
  id: number,
  prompt: bigint,
  output: bigint | null,
  end: bigint | null,
): BlockPayload {
  return {
    id,
    prompt_line: prompt,
    output_line: output,
    end_line: end,
    state: end === null ? { state: 'running' } : { state: 'finished', exit_code: 0 },
    command: `cmd-${id}`,
    cwd: '/',
  };
}

const CURSOR = { row: 0, col: 0, visible: true, shape: 0 } as const;

test('a width-change keyframe stops old-numbering scrollback reaching live blocks', () => {
  const view = new GridView();
  view.applyKeyframe({ cols: 10, rows_data: [synthRow(6n, 'old 6')], attrs: [], cursor: CURSOR, modes: 0 });
  // Lines that scrolled out at 10 cols: kept client-side, ids 6..9.
  view.applyDelta({
    blocks: [],
    attrs: [],
    ops: [6n, 7n, 8n, 9n].map((l) => ({ op: 'sb_push' as const, payload: synthRow(l, `old ${l}`) })),
  });

  // Widening rewraps: the same session now occupies fewer rows, so the live
  // ids come back *lower* than the ids the kept scrollback was recorded under.
  view.applyKeyframe({
    cols: 20,
    rows_data: [2n, 3n, 4n, 5n].map((l) => synthRow(l, `new ${l}`)),
    attrs: [],
    cursor: CURSOR,
    modes: 0,
    blocks: [synthBlock(0, 0n, 1n, 3n), synthBlock(1, 4n, 5n, null)],
    blocks_from: 0,
  });

  const { slices, tail } = sliceBlocks(view);
  assert.equal(
    tail.length,
    0,
    'live viewport rows in the tail is the reflow misjoin: stale scrollback ids above the ' +
      'open block pushed the cursor past it before the real rows arrived',
  );
  const open = slices.find((s) => s.open);
  assert.ok(open, 'the resize keyframe carries an open block');
  assert.deepEqual(
    [...open.promptRows, ...open.outputRows].map((r) => r.line),
    [4n, 5n],
    'the open block must hold exactly its reanchored rows — rows kept under the pre-resize ' +
      'numbering describe other text and must not render inside a live command',
  );
});

test('a width-change keyframe drops evicted blocks anchored in the old numbering', () => {
  const view = new GridView();
  view.applyKeyframe({
    cols: 10,
    rows_data: [6n, 7n, 8n, 9n].map((l) => synthRow(l, `old ${l}`)),
    attrs: [],
    cursor: CURSOR,
    modes: 0,
    blocks: [synthBlock(0, 6n, 7n, 9n)],
    blocks_from: 0,
  });

  // The host evicted block 0 before the resize keyframe (blocks_from: 1), so
  // only the client's copy survives — still anchored at pre-reflow ids that
  // now overlap the renumbered live rows.
  view.applyKeyframe({
    cols: 20,
    rows_data: [2n, 3n, 4n, 5n].map((l) => synthRow(l, `new ${l}`)),
    attrs: [],
    cursor: CURSOR,
    modes: 0,
    blocks: [synthBlock(1, 4n, 5n, null)],
    blocks_from: 1,
  });

  const { slices } = sliceBlocks(view);
  assert.ok(
    slices.every((s) => s.block.id !== 0),
    'a block the reflow reanchored away must not survive under stale anchors — its old ' +
      'range now names live rows and steals them from the block that owns them',
  );
  const rows = walked(view);
  const bottom = rows[rows.length - 1] as RowPayload;
  const open = slices.find((s) => s.open);
  assert.ok(open, 'the resize keyframe carries an open block');
  assert.ok(
    [...open.promptRows, ...open.outputRows].some((r) => r.line === bottom.line),
    'the open block must still reach the bottom row after a resize, or a running build ' +
      'stops rendering the moment the window is widened',
  );
});

test('a same-width keyframe keeps client-side history', () => {
  const view = new GridView();
  view.applyKeyframe({
    cols: 10,
    rows_data: [synthRow(4n, 'live')],
    attrs: [],
    cursor: CURSOR,
    modes: 0,
    blocks: [synthBlock(0, 0n, 1n, 2n)],
    blocks_from: 0,
  });
  view.applyDelta({
    blocks: [],
    attrs: [],
    ops: [{ op: 'sb_push', payload: synthRow(3n, 'scrolled out') }],
  });

  // Height changes and reconnects re-key nothing: line ids only renumber when
  // the *width* changes, so this keyframe must not cost the phone its history.
  view.applyKeyframe({
    cols: 10,
    rows_data: [synthRow(4n, 'live'), synthRow(5n, 'grew')],
    attrs: [],
    cursor: CURSOR,
    modes: 0,
    blocks: [synthBlock(1, 4n, 5n, null)],
    blocks_from: 1,
  });

  assert.equal(
    view.scrollback.length,
    1,
    'a keyframe at the same width did not renumber anything — dropping scrollback here ' +
      'wipes an hour of phone history on every reconnect',
  );
  assert.ok(
    view.blocks.some((b) => b.id === 0),
    'ids are still valid at the same width, so a host-evicted block the client holds ' +
      'must survive the keyframe',
  );
});

test('rowSpans round-trips rowText', () => {
  const view = replay('blocks-zsh');
  let checked = 0;
  for (const row of walked(view)) {
    const spans = rowSpans(row, view.attrs);
    assert.equal(
      spans.map((s) => s.text).join(''),
      rowText(expandRow(row, 0, view.attrs)),
      `line ${row.line}: coalescing must only merge styling — the text is not its to change`,
    );
    checked += 1;
  }
  assert.ok(checked > 0, 'the recording has rows to check');
});

test('an astral-plane row produces no phantom spacer text', () => {
  const view = replay('astral');
  const row = [...view.scrollback, ...view.rows].find((r) =>
    r.runs.some((run) => run.text.includes('\u{1F4A9}')),
  );
  assert.ok(row, 'the astral fixture interleaves ASCII with an astral-plane emoji');

  const spans = rowSpans(row, view.attrs);
  assert.deepEqual(
    spans.map((s) => s.text),
    ['a', '\u{1F4A9}', 'b', '\u{1F4A9}', 'c'],
    'each WIDE spacer carries no visible text — keeping them grows a space per glyph',
  );
  for (const s of spans) {
    assert.equal(
      [...s.text].length,
      1,
      `${JSON.stringify(s.text)} is one code point however many UTF-16 units it takes — ` +
        `counting units shifts every cell after an astral glyph`,
    );
  }
  assert.equal(
    spans.filter((s) => hasFlag(s.flags, CellFlags.WIDE)).length,
    2,
    'the emoji spans keep their WIDE flag so a renderer can still size them',
  );
});

test('a stale open block stops at the next block instead of claiming the session', () => {
  // "An open block never ends, so nothing advances past it" is right for the
  // command still running at the bottom, and only for that one. A host that
  // leaves an *earlier* block open — an abandoned zsh prompt, before #193 —
  // made the first one swallow every row below it: later blocks rendered as
  // bare headers with no rows, and the live prompt was drawn inside a card in
  // the middle of the pane while the prompt line at the bottom sat empty.
  //
  // Fixed in `zest-core`, and defended here too: only the last block may run
  // to the bottom, whatever a host of any age sends.
  const view = new GridView();
  view.applyKeyframe({
    cols: 20,
    rows_data: [0n, 1n, 2n, 3n].map((l) => synthRow(l, `row ${l}`)),
    attrs: [],
    cursor: CURSOR,
    modes: 0,
    blocks: [
      {
        id: 0,
        prompt_line: 0n,
        output_line: null,
        end_line: null,
        state: { state: 'prompt' },
        command: '',
        cwd: '/',
      },
      synthBlock(1, 2n, 3n, null),
    ],
    blocks_from: 0,
  });

  const { slices } = sliceBlocks(view);
  assert.equal(slices.length, 2);
  assert.deepEqual(
    [...(slices[0]?.promptRows ?? []), ...(slices[0]?.outputRows ?? [])].map((r) => r.line),
    [0n, 1n],
    'the stale block is bounded by where the next one starts',
  );
  assert.deepEqual(
    [...(slices[1]?.promptRows ?? []), ...(slices[1]?.outputRows ?? [])].map((r) => r.line),
    [2n, 3n],
    'the live block keeps its own rows — including the row the user is typing on',
  );
});

test('the last block still runs to the bottom', () => {
  // The bound above must not cost a running command its live output: rows
  // arriving below the newest block belong to it, which is what makes a long
  // build readable while it happens rather than only when it finishes.
  const view = new GridView();
  view.applyKeyframe({
    cols: 20,
    rows_data: [0n, 1n, 2n].map((l) => synthRow(l, `row ${l}`)),
    attrs: [],
    cursor: CURSOR,
    modes: 0,
    blocks: [synthBlock(0, 0n, 1n, null)],
    blocks_from: 0,
  });

  const { slices, tail } = sliceBlocks(view);
  assert.deepEqual(
    (slices[0]?.outputRows ?? []).map((r) => r.line),
    [1n, 2n],
    'an open final block takes every row below its output line',
  );
  assert.equal(tail.length, 0, 'nothing escapes past the open block');
});

test('a height-change keyframe keeps the rows that scrolled out of the viewport', () => {
  // Dragging the window's height down and back is the gesture that "every
  // block is gone" comes from. The width never changes, so nothing is
  // renumbered and nothing needs re-anchoring — but the rows that were on
  // screen are now in the host's history, and replacing `rows` wholesale threw
  // away this client's only copy. The blocks anchored there went on naming
  // them, so they rendered as blocks with no rows at all. (#200)
  const view = new GridView();
  view.applyKeyframe({
    cols: 20,
    rows_data: [0n, 1n, 2n, 3n].map((l) => synthRow(l, `line ${l}`)),
    attrs: [],
    cursor: CURSOR,
    modes: 0,
    blocks: [synthBlock(0, 0n, 1n, 3n)],
    blocks_from: 0,
  });

  // The window grew: the host's viewport has moved on and lines 0..3 are
  // history now. Same width, so the ids still mean what they meant.
  view.applyKeyframe({
    cols: 20,
    rows_data: [4n, 5n, 6n, 7n].map((l) => synthRow(l, `line ${l}`)),
    attrs: [],
    cursor: CURSOR,
    modes: 0,
    blocks: [synthBlock(0, 0n, 1n, 3n)],
    blocks_from: 0,
  });

  const { slices } = sliceBlocks(view);
  const rows = [...(slices[0]?.promptRows ?? []), ...(slices[0]?.outputRows ?? [])];
  assert.equal(
    rows.length,
    4,
    'the block came back with no rows: the client dropped the only copy it had of ' +
      'lines the host still holds, which renders as the block having vanished',
  );
  assert.equal(rows[1]?.runs[0]?.text, 'line 1');
});

test('a width-change keyframe still discards the rows it cannot renumber', () => {
  // The counterpart, and the reason the carry-over above is gated on the
  // width: a reflow renumbers every id, so displaced rows cannot be filed
  // under a numbering the keyframe has just replaced.
  const view = new GridView();
  view.applyKeyframe({
    cols: 20,
    rows_data: [0n, 1n, 2n, 3n].map((l) => synthRow(l, `line ${l}`)),
    attrs: [],
    cursor: CURSOR,
    modes: 0,
  });
  view.applyKeyframe({
    cols: 10,
    rows_data: [4n, 5n].map((l) => synthRow(l, `line ${l}`)),
    attrs: [],
    cursor: CURSOR,
    modes: 0,
  });
  assert.equal(view.scrollback.length, 0, 'old-numbering rows must not survive a reflow');
});

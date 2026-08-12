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
import { isKeyframe, isUpdate, parseHostMessage, type RowPayload } from '../src/wire.ts';
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

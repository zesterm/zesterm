/**
 * The decoder rules, one at a time.
 *
 * Ported from the `#[test]`s in `crates/zest-proto/src/decode.rs` and
 * `apply.rs`, keeping their names and their reasons so a failure in either
 * language points at the same paragraph. The corpus replay in
 * `conformance.test.ts` is the real check; these are what say *which* rule broke
 * when it does, and they cover two ops no recording reaches.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { GridView, NO_LINE } from '../src/grid-view.ts';
import { expandRow, rowText } from '../src/cells.ts';
import type { AttrDef, Delta, RowPayload } from '../src/wire.ts';

function row(line: number, text: string): RowPayload {
  return {
    line: BigInt(line),
    runs: [{ attr: 0, cells: [...text].length, text, marks: [] }],
    wrapped: false,
  };
}

const NO_ATTRS = new Map<number, AttrDef>();

test('a scroll moves rows up and blanks the bottom', () => {
  const v = new GridView();
  v.rows = [row(0, 'a'), row(1, 'b'), row(2, 'c')];
  v.applyDelta({ blocks: [], attrs: [], ops: [{ op: 'scroll', top: 0, bottom: 2, lines: 1 }] });

  assert.equal(v.rows[0]?.runs[0]?.text, 'b');
  assert.equal(v.rows[1]?.runs[0]?.text, 'c');
  assert.equal(v.rows[2]?.runs.length, 0, 'the exposed row was not blanked');
  assert.equal(v.rows[2]?.line, NO_LINE, 'a blanked row keeps no line id');
});

test('a scroll outside the grid is a no-op rather than an error', () => {
  const v = new GridView();
  v.rows = [row(0, 'a'), row(1, 'b')];
  const before = [...v.rows];

  for (const op of [
    { op: 'scroll', top: 0, bottom: 9, lines: 1 } as const, // bottom past the end
    { op: 'scroll', top: 1, bottom: 0, lines: 1 } as const, // inverted
    { op: 'scroll', top: 0, bottom: 1, lines: 0 } as const, // nothing to move
    // Negative lines is `usize::try_from(...).unwrap_or(0)` in Rust, so it is a
    // silent no-op and *not* an upward scroll. Matching that exactly matters
    // more than it being the nicer rule.
    { op: 'scroll', top: 0, bottom: 1, lines: -1 } as const,
  ]) {
    v.applyDelta({ blocks: [], attrs: [], ops: [op] });
    assert.deepEqual(v.rows, before, `${JSON.stringify(op)} changed the grid`);
  }
});

test('attributes arrive before the runs that use them', () => {
  // A run naming a style the client has never been told about would render in
  // whatever it happened to be holding. The delta carries both.
  const v = new GridView();
  v.rows = [row(0, 'x')];
  v.applyDelta({
    blocks: [],
    attrs: [{ id: 7, fg: { Indexed: 1 }, bg: 'Default', flags: 0 }],
    ops: [
      {
        op: 'row',
        row: 0,
        payload: {
          line: 0n,
          runs: [{ attr: 7, cells: 3, text: 'red', marks: [] }],
          wrapped: false,
        },
      },
    ],
  });

  assert.ok(v.attrs.has(7));
  assert.deepEqual(expandRow(v.rows[0] as RowPayload, 3, v.attrs)[0]?.fg, { Indexed: 1 });
});

test('a keyframe merges attributes rather than replacing them', () => {
  // The encoder does not reset its interning table on a keyframe, so ids stay
  // meaningful to a client that has been attached throughout. Dropping the
  // table would discard definitions later deltas still name.
  const v = new GridView();
  v.attrs.set(1, { id: 1, fg: 'Default', bg: { Indexed: 2 }, flags: 0 });
  v.applyKeyframe({
    cols: 4,
    rows_data: [row(0, 'ab')],
    attrs: [{ id: 2, fg: 'Default', bg: 'Default', flags: 1 }],
    cursor: { row: 0, col: 0, visible: true, shape: 0 },
    modes: 0,
  });

  assert.ok(v.attrs.has(1), 'a keyframe dropped an attribute a later delta may still name');
  assert.ok(v.attrs.has(2));
});

test('a keyframe that says history was cleared empties scrollback', () => {
  // `history_clears` is what carries an ED 3 to a client that applies
  // keyframes: the rows are not damaged, they are gone, and nothing else in a
  // keyframe can say so. Advanced counter drops the view's history — and
  // suppresses the displaced-row carry-over for the same keyframe, or the
  // pre-cls viewport rows would be filed straight back into scrollback.
  // Unmoved (or absent) counter leaves history alone: eviction stays silent,
  // and a client's longer history stays its own. (#314)
  const cursor = { row: 0, col: 0, visible: true, shape: 0 } as const;
  const v = new GridView();
  v.applyKeyframe({ cols: 20, rows_data: [row(0, 'a'), row(1, 'b'), row(2, 'c')], attrs: [], cursor, modes: 0 });
  v.applyKeyframe({ cols: 20, rows_data: [row(3, 'd'), row(4, 'e'), row(5, 'f')], attrs: [], cursor, modes: 0 });
  assert.ok(v.scrollback.length > 0, 'the view holds no history, so this proves nothing');

  v.applyKeyframe({
    cols: 20,
    rows_data: [row(6, '$'), { line: NO_LINE, runs: [], wrapped: false }, { line: NO_LINE, runs: [], wrapped: false }],
    attrs: [],
    cursor,
    modes: 0,
    history_clears: 1,
  });
  assert.equal(
    v.scrollback.length,
    0,
    'a keyframe carrying an advanced counter left the view holding destroyed history',
  );

  // The same counter again destroys nothing more.
  v.applyKeyframe({ cols: 20, rows_data: [row(7, 'g'), row(8, 'h')], attrs: [], cursor, modes: 0, history_clears: 1 });
  v.applyKeyframe({ cols: 20, rows_data: [row(9, 'i'), row(10, 'j')], attrs: [], cursor, modes: 0, history_clears: 1 });
  assert.ok(v.scrollback.length > 0, 'an unmoved counter re-cleared history it had no right to');
});

test('a row past the end grows the grid rather than being dropped', () => {
  // A resize this client has not been told about. Growing is the forgiving
  // answer; dropping would leave a permanently stale line. `Applier` asks for a
  // keyframe here instead -- it writes into a real grid, which cannot grow one
  // row without also being wrong about its size.
  const v = new GridView();
  v.rows = [row(0, 'a')];
  v.applyDelta({ blocks: [], attrs: [], ops: [{ op: 'row', row: 3, payload: row(3, 'd') }] });

  assert.equal(v.rows.length, 4);
  assert.equal(v.rows[1]?.runs.length, 0, 'the gap was not blanked');
  assert.equal(v.rows[3]?.runs[0]?.text, 'd');
});

test('a run states its cell count independently of its text', () => {
  // The width rule, in isolation: a wide character arrives as two runs -- one
  // carrying the character, one carrying none -- and the client must never
  // recompute either from the text.
  const v = new GridView();
  v.applyDelta({
    blocks: [],
    attrs: [],
    ops: [
      {
        op: 'row',
        row: 0,
        payload: {
          line: 0n,
          runs: [
            { attr: 0, cells: 1, text: '世', marks: [] },
            { attr: 0, cells: 1, text: '', marks: [] },
            { attr: 0, cells: 2, text: 'ab', marks: [] },
          ],
          wrapped: false,
        },
      },
    ],
  });

  const cells = expandRow(v.rows[0] as RowPayload, 4, NO_ATTRS);
  assert.equal(cells.length, 4);
  assert.equal(rowText(cells), '世 ab', 'the spacer must occupy a cell and hold no glyph');
});

test('marks land on the right cell after several runs', () => {
  // `CellMarks.at` is an offset *within its run*. A decoder reading it as a row
  // offset puts the accent on the wrong letter, and only once runs have split.
  const v = new GridView();
  v.applyDelta({
    blocks: [],
    attrs: [],
    ops: [
      {
        op: 'row',
        row: 0,
        payload: {
          line: 0n,
          runs: [
            { attr: 0, cells: 4, text: 'abcd', marks: [] },
            { attr: 0, cells: 2, text: 'ef', marks: [{ at: 1, marks: '́' }] },
          ],
          wrapped: false,
        },
      },
    ],
  });

  const cells = expandRow(v.rows[0] as RowPayload, 6, NO_ATTRS);
  assert.equal(cells[5]?.marks, '́', 'the mark landed on the wrong cell');
  assert.equal(cells[1]?.marks, '', 'the mark was applied as a row offset');
});

test('scrollback accumulates client side', () => {
  // The host may evict a line before this client asks for it, so what scrolled
  // past has to be kept as it goes. Unconditional here: a flat row list has no
  // real history to double-count, which is the guard `Applier` needs and this
  // does not.
  const v = new GridView();
  v.rows = [row(0, 'a'), row(1, 'b')];
  v.applyDelta({
    blocks: [],
    attrs: [],
    ops: [
      { op: 'scroll', top: 0, bottom: 1, lines: 1 },
      { op: 'sb_push', payload: row(0, 'a') },
    ],
  });

  assert.equal(v.scrollback.length, 1, 'nothing was pushed to scrollback');
  assert.equal(v.scrollback[0]?.runs[0]?.text, 'a');
});

test('an unknown attribute renders default rather than failing', () => {
  const cells = expandRow(
    { line: 0n, runs: [{ attr: 9999, cells: 2, text: 'hi', marks: [] }], wrapped: false },
    2,
    NO_ATTRS,
  );
  assert.equal(cells[0]?.fg, 'Default');
  assert.equal(cells[0]?.bg, 'Default');
  assert.equal(cells[0]?.flags, 0);
});

test('alt_screen and modes do not touch each other', () => {
  // Two projections of one value on the encoder's side, so they cannot disagree
  // in practice -- but each op writes only its own field, and a decoder that
  // derives one from the other would drift the moment they arrive separately.
  const v = new GridView();
  v.applyDelta({ blocks: [], attrs: [], ops: [{ op: 'alt_screen', active: true }] });
  assert.equal(v.altScreen, true);
  assert.equal(v.modes, 0, 'alt_screen must not write mode bits');

  v.applyDelta({ blocks: [], attrs: [], ops: [{ op: 'modes', bits: 0b101 }] });
  assert.equal(v.modes, 0b101);
  assert.equal(v.altScreen, true, 'modes must not clear the screen flag');
});

test('both references read erase the same way', () => {
  // `DeltaOp::Erase` is not emitted by the current encoder, so no recording
  // reaches it -- and both Rust references implement it, because two readings of
  // one wire format that disagree about an op is how a client author learns the
  // format is ambiguous. These are the literals from the Rust test of the same
  // name.
  const v = new GridView();
  v.attrs.set(9000, { id: 9000, fg: 'Default', bg: { Indexed: 4 }, flags: 0 });
  v.rows = [row(0, 'abcdefghij'), row(1, 'klmnopqrst')];

  const d: Delta = {
    blocks: [],
    attrs: [],
    ops: [{ op: 'erase', top: 0, left: 2, bottom: 1, right: 5, attr: 9000 }],
  };
  v.applyDelta(d);

  for (const r of [0, 1]) {
    const cells = expandRow(v.rows[r] as RowPayload, 10, v.attrs);
    assert.equal(
      rowText(cells).slice(2, 6),
      '    ',
      `row ${r}: the erased span still holds glyphs`,
    );
    for (let c = 2; c <= 5; c++) {
      assert.deepEqual(
        cells[c]?.bg,
        { Indexed: 4 },
        `row ${r} col ${c}: erase must paint the attribute it names`,
      );
    }
  }
  assert.equal(
    rowText(expandRow(v.rows[0] as RowPayload, 10, v.attrs)).slice(0, 2),
    'ab',
    'erase painted outside the span it named',
  );
});

test('an erase outside the row is a no-op', () => {
  const v = new GridView();
  v.rows = [row(0, 'abc')];
  const before = [...v.rows];
  v.applyDelta({
    blocks: [],
    attrs: [],
    ops: [{ op: 'erase', top: 1, left: 0, bottom: 0, right: 1, attr: 0 }],
  });
  assert.deepEqual(v.rows, before);
});

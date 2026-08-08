/**
 * The bit tables against the Rust definitions.
 *
 * **The recordings cannot catch a mistake here.** Every fixture compares `flags`
 * as a raw integer, so a table that maps `ITALIC` to bit 4 instead of bit 2
 * passes all 60-odd frames. `bits.json` is exported straight from
 * `CellFlags::all().iter_names()` and `Modes::all().iter_names()` for this one
 * purpose, and this is the file that uses it.
 *
 * Checked in **both** directions: a name we are missing is a flag we would
 * silently fail to interpret, and a name we invented is a value that means
 * nothing on the wire.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { CellFlags, KNOWN_CELL_FLAGS, KNOWN_MODES, Modes, cellFlagNames } from '../src/flags.ts';
import { loadBits } from './fixtures.ts';

const bits = loadBits();

test('every CellFlags name and bit matches the Rust', () => {
  assert.deepEqual(
    Object.fromEntries(Object.entries(CellFlags).sort()),
    Object.fromEntries(Object.entries(bits.cell_flags).sort()),
    'the hand-written CellFlags table has drifted from zest-core/src/cell.rs',
  );
});

test('every Modes name and bit matches the Rust', () => {
  assert.deepEqual(
    Object.fromEntries(Object.entries(Modes).sort()),
    Object.fromEntries(Object.entries(bits.modes).sort()),
    'the hand-written Modes table has drifted from zest-core/src/modes.rs',
  );
});

test('the known-bit masks cover exactly what the tables define', () => {
  const flagUnion = Object.values(bits.cell_flags).reduce((a, b) => a | b, 0);
  const modeUnion = Object.values(bits.modes).reduce((a, b) => a | b, 0);
  assert.equal(KNOWN_CELL_FLAGS, flagUnion);
  assert.equal(KNOWN_MODES, modeUnion);
});

test('an unknown attribute bit renders plainer rather than failing', () => {
  // `from_bits_truncate` on the Rust side: "a newer host that has learned an
  // attribute should render plainer here, not fail to render at all". The raw
  // value is still what gets compared -- masking is for interpretation only.
  const withUnknown = CellFlags.BOLD | 0x8000;
  assert.equal(cellFlagNames(withUnknown), 'BOLD|unknown(0x8000)');
  assert.equal(withUnknown & KNOWN_CELL_FLAGS, CellFlags.BOLD);
});

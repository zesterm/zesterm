/**
 * The spine, in TypeScript.
 *
 * Replays every fixture frame by frame and asserts this decoder agrees with the
 * **host terminal** — not with `GridView`, which would only prove the two Rust
 * and TypeScript readings are wrong in the same way.
 *
 * Checked at *every* frame, not at the end: two errors that cancel out would
 * pass a final comparison, and the point is to find the first frame where they
 * diverge.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { FrameReader } from '../src/frame.ts';
import { decode } from '../src/msgpack.ts';
import { isKeyframe, isUpdate, parseHostMessage } from '../src/wire.ts';
import { GridView } from '../src/grid-view.ts';
import { expandRow } from '../src/cells.ts';
import { Modes } from '../src/flags.ts';
import {
  diffRow,
  expandExpectedRow,
  fixtureNames,
  hexToBytes,
  KNOWN_GAPS,
  loadFixture,
} from './fixtures.ts';

const names = fixtureNames();

test('the fixture set is not empty', () => {
  assert.ok(
    names.length >= 5,
    `only ${names.length} fixtures found -- run \`cargo xtask fixtures\`. ` +
      `A loader that silently finds nothing makes every test below pass vacuously.`,
  );
});

test('exactly one field is excluded from the comparison', () => {
  // The Rust comparator names its one exclusion in the failure message so that a
  // later failure cannot be "fixed" by quietly widening the list. Same here: if
  // this number goes up, it happened in a commit that had to say why.
  assert.equal(
    KNOWN_GAPS.length,
    1,
    `the comparison excludes ${KNOWN_GAPS.length} fields: ${KNOWN_GAPS.join(', ')}`,
  );
});

for (const name of names) {
  test(`${name} decodes cell for cell`, () => {
    const fixture = loadFixture(name);
    const view = new GridView();

    let frames = 0;
    let cells = 0;

    for (const [i, frame] of fixture.frames.entries()) {
      // Through the real framing and the real MessagePack reader, not a JSON
      // shortcut -- those two are as much under test as the decoder is.
      const reader = new FrameReader();
      reader.feed(hexToBytes(frame.wire));
      const body = reader.next();
      assert.ok(body !== undefined, `${name} frame ${i}: the wire did not contain a whole frame`);
      assert.equal(
        reader.pending,
        0,
        `${name} frame ${i}: bytes left over after one frame -- the length prefix is being misread`,
      );

      const msg = parseHostMessage(decode(body));

      if (frame.kind === 'keyframe') {
        assert.ok(isKeyframe(msg), `${name} frame ${i}: expected a keyframe, got ${msg.t}`);
        view.applyKeyframe(msg);
        assert.equal(
          msg.seq,
          BigInt(frame.seq),
          `${name} frame ${i}: the keyframe names a different sequence than the fixture`,
        );
      } else {
        assert.ok(isUpdate(msg), `${name} frame ${i}: expected an update, got ${msg.t}`);
        assert.equal(
          msg.base,
          BigInt(frame.base ?? -1),
          `${name} frame ${i}: an update must name the state it builds on`,
        );
        view.applyDelta(msg.delta);
      }

      const want = frame.expect;

      assert.equal(
        view.rows.length,
        want.rows.length,
        `${name} frame ${i}: holding ${view.rows.length} rows, the host had ${want.rows.length}`,
      );

      for (const [r, wantRow] of want.rows.entries()) {
        const viewRow = view.rows[r];
        assert.ok(viewRow !== undefined, `${name} frame ${i} row ${r}: missing`);

        assert.equal(
          viewRow.line,
          BigInt(wantRow.line),
          `${name} frame ${i} row ${r}: absolute line id diverged -- selections and command ` +
            `blocks would name different rows on the two machines`,
        );
        assert.equal(
          viewRow.wrapped,
          wantRow.wrapped,
          `${name} frame ${i} row ${r}: wrap flag diverged -- copying this line would insert ` +
            `a newline the host does not have`,
        );

        const got = expandRow(viewRow, fixture.cols, view.attrs);
        const expected = expandExpectedRow(wantRow, fixture.cols);

        const diff = diffRow(got, expected);
        assert.equal(diff, undefined, `${name} frame ${i} row ${r}: ${diff ?? ''}`);
        cells += got.length;
      }

      // State a renderer reads that no cell comparison covers.
      assert.deepEqual(
        view.cursor,
        want.cursor,
        `${name} frame ${i}: the cursor is in the wrong place`,
      );
      assert.equal(view.modes, want.modes, `${name} frame ${i}: mode bits diverged`);
      assert.equal(
        view.altScreen,
        want.alt_screen,
        `${name} frame ${i}: this client is on the wrong screen`,
      );
      assert.equal(
        view.altScreen,
        (view.modes & Modes.ALT_SCREEN) !== 0,
        `${name} frame ${i}: altScreen and the ALT_SCREEN mode bit disagree`,
      );
      assert.equal(view.title, want.title, `${name} frame ${i}: window title diverged`);

      frames += 1;
    }

    assert.equal(
      frames,
      fixture.frames.length,
      `${name}: replayed ${frames} of ${fixture.frames.length} frames`,
    );
    assert.ok(cells > 0, `${name}: compared no cells at all`);
    console.log(`  ${name}: ${frames} frames, ${cells} cells compared`);
  });
}

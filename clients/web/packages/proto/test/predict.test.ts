/**
 * The echo predictor replayed against `crates/zest-proto/fixtures/predict.json`
 * — the same file `crates/zest-proto/tests/predict.rs` replays, so the two
 * ports are held to one set of rules rather than to a review.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

import { Predictor, type PredictKey, type PredictPolicy } from '../src/predict.ts';
import type { Delta } from '../src/wire.ts';
import { FIXTURES_DIR } from './fixtures.ts';

interface Pos {
  row: number;
  col: number;
}
interface Step {
  at: number;
  input?: PredictKey;
  delta?: Delta;
  keyframe?: { cursor: Pos; cols: number; alt_screen: boolean };
  tick?: boolean;
  expect: {
    overlay: Array<[number, number, string]>;
    pending: number;
    showing: boolean;
    latency_ms: number | null;
  };
}
interface Scenario {
  name: string;
  policy: PredictPolicy;
  remote_hint: boolean;
  cols: number;
  cursor: Pos;
  steps: Step[];
}
interface Fixture {
  schema: number;
  scenarios: Scenario[];
}

const cursor = (p: Pos) => ({ row: p.row, col: p.col, visible: true, shape: 0 });

test('every scenario in predict.json replays', () => {
  const f = JSON.parse(readFileSync(join(FIXTURES_DIR, 'predict.json'), 'utf8')) as Fixture;
  assert.equal(f.schema, 1, 'fixture schema moved; update both replayers together');
  assert.ok(f.scenarios.length > 0);

  for (const sc of f.scenarios) {
    const p = new Predictor(sc.policy);
    p.setRemoteHint(sc.remote_hint);
    p.onKeyframe(cursor(sc.cursor), sc.cols, false);

    sc.steps.forEach((step, i) => {
      const here = `${sc.name} step ${i} (at ${step.at})`;
      if (step.input) p.onInput(step.input, step.at);
      if (step.delta) p.reconcile(step.delta, step.at);
      if (step.keyframe) {
        p.onKeyframe(cursor(step.keyframe.cursor), step.keyframe.cols, step.keyframe.alt_screen);
      }
      if (step.tick) p.tick(step.at);

      const got = p.overlay().map((x) => [x.row, x.col, x.ch]);
      assert.deepEqual(got, step.expect.overlay, `${here}: overlay`);
      assert.equal(p.pending().length, step.expect.pending, `${here}: pending count`);
      assert.equal(p.showing(), step.expect.showing, `${here}: showing`);
      const lat = p.echoLatencyMs();
      if (step.expect.latency_ms === null) {
        assert.equal(lat, null, `${here}: latency`);
      } else {
        assert.ok(lat !== null, `${here}: latency measured`);
        assert.ok(
          Math.abs(lat - step.expect.latency_ms) < 0.01,
          `${here}: latency ${lat} != ${step.expect.latency_ms}`,
        );
      }
    });
  }
});

test('the caret sits after the last guess', () => {
  const p = new Predictor('always');
  p.onKeyframe({ row: 2, col: 5, visible: true, shape: 0 }, 80, false);
  assert.equal(p.caret(), null, 'no guess, the grid cursor is the caret');
  p.onInput({ key: 'printable', ch: 'a' }, 0);
  p.onInput({ key: 'printable', ch: 'b' }, 1);
  assert.deepEqual(p.caret(), { row: 2, col: 7 }, 'the line must read as typed, caret included');
});

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { blockHeaderFill, builtinThemes, obsidian, softHairline, titlebarFill } from '../src/index.ts';
import { contrastRatio, luminance } from '../src/oklch.ts';

function assertNear(got: string, want: string, tolerance: number, why: string): void {
  const ch = (hex: string, i: number): number => parseInt(hex.slice(1 + 2 * i, 3 + 2 * i), 16);
  for (let i = 0; i < 3; i++) {
    assert.ok(
      Math.abs(ch(got, i) - ch(want, i)) <= tolerance,
      `${why}: got ${got}, want ${want} (±${tolerance}/channel)`,
    );
  }
}

test('obsidian lands on the values derived.rs computes (and the mock pins)', () => {
  // Expected values are the Rust crate's own output for obsidian
  // (derived::titlebar_fill etc.); the design README's hexes (#0d121f,
  // #0f1526) are those same surfaces within its own ±4 quantization budget.
  // Drift means the web chrome and the native chrome shade differently.
  const ui = obsidian.ui;
  assertNear(titlebarFill(ui), '#0d121f', 2, 'titlebar');
  assertNear(blockHeaderFill(ui), '#101524', 2, 'block header');
  assertNear(softHairline(ui), '#1b2237', 2, 'soft hairline');
});

test('surfaces sit between chrome and panel in every theme', () => {
  // The reason these are derived rather than literal: the step must run in
  // each theme's own direction — the mock's dark hexes pasted into `paper`
  // would fail this immediately.
  for (const t of builtinThemes) {
    const a = luminance(t.ui.chrome);
    const b = luminance(t.ui.panel);
    const lo = Math.min(a, b) - 0.002;
    const hi = Math.max(a, b) + 0.002;
    for (const [name, c] of [
      ['titlebar', titlebarFill(t.ui)],
      ['block header', blockHeaderFill(t.ui)],
    ] as const) {
      const l = luminance(c);
      assert.ok(
        l >= lo && l <= hi,
        `${t.id}: ${name} (${c}) fell outside the chrome..panel lightness band`,
      );
    }
    assert.ok(
      contrastRatio(softHairline(t.ui), t.ui.chrome) < contrastRatio(t.ui.line, t.ui.chrome),
      `${t.id}: the soft hairline is not quieter than line`,
    );
  }
});

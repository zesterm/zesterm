import { test } from 'node:test';
import assert from 'node:assert/strict';
import { builtinThemes, obsidian, resolveTerminalPalette } from '../src/index.ts';
import { luminance } from '../src/oklch.ts';

/** ±tolerance/255 per channel — the OKLCH parity budget with the Rust crate. */
function assertNear(got: string, want: string, tolerance: number, why: string): void {
  const ch = (hex: string, i: number): number => parseInt(hex.slice(1 + 2 * i, 3 + 2 * i), 16);
  for (let i = 0; i < 3; i++) {
    assert.ok(
      Math.abs(ch(got, i) - ch(want, i)) <= tolerance,
      `${why}: got ${got}, want ${want} (±${tolerance}/channel)`,
    );
  }
}

test('indices 0..15 are the ansi row', () => {
  const p = resolveTerminalPalette(obsidian.ui);
  assert.equal(p.ansi.length, 16);
  for (let i = 0; i < 16; i++) {
    assert.equal(p.indexed(i), p.ansi[i], `indexed(${i}) must alias ansi[${i}]`);
  }
  assert.equal(p.indexed(1), '#e0606a', 'normal red comes straight from ui.red');
  assert.equal(p.defaultFg, '#d7dcea');
  assert.equal(p.defaultBg, '#0b0f1a');
});

test('16..231 follow the xterm 6x6x6 cube formula', () => {
  // Same spot checks as resolve.rs's extended_palette_matches_xterm test, so
  // the two implementations are pinned to the same convention.
  const p = resolveTerminalPalette(obsidian.ui);
  assert.equal(p.indexed(16), '#000000');
  assert.equal(p.indexed(231), '#ffffff');
  assert.equal(p.indexed(196), '#ff0000', '196 = 16 + 36*5: pure red corner');
  // 33 - 16 = 17 = 36*0 + 6*2 + 5, so r=0 g=2 b=5 -> #0087ff.
  assert.equal(p.indexed(33), '#0087ff');
});

test('232..255 are the grayscale ramp, 8 + 10 per step', () => {
  const p = resolveTerminalPalette(obsidian.ui);
  assert.equal(p.indexed(232), '#080808');
  assert.equal(p.indexed(255), '#eeeeee');
});

test('out-of-range indices throw instead of wrapping', () => {
  const p = resolveTerminalPalette(obsidian.ui);
  assert.throws(() => p.indexed(256), RangeError, 'a wrapped index would render the wrong color silently');
  assert.throws(() => p.indexed(-1), RangeError);
  assert.throws(() => p.indexed(1.5), RangeError);
});

test('brights 1..6 match zest-theme\'s oklch::brighten_ansi output', () => {
  // Expected values computed by the Rust crate (oklch::brighten_ansi over
  // obsidian's normal row, printed via a one-off dump of builtin::all()).
  // Drift beyond rounding means the TS OKLCH port diverged from the `palette`
  // crate and the two clients would show different bright colors.
  const p = resolveTerminalPalette(obsidian.ui);
  const rust = ['#ff7c85', '#7af49a', '#ffd35b', '#89c8ff', '#d199ff', '#7ae6ff'];
  rust.forEach((want, i) => {
    assertNear(p.ansi[8 + 1 + i]!, want, 2, `bright ansi ${i + 1}`);
  });
});

test('bright black is faint and bright white is fg — resolve.rs\'s conventional exceptions', () => {
  // Documented divergence from the *builtins'* stored brights (builtin.rs
  // brightens all eight): with only tokens available this follows resolve.rs's
  // default path, where the two ends are conventional rather than computed.
  const p = resolveTerminalPalette(obsidian.ui);
  assert.equal(p.ansi[8], obsidian.ui.faint, 'bright black = the faintest readable grey');
  assert.equal(p.ansi[15], obsidian.ui.fg, 'bright white = the plain foreground');
});

test('brights are lighter than their normals in every builtin', () => {
  // The property brighten_ansi exists for; a regression here means brights
  // stopped reading as bright.
  for (const t of builtinThemes) {
    const p = resolveTerminalPalette(t.ui);
    for (let i = 1; i < 7; i++) {
      assert.ok(
        luminance(p.ansi[i + 8]!) > luminance(p.ansi[i]!),
        `${t.id}: bright color ${i} is not brighter than its normal`,
      );
    }
  }
});

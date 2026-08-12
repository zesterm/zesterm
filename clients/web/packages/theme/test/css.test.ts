import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  applyCssVars,
  applyThemeCss,
  blockHeaderFill,
  builtinThemes,
  cssVarsOf,
  derivedCssVars,
  obsidian,
  paper,
  softHairline,
  titlebarFill,
} from '../src/index.ts';
import { luminance } from '../src/oklch.ts';

// The full --zt-* vocabulary. The design handoff
// (docs/design/client-ui/README.md) pins `--zt-bg` and `--zt-panel` and says
// to drive the rest "straight off" the token record; this is that mapping,
// kebab-cased, written out so a rename in css.ts cannot slip past review.
const EXPECTED_VAR_NAMES = [
  '--zt-bg',
  '--zt-panel',
  '--zt-chrome',
  '--zt-line',
  '--zt-shadow',
  '--zt-fg',
  '--zt-dim',
  '--zt-faint',
  '--zt-accent',
  '--zt-accent-soft',
  '--zt-accent-text',
  '--zt-sel-soft',
  '--zt-success',
  '--zt-warn',
  '--zt-danger',
  '--zt-info',
  '--zt-black',
  '--zt-red',
  '--zt-green',
  '--zt-yellow',
  '--zt-blue',
  '--zt-magenta',
  '--zt-cyan',
  '--zt-white',
];

test('cssVarsOf uses the mockup\'s --zt-* names', () => {
  const vars = cssVarsOf(obsidian.ui);
  assert.deepEqual(
    Object.keys(vars),
    EXPECTED_VAR_NAMES,
    'stylesheets are written against these exact names; a silent rename unstyles the app',
  );
  assert.equal(vars['--zt-bg'], '#0b0f1a', 'the README pins --zt-bg to ui.bg');
  assert.equal(vars['--zt-panel'], '#121829', 'and --zt-panel to ui.panel');
  assert.equal(vars['--zt-accent-soft'], '#16203a', 'camelCase folds to kebab');
});

test('applyCssVars sets every variable on the target', () => {
  // Structural style target: the reason applyCssVars takes { style: {
  // setProperty } } is exactly so this test runs under node with no DOM.
  const seen = new Map<string, string>();
  applyCssVars(obsidian.ui, { style: { setProperty: (k, v) => void seen.set(k, v) } });
  assert.equal(seen.size, 24, 'one property per token');
  assert.equal(seen.get('--zt-accent'), '#6ea8ff');
});

test('derivedCssVars names the three chrome surfaces and lands on the mock for obsidian', () => {
  const vars = derivedCssVars(obsidian.ui);
  assert.deepEqual(
    Object.keys(vars),
    ['--zt-titlebar', '--zt-block-header', '--zt-hairline'],
    'stylesheets are written against these exact names; a rename unstyles the chrome',
  );
  assert.equal(
    vars['--zt-titlebar'],
    titlebarFill(obsidian.ui),
    'the variable must carry the derivation, not a literal',
  );
  assert.equal(vars['--zt-block-header'], blockHeaderFill(obsidian.ui));
  assert.equal(vars['--zt-hairline'], softHairline(obsidian.ui));

  // The handoff writes these surfaces out as hexes exactly so this mapping is
  // checkable — #0d121f / #0f1526 / #1b2338. The titlebar lands verbatim; the
  // other two sit inside the handoff's own ±4/channel quantization budget
  // (the same tolerance derived.rs's test grants), because the derivation is
  // pinned to the Rust crate's math, not to the mock's rounded hexes.
  assert.equal(vars['--zt-titlebar'], '#0d121f', 'the mock pins the obsidian titlebar');
  const near = (got: string, want: string, what: string): void => {
    const ch = (hex: string, i: number): number => parseInt(hex.slice(1 + 2 * i, 3 + 2 * i), 16);
    for (let i = 0; i < 3; i++) {
      assert.ok(
        Math.abs(ch(got, i) - ch(want, i)) <= 4,
        `${what}: ${got} strays from the mock's ${want} beyond its quantization budget`,
      );
    }
  };
  near(vars['--zt-block-header']!, '#0f1526', 'block header');
  near(vars['--zt-hairline']!, '#1b2338', 'hairline');
});

test('the derived titlebar follows each theme, not the mock', () => {
  // The whole reason the surfaces are derived: the step runs in each theme's
  // own direction, so paper's titlebar is a *light* surface — the mock's dark
  // literal pasted there would paint a dark bar onto a light theme.
  for (const t of builtinThemes) {
    const bar = derivedCssVars(t.ui)['--zt-titlebar']!;
    const a = luminance(t.ui.chrome);
    const b = luminance(t.ui.panel);
    const l = luminance(bar);
    assert.ok(
      l >= Math.min(a, b) - 0.002 && l <= Math.max(a, b) + 0.002,
      `${t.id}: the titlebar (${bar}) fell outside the chrome..panel lightness band`,
    );
  }
  assert.ok(
    luminance(derivedCssVars(paper.ui)['--zt-titlebar']!) > luminance('#0d121f'),
    "paper's titlebar must be far lighter than the mock's dark hex — a literal would get this wrong",
  );
});

test('applyThemeCss sets the 24 token vars plus the 3 derived — exactly 27', () => {
  const seen = new Map<string, string>();
  applyThemeCss(obsidian.ui, { style: { setProperty: (k, v) => void seen.set(k, v) } });
  assert.equal(seen.size, 27, 'a theme switch must repaint tokens AND derived surfaces in one call');
  assert.equal(seen.get('--zt-bg'), '#0b0f1a', 'token vars still land');
  assert.equal(seen.get('--zt-titlebar'), '#0d121f', 'derived vars land beside them');
});

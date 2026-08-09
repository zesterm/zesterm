import { test } from 'node:test';
import assert from 'node:assert/strict';
import { applyCssVars, cssVarsOf, obsidian } from '../src/index.ts';

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

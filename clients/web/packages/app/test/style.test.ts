import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';

import { LAUNCHER_WIDTH } from '../src/chrome-model.ts';

const css = await readFile(new URL('../src/style.css', import.meta.url), 'utf8');

test('every colour in the stylesheet is a --zt-* token, never a literal', () => {
  // Comments may cite the mock's literals (that is provenance); only rules count.
  const rules = css.replace(/\/\*[\s\S]*?\*\//g, '');
  const literals = rules.match(/#[0-9a-fA-F]{3,8}\b|\brgba?\(|\bhsla?\(/g) ?? [];
  assert.deepEqual(
    literals,
    [],
    'design ground rule: tokens come from zest-theme — a hardcoded colour survives every theme switch unchanged',
  );
});

test('the launcher width and its measured anchor classes agree with launcherAlign', () => {
  assert.ok(
    new RegExp(String.raw`\.launcher\s*\{[^}]*width:\s*${LAUNCHER_WIDTH}px`).test(css),
    'launcherAlign decides off LAUNCHER_WIDTH — a stylesheet width that drifts from it re-opens the clipped-menu bug at a different tab count',
  );
  assert.ok(
    /\.launcher\.align-right\s*\{[^}]*right:\s*0/.test(css),
    'the predicate’s two verdicts must each have a rule to land on',
  );
  assert.ok(
    /\.launcher\.align-left\s*\{[^}]*left:\s*0/.test(css),
    'without the left-opening rule, a + near the left window edge clips its 318px menu off-viewport',
  );
});

test('the icon rail collapses the sidebar footer to its dot', () => {
  const at = css.indexOf('@media (max-width: 900px)');
  assert.notEqual(at, -1, 'the icon-rail breakpoint the sidebar collapses under must exist');
  assert.ok(
    css.slice(at).includes('.hosts-label'),
    "an '● N hosts' sentence wraps out of the 48px rail — only the dot fits (design: host dots only)",
  );
});

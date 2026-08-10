import { test } from 'node:test';
import assert from 'node:assert/strict';
import { builtinThemes, themeById, obsidian, paper } from '../src/index.ts';

test('every builtin resolves by id and ids are unique', () => {
  const ids = builtinThemes.map((t) => t.id);
  assert.deepEqual(
    ids,
    ['obsidian', 'nord', 'gum', 'classic', 'paper'],
    'builtin.rs\'s IDS list, in order — the theme picker relies on it',
  );
  for (const id of ids) {
    assert.equal(themeById(id)?.id, id, `${id} is listed but not resolvable`);
  }
  assert.equal(themeById('no-such-theme'), undefined);
});

test('obsidian matches builtin.rs value for value', () => {
  // These literals predate `cargo xtask export-web` and are kept deliberately:
  // the generator now writes builtin.generated.ts, so this is the one place
  // that still states the expected values *independently* of it. A generator
  // bug that produced self-consistent nonsense would pass check-export-web and
  // fail here. The whole record is authored in builtin.rs (obsidian is the one
  // builtin whose ui is not derived), so spot-check every token group.
  const ui = obsidian.ui;
  assert.equal(ui.bg, '#0b0f1a');
  assert.equal(ui.panel, '#121829');
  assert.equal(ui.line, '#2a3350');
  assert.equal(ui.fg, '#d7dcea');
  assert.equal(ui.dim, '#8b93a7');
  assert.equal(ui.faint, '#4a516a');
  assert.equal(ui.accent, '#6ea8ff');
  assert.equal(ui.accentSoft, '#16203a');
  assert.equal(ui.accentText, '#0b0f1a');
  assert.equal(ui.selSoft, '#161d33');
  assert.equal(ui.success, '#5fd17f');
  assert.equal(ui.warn, '#e0b341');
  assert.equal(ui.danger, '#e0606a');
  assert.equal(ui.info, '#5fc4e0');
  assert.equal(ui.magenta, '#b07cff');
  // The ansi row doubles as the theme-gallery swatch strip, in index order.
  assert.deepEqual(
    [ui.black, ui.red, ui.green, ui.yellow, ui.blue, ui.magenta, ui.cyan, ui.white],
    ['#0b0f1a', '#e0606a', '#5fd17f', '#e0b341', '#6ea8ff', '#b07cff', '#5fc4e0', '#d7dcea'],
  );
});

test('modes match builtin.rs — light is detected from the background, not the name', () => {
  assert.equal(paper.mode, 'light');
  for (const t of builtinThemes) {
    if (t.id !== 'paper') {
      assert.equal(t.mode, 'dark', `${t.id} is a dark theme in builtin.rs`);
    }
  }
});

test('every builtin theme value is an opaque #rrggbb hex', () => {
  // The palette and CSS layers both assume the serialized Rgba8 form;
  // a stray named color or rgba() here would fail far from this file.
  for (const t of builtinThemes) {
    for (const [key, value] of Object.entries(t.ui)) {
      assert.match(value, /^#[0-9a-f]{6}$/, `${t.id}.${key} = ${value}`);
    }
  }
});

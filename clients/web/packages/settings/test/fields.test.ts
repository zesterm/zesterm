import { test } from 'node:test';
import assert from 'node:assert/strict';
import {
  uiFields,
  fieldsInGroup,
  groups,
  fieldByKey,
  settingsSchema,
  SCHEMA_VERSION,
  type Widget,
} from '../src/index.ts';

const WIDGETS: readonly Widget[] = [
  'toggle',
  'number',
  'slider',
  'select',
  'theme-picker',
  'text',
  'path',
  'font-list',
  'tag-list',
  'key-value',
];

test('the generated field list is not empty', () => {
  // An empty list type-checks and renders an empty settings screen, which is
  // exactly how a broken `export-web` would look if nothing asserted it.
  assert.ok(uiFields.length > 0, 'generated/ui-fields.json produced no fields');
});

test('every field carries a widget this client knows how to render', () => {
  // The Rust `Widget` enum and the TypeScript union are hand-matched. A
  // variant added on one side is only caught here — tsc cannot see into JSON.
  for (const f of uiFields) {
    assert.ok(
      WIDGETS.includes(f.widget),
      `${f.key} asks for widget "${f.widget}", which this client cannot draw`,
    );
  }
});

test('keys are unique and dotted', () => {
  // Duplicate keys mean a form writes one value to two rows, and a key without
  // a dot is not addressable in the TOML cascade.
  const seen = new Set<string>();
  for (const f of uiFields) {
    assert.ok(!seen.has(f.key), `${f.key} appears twice`);
    seen.add(f.key);
    assert.match(f.key, /^[a-z0-9_]+(\.[a-z0-9_]+)+$/, `${f.key} is not a dotted config key`);
  }
});

test('the walk excludes the keys zest-config says are not preferences', () => {
  // `UI_EXCLUDED` in crates/zest-config/src/ui.rs: schema_version is
  // machinery, profiles is a tree with its own future form.
  for (const key of ['schema_version', 'profiles']) {
    assert.equal(fieldByKey(key), undefined, `${key} must not reach a settings form`);
  }
});

test('a range, when present, is an ordered pair', () => {
  for (const f of uiFields) {
    if (f.range === null) continue;
    const [min, max] = f.range;
    assert.ok(min <= max, `${f.key} has an inverted range [${min}, ${max}]`);
  }
});

test('slider fields carry the range they need to be drawn', () => {
  // A slider without bounds has no track. The Rust only emits `range` when the
  // schema has both minimum and maximum, so this is the assertion that catches
  // a setting annotated as a slider but never given limits.
  for (const f of uiFields.filter((f) => f.widget === 'slider')) {
    assert.notEqual(f.range, null, `${f.key} is a slider with no range`);
  }
});

test('select fields offer variants, and theme-picker deliberately does not', () => {
  for (const f of uiFields) {
    if (f.widget === 'select') {
      assert.ok(f.variants.length > 0, `${f.key} is a select with no options`);
    }
    if (f.widget === 'theme-picker') {
      // ui.rs: the crate cannot know the theme roster, so the client fills it.
      assert.equal(f.variants.length, 0, `${f.key} should leave variants to the client`);
    }
  }
});

test('grouping helpers agree with the flat list', () => {
  const names = groups();
  assert.ok(names.length > 0, 'no groups');
  assert.equal(
    names.length,
    new Set(names).size,
    'groups() must report first-appearance order without repeats',
  );
  assert.equal(
    names.reduce((n, g) => n + fieldsInGroup(g).length, 0),
    uiFields.length,
    'every field belongs to exactly one reported group',
  );
});

test('the raw schema is present and its version is readable', () => {
  assert.equal(typeof settingsSchema['$defs'], 'object', 'schema lost its $defs');
  assert.ok(Number.isInteger(SCHEMA_VERSION), 'schema_version default is not an integer');
});

/**
 * `@zesterm/settings` — the settings schema and its walked fields, generated
 * from `crates/zest-config` by `cargo xtask export-web`.
 *
 * No runtime dependencies, matching `proto`/`theme`/`input`: this package is
 * data plus the types that describe it, and a settings form is built on top
 * rather than inside.
 */

export type { Widget, UiVariant, UiField } from './fields.ts';
export { uiFields, fieldsInGroup, groups, fieldByKey } from './fields.ts';
export { settingsSchema, SCHEMA_VERSION } from './schema.ts';

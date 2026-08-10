/**
 * The settings JSON Schema itself.
 *
 * `uiFields` is the walked, render-ready view and is what a form should use.
 * The raw schema is here for the things the walk deliberately drops — the
 * `$defs` a value validator needs, and `schema_version` for migration
 * decisions — so a client that needs them does not re-derive them from a
 * second copy.
 */

import generated from '../generated/schema.json' with { type: 'json' };

/** The schema exactly as `schemars` emitted it for `zest_config::Settings`. */
export const settingsSchema: Readonly<Record<string, unknown>> = generated;

/**
 * The config format version this schema describes.
 *
 * Read from the schema's own `schema_version` default rather than restated,
 * because a restated constant is one more thing that can lag the Rust — which
 * is the failure this whole package exists to remove.
 */
export const SCHEMA_VERSION: number = ((): number => {
  const props = generated.properties as Record<string, { default?: unknown }> | undefined;
  const value = props?.['schema_version']?.default;
  if (typeof value !== 'number') {
    throw new Error(
      'generated/schema.json has no numeric schema_version default — ' +
        'run `cargo xtask export-web`',
    );
  }
  return value;
})();

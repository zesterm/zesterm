/**
 * The settings fields, as `zest-config`'s own walk produced them.
 *
 * `crates/zest-config/src/schema.rs` states the contract — *"the schema is not
 * documentation, it is what the web and phone settings UIs are generated
 * from"* — and `src/ui.rs` puts the walk outside the `fs` feature so a browser
 * can use it. This module is the browser end of that: a settings form reads
 * this list, so a new setting reaches the web client without anyone editing
 * TypeScript.
 *
 * The types below mirror `zest_config::ui`. They are hand-written on purpose,
 * unlike the data: `cargo xtask check-export-web` regenerates the JSON, and a
 * shape change then fails `tsc` here rather than at runtime in a form.
 */

import generated from '../generated/ui-fields.json' with { type: 'json' };

/**
 * How a field asks to be edited — `zest_config::ui::Widget`.
 *
 * A vocabulary, not a component list: each client maps these to whatever it
 * has, and nothing in the Rust knows how to draw one.
 */
export type Widget =
  | 'toggle'
  | 'number'
  | 'slider'
  | 'select'
  | 'theme-picker'
  | 'text'
  | 'path'
  | 'file-path'
  | 'font-list'
  | 'tag-list'
  | 'key-value'
  | 'host-picker'
  | 'scheme-picker'
  | 'accent-picker'
  | 'icon-picker';

/** One option of a `select` field. */
export interface UiVariant {
  /** The kebab-case wire value, exactly as serde writes it. */
  readonly value: string;
  /** The variant's doc comment; empty when the schema has none. */
  readonly description: string;
}

/** One settings field, as a UI should render it. */
export interface UiField {
  /** Dotted key, e.g. `typography.size_pt`. */
  readonly key: string;
  /** Display group. Grouping *order* is the client's call, not the schema's. */
  readonly group: string;
  readonly widget: Widget;
  readonly description: string;
  /** Schema `minimum`/`maximum`, when both are present. */
  readonly range: readonly [number, number] | null;
  /** Integer-typed, which decides how an edited value is written back. */
  readonly integer: boolean;
  /**
   * Empty for `theme-picker` — the Rust cannot know the theme roster, so the
   * client fills it from `@zesterm/theme`.
   */
  readonly variants: readonly UiVariant[];
  readonly default: unknown;
  /** Advisory. `invalidate::class_of` is the authoritative answer in the Rust. */
  readonly restartHint: boolean;
}

/** Every renderable settings field, in schema walk order. */
export const uiFields: readonly UiField[] = generated as readonly UiField[];

/** The fields of one group, in walk order. */
export function fieldsInGroup(group: string): readonly UiField[] {
  return uiFields.filter((f) => f.group === group);
}

/** Every group name, in first-appearance order — not alphabetical. */
export function groups(): readonly string[] {
  return [...new Set(uiFields.map((f) => f.group))];
}

/** One field by its dotted key, or `undefined`. */
export function fieldByKey(key: string): UiField | undefined {
  return uiFields.find((f) => f.key === key);
}

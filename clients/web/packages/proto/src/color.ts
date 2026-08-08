/**
 * `zest_core::Color`, hand-written because `ts-rs` does not generate it.
 *
 * `AttrDef.fg` and `AttrDef.bg` are typed `unknown` in the committed bindings —
 * `Color` lives in `zest-core`, which has no `ts` feature, so there is no
 * `Color.ts` to import. This file is the gap, and the fixtures are what keep it
 * honest: real sessions carry all three shapes, so a wrong reading fails the
 * corpus rather than lurking.
 *
 * The shapes are serde's default externally-tagged enum encoding:
 *
 * - `Color::Default`      -> the string `"Default"`
 * - `Color::Indexed(u8)`  -> `{ "Indexed": 4 }`
 * - `Color::Rgb(r, g, b)` -> `{ "Rgb": [10, 20, 30] }`
 */

export type Color =
  | 'Default'
  | { readonly Indexed: number }
  | { readonly Rgb: readonly [number, number, number] };

export const DEFAULT_COLOR: Color = 'Default';

export class ColorError extends Error {
  override name = 'ColorError';
}

/**
 * Read a colour off the wire.
 *
 * Colours stay **unresolved** — an `Indexed(4)` is carried as an index, not as
 * the RGB some host thinks index 4 means. That is what lets a desktop and a
 * phone render the same live session under different themes at the same time,
 * and it is why this returns a tagged value rather than a hex string.
 */
export function parseColor(v: unknown): Color {
  if (v === 'Default') return 'Default';

  if (typeof v === 'object' && v !== null) {
    const o = v as Record<string, unknown>;

    if ('Indexed' in o) {
      const i = o['Indexed'];
      if (typeof i === 'number' && Number.isInteger(i) && i >= 0 && i <= 255) {
        return { Indexed: i };
      }
      throw new ColorError(`Indexed colour is not a byte: ${JSON.stringify(i)}`);
    }

    if ('Rgb' in o) {
      const c = o['Rgb'];
      if (Array.isArray(c) && c.length === 3 && c.every((n) => typeof n === 'number')) {
        return { Rgb: [c[0] as number, c[1] as number, c[2] as number] };
      }
      throw new ColorError(`Rgb colour is not three numbers: ${JSON.stringify(c)}`);
    }
  }

  throw new ColorError(`not a colour: ${JSON.stringify(v)}`);
}

/** Structural equality, for comparing a decoded cell against an expectation. */
export function colorsEqual(a: Color, b: Color): boolean {
  if (a === 'Default' || b === 'Default') return a === b;
  if ('Indexed' in a) return 'Indexed' in b && a.Indexed === b.Indexed;
  if ('Indexed' in b) return false;
  return a.Rgb[0] === b.Rgb[0] && a.Rgb[1] === b.Rgb[1] && a.Rgb[2] === b.Rgb[2];
}

/** For failure messages. Not a rendering path — colours resolve against a theme. */
export function colorToString(c: Color): string {
  if (c === 'Default') return 'Default';
  if ('Indexed' in c) return `Indexed(${c.Indexed})`;
  return `Rgb(${c.Rgb[0]},${c.Rgb[1]},${c.Rgb[2]})`;
}

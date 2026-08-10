/**
 * The built-in themes — `crates/zest-theme/src/builtin.rs`, generated.
 *
 * The records themselves live in `builtin.generated.ts`, written by
 * `cargo xtask export-web` and gated by `cargo xtask check-export-web`. They
 * used to be hand-copied hex, and the transcription had no gate: drift meant
 * zesterm's native window and the browser disagreed about what `obsidian`
 * looks like, with nothing to catch it.
 *
 * What stays here is the part that is logic rather than data — a lookup the
 * generator has no business emitting five times.
 */

export {
  obsidian,
  nord,
  gum,
  classic,
  paper,
  builtinThemes,
  DEFAULT_DARK,
  DEFAULT_LIGHT,
} from './builtin.generated.ts';

import { builtinThemes } from './builtin.generated.ts';
import type { Theme } from './tokens.ts';

/** Look up a built-in by its stable id, or `undefined` — builtin.rs's `get`. */
export function themeById(id: string): Theme | undefined {
  return builtinThemes.find((t) => t.id === id);
}

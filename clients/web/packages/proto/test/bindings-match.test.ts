/**
 * The hand-written wire types must accept everything the generated ones describe.
 *
 * `cargo xtask check-bindings` stops `crates/zest-proto/bindings/` drifting from
 * the Rust. This is the other half: it stops *this package* drifting from the
 * bindings. Without it the two gates cover the Rust twice and the TypeScript not
 * at all.
 *
 * A **type-level** test, so `tsc --noEmit` is what fails — there is nothing to
 * assert at runtime, because generated types have no runtime form. The
 * `node --test` case below exists only so the file reports as a test rather than
 * silently doing nothing.
 *
 * **Direction matters.** A decoder must accept everything a host can send, so
 * the check is `Generated extends Ours`, never the reverse. Ours may be wider —
 * an `UnknownMessage` fallback — and must never be narrower.
 */

import { test } from 'node:test';

import type { AttrDef as GenAttrDef } from '@zest/bindings/AttrDef';
import type { BlockMatch as GenBlockMatch } from '@zest/bindings/BlockMatch';
import type { Color as GenColor } from '@zest/bindings/Color';
import type { HostOffer as GenHostOffer } from '@zest/bindings/HostOffer';
import type { HostProfile as GenHostProfile } from '@zest/bindings/HostProfile';
import type { CellMarks as GenCellMarks } from '@zest/bindings/CellMarks';
import type { CursorState as GenCursorState } from '@zest/bindings/CursorState';
import type { DeltaOp as GenDeltaOp } from '@zest/bindings/DeltaOp';
import type { Delta as GenDelta } from '@zest/bindings/Delta';
import type { RowPayload as GenRowPayload } from '@zest/bindings/RowPayload';
import type { Run as GenRun } from '@zest/bindings/Run';

import type { Color } from '../src/color.ts';
import type {
  AttrDef,
  BlockMatch,
  CellMarks,
  CursorState,
  Delta,
  DeltaOp,
  HostOffer,
  HostProfile,
  RowPayload,
  Run,
} from '../src/wire.ts';

/** Compiles only if `Generated` is assignable to `Ours`. */
type Accepts<Generated extends Ours, Ours> = true;

/**
 * The decoded form of a generated type.
 *
 * `Run.marks` is `marks?` in the binding — `skip_serializing_if` means a run
 * with no marks has no key at all on the wire (#15) — but the parser fills
 * every such absence with its default (`[]`, `0`, `''`), so what a consumer
 * holds is the required form. Recursively de-optionalising the generated type
 * states exactly that relationship; a field the parser leaves absent
 * (`BlockPayload.started_ms`) still checks, because required is assignable to
 * optional.
 */
type Filled<T> = T extends object ? { [K in keyof T]-?: Filled<T[K]> } : T;

// Exact, no adjustment needed.
type _Marks = Accepts<GenCellMarks, CellMarks>;
type _Cursor = Accepts<GenCursorState, CursorState>;

/**
 * `Color` is generated since #15 — `zest-core` grew a `ts` feature so
 * `AttrDef.fg`/`bg` stopped reading `unknown` — and `color.ts` remains the
 * hand-written reader the fixtures exercise. This is the line that keeps the
 * two describing the same three shapes.
 */
type _Color = Accepts<GenColor, Color>;
type _Attr = Accepts<GenAttrDef, AttrDef>;

/**
 * The host offer (#262) is exact in both directions, and it is worth saying
 * why it needs no `Filled` when `Run` does.
 *
 * Every field but `HostProfile.name` is `#[serde(default)]`, so an older peer
 * may omit any of them — but `ts-rs` types them as required, which is the
 * *decoded* shape rather than the wire one, and that is exactly what `wire.ts`
 * produces: the parser substitutes `''`, `[]` or `null` for an absent key. The
 * two agree because the parser does the work, not because the wire is strict.
 */
type _HostProfile = Accepts<GenHostProfile, HostProfile>;
type _HostOffer = Accepts<GenHostOffer, HostOffer>;

/**
 * A search hit (#527) is exact in both directions and needs no `Filled`:
 * every option is a plain `null` on the wire, never a skipped key, because
 * the message is a reply and never rides a delta.
 */
type _BlockMatch = Accepts<GenBlockMatch, BlockMatch>;

type _Run = Accepts<Filled<GenRun>, Run>;
type _Row = Accepts<Filled<GenRowPayload>, RowPayload>;

/**
 * The discriminated unions, which is where a wire change would actually land.
 *
 * `AttrId` is a `number` in the bindings and ours, so `erase.attr` lines up; the
 * `op` tag values must match exactly or this fails to compile. Since #14 the
 * integer story needs no footnote: `Seq`, `SessionId` and every line id are
 * `number` in the bindings, which is what the wire's narrowest-encoding rule
 * delivers at runtime.
 */
type _Op = Accepts<Filled<GenDeltaOp>, DeltaOp>;
type _Delta = Accepts<Filled<GenDelta>, Delta>;

test('the generated bindings are assignable to the hand-written wire types', () => {
  // Nothing to run: the assertions above are the test, and `tsc --noEmit` is
  // what evaluates them. Listed here so a missing typecheck step is visible as
  // a test that claims something it did not check.
});

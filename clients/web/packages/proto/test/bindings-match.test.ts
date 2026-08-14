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
 * `marks` optional, an `UnknownMessage` fallback — and must never be narrower.
 */

import { test } from 'node:test';

import type { AttrDef as GenAttrDef } from '@zest/bindings/AttrDef';
import type { HostOffer as GenHostOffer } from '@zest/bindings/HostOffer';
import type { HostProfile as GenHostProfile } from '@zest/bindings/HostProfile';
import type { CellMarks as GenCellMarks } from '@zest/bindings/CellMarks';
import type { CursorState as GenCursorState } from '@zest/bindings/CursorState';
import type { DeltaOp as GenDeltaOp } from '@zest/bindings/DeltaOp';
import type { Delta as GenDelta } from '@zest/bindings/Delta';
import type { RowPayload as GenRowPayload } from '@zest/bindings/RowPayload';
import type { Run as GenRun } from '@zest/bindings/Run';

import type {
  AttrDef,
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

// Exact, no exclusions needed.
type _Marks = Accepts<GenCellMarks, CellMarks>;
type _Cursor = Accepts<GenCursorState, CursorState>;

/**
 * The host offer (#262) is exact in both directions, and it is worth saying
 * why it needs no exclusion when so much else here does.
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
 * `Run.marks` is excluded, and this is the reason.
 *
 * The field carries `#[serde(skip_serializing_if = "Vec::is_empty")]`, so a run
 * with no marks has no key at all on the wire — but `ts-rs` types it as a
 * non-optional `Array<CellMarks>`. The generated file overstates what arrives.
 * Ours makes it required-after-parsing (the parser substitutes `[]`), which is
 * the honest shape for a decoded value.
 *
 * Fixed by adding `ts(optional)` beside the `skip_serializing_if` in
 * `delta.rs`; tracked as a follow-up.
 */
type _Run = Accepts<Omit<GenRun, 'marks'>, Omit<Run, 'marks'>>;

/**
 * `RowPayload.line` is `bigint` in both — this package follows the bindings.
 *
 * Worth recording that the bindings and the wire disagree here: `rmp_serde`
 * writes the narrowest integer that fits, so a line id arrives as a plain
 * MessagePack integer and reaches JavaScript as a `number`. `wire.ts` coerces at
 * one boundary so the binding's promise holds for every consumer.
 */
type _Row = Accepts<GenRowPayload, RowPayload>;

/**
 * `AttrDef.fg` and `bg` are excluded because the bindings say `unknown`.
 *
 * `zest_core::Color` has no `ts` feature and therefore no generated type, so
 * there is nothing here to check against — `color.ts` is hand-written and kept
 * honest by the fixtures instead, which carry all three of its shapes. Giving
 * `Color` a binding is a follow-up; it needs a `ts` feature on `zest-core`.
 */
type _Attr = Accepts<Omit<GenAttrDef, 'fg' | 'bg'>, Omit<AttrDef, 'fg' | 'bg'>>;

/**
 * The discriminated unions, which is where a wire change would actually land.
 *
 * `AttrId` is a `number` in the bindings and ours, so `erase.attr` lines up; the
 * `op` tag values must match exactly or this fails to compile.
 */
type _Op = Accepts<GenDeltaOp, DeltaOp>;

/**
 * `Delta` is checked in halves, because the `fg` gap is nested inside it.
 *
 * `Delta.attrs` is `AttrDef[]`, and `AttrDef.fg` is `unknown`, so a whole-type
 * check would fail for a reason that has nothing to do with `Delta` — and
 * excluding `attrs` outright would silently stop checking the field that
 * actually carries the ops. `ops` is checked here in full; `attrs` is `_Attr`
 * above, with its exclusion stated once.
 */
type _DeltaOps = Accepts<GenDelta['ops'], Delta['ops']>;
type _DeltaAttrCount = Accepts<
  Omit<GenDelta['attrs'][number], 'fg' | 'bg'>,
  Omit<Delta['attrs'][number], 'fg' | 'bg'>
>;

test('the generated bindings are assignable to the hand-written wire types', () => {
  // Nothing to run: the assertions above are the test, and `tsc --noEmit` is
  // what evaluates them. Listed here so a missing typecheck step is visible as
  // a test that claims something it did not check.
});

/**
 * The wire types, read out of decoded MessagePack.
 *
 * `HostMessage` and `ClientMessage` are `#[serde(tag = "t")]` and `DeltaOp` is
 * `#[serde(tag = "op")]`, so every one of them is a flat map with a discriminant
 * key. The committed bindings state the shapes; this module is what turns
 * `unknown` into them, checking as it goes.
 *
 * # Integers
 *
 * `Seq`, `SessionId` and `RowPayload.line` are `bigint` in the generated
 * bindings, and this file follows the bindings rather than the wire. It is worth
 * knowing that they disagree: `rmp_serde` writes the narrowest encoding that
 * fits, so a `Seq` of 3 arrives as a MessagePack fixint and reaches JavaScript
 * as a plain `number`. Coercing here means one boundary does the work and every
 * consumer sees the type the bindings promised. Tracked upstream — the honest
 * fix is in the Rust attributes, not in a workaround here.
 */

import { type Color, parseColor } from './color.ts';

export class WireError extends Error {
  override name = 'WireError';
}

// ---------------------------------------------------------------------------
// Primitives
// ---------------------------------------------------------------------------

function obj(v: unknown, what: string): Record<string, unknown> {
  if (typeof v !== 'object' || v === null || Array.isArray(v)) {
    throw new WireError(`${what}: expected a map, got ${describe(v)}`);
  }
  return v as Record<string, unknown>;
}

function arr(v: unknown, what: string): unknown[] {
  if (!Array.isArray(v)) throw new WireError(`${what}: expected an array, got ${describe(v)}`);
  return v;
}

function num(v: unknown, what: string): number {
  if (typeof v === 'number') return v;
  if (typeof v === 'bigint') {
    throw new WireError(`${what}: ${v} does not fit in a JavaScript number`);
  }
  throw new WireError(`${what}: expected a number, got ${describe(v)}`);
}

function str(v: unknown, what: string): string {
  if (typeof v !== 'string') throw new WireError(`${what}: expected a string, got ${describe(v)}`);
  return v;
}

function bool(v: unknown, what: string): boolean {
  if (typeof v !== 'boolean') throw new WireError(`${what}: expected a bool, got ${describe(v)}`);
  return v;
}

/** A field the bindings type as `bigint`, whatever width the wire used. */
function big(v: unknown, what: string): bigint {
  if (typeof v === 'bigint') return v;
  if (typeof v === 'number' && Number.isInteger(v)) return BigInt(v);
  throw new WireError(`${what}: expected an integer, got ${describe(v)}`);
}

function describe(v: unknown): string {
  if (v === null) return 'null';
  if (Array.isArray(v)) return `an array of ${v.length}`;
  return typeof v;
}

// ---------------------------------------------------------------------------
// Delta types
// ---------------------------------------------------------------------------

export interface AttrDef {
  readonly id: number;
  readonly fg: Color;
  readonly bg: Color;
  readonly flags: number;
}

export interface CellMarks {
  /** Offset of the cell **within its run**, in cells — not within the row. */
  readonly at: number;
  readonly marks: string;
}

export interface Run {
  readonly attr: number;
  /**
   * How many cells this run occupies. **The host's decision, never recomputed.**
   * A double-width character arrives as two runs — one carrying the character,
   * one carrying none — so `cells` and the character count differ on purpose.
   */
  readonly cells: number;
  readonly text: string;
  readonly marks: readonly CellMarks[];
}

export interface RowPayload {
  readonly line: bigint;
  readonly runs: readonly Run[];
  readonly wrapped: boolean;
}

export interface CursorState {
  readonly row: number;
  readonly col: number;
  readonly visible: boolean;
  readonly shape: number;
}

export type DeltaOp =
  | { readonly op: 'scroll'; readonly top: number; readonly bottom: number; readonly lines: number }
  | { readonly op: 'row'; readonly row: number; readonly payload: RowPayload }
  | {
      readonly op: 'erase';
      readonly top: number;
      readonly left: number;
      readonly bottom: number;
      readonly right: number;
      readonly attr: number;
    }
  | { readonly op: 'cursor'; readonly cursor: CursorState }
  | { readonly op: 'sb_push'; readonly payload: RowPayload }
  | { readonly op: 'alt_screen'; readonly active: boolean }
  | { readonly op: 'title'; readonly title: string }
  | { readonly op: 'modes'; readonly bits: number };

export interface Delta {
  readonly attrs: readonly AttrDef[];
  readonly ops: readonly DeltaOp[];
}

export function parseAttrDef(v: unknown): AttrDef {
  const o = obj(v, 'AttrDef');
  return {
    id: num(o['id'], 'AttrDef.id'),
    fg: parseColor(o['fg']),
    bg: parseColor(o['bg']),
    flags: num(o['flags'], 'AttrDef.flags'),
  };
}

export function parseRun(v: unknown): Run {
  const o = obj(v, 'Run');
  // `marks` carries `skip_serializing_if = "Vec::is_empty"`, so a run without
  // any simply has no key. The binding types it non-optional, which is a
  // generated file overstating the wire.
  const marks = o['marks'] === undefined ? [] : arr(o['marks'], 'Run.marks').map(parseCellMarks);
  return {
    attr: num(o['attr'], 'Run.attr'),
    cells: num(o['cells'], 'Run.cells'),
    text: str(o['text'], 'Run.text'),
    marks,
  };
}

function parseCellMarks(v: unknown): CellMarks {
  const o = obj(v, 'CellMarks');
  return { at: num(o['at'], 'CellMarks.at'), marks: str(o['marks'], 'CellMarks.marks') };
}

export function parseRowPayload(v: unknown): RowPayload {
  const o = obj(v, 'RowPayload');
  return {
    line: big(o['line'], 'RowPayload.line'),
    runs: arr(o['runs'], 'RowPayload.runs').map(parseRun),
    wrapped: bool(o['wrapped'], 'RowPayload.wrapped'),
  };
}

export function parseCursorState(v: unknown): CursorState {
  const o = obj(v, 'CursorState');
  return {
    row: num(o['row'], 'CursorState.row'),
    col: num(o['col'], 'CursorState.col'),
    visible: bool(o['visible'], 'CursorState.visible'),
    shape: num(o['shape'], 'CursorState.shape'),
  };
}

export function parseDeltaOp(v: unknown): DeltaOp {
  const o = obj(v, 'DeltaOp');
  const op = str(o['op'], 'DeltaOp.op');
  switch (op) {
    case 'scroll':
      return {
        op,
        top: num(o['top'], 'scroll.top'),
        bottom: num(o['bottom'], 'scroll.bottom'),
        lines: num(o['lines'], 'scroll.lines'),
      };
    case 'row':
      return { op, row: num(o['row'], 'row.row'), payload: parseRowPayload(o['payload']) };
    case 'erase':
      return {
        op,
        top: num(o['top'], 'erase.top'),
        left: num(o['left'], 'erase.left'),
        bottom: num(o['bottom'], 'erase.bottom'),
        right: num(o['right'], 'erase.right'),
        attr: num(o['attr'], 'erase.attr'),
      };
    case 'cursor':
      return { op, cursor: parseCursorState(o['cursor']) };
    case 'sb_push':
      return { op, payload: parseRowPayload(o['payload']) };
    case 'alt_screen':
      return { op, active: bool(o['active'], 'alt_screen.active') };
    case 'title':
      return { op, title: str(o['title'], 'title.title') };
    case 'modes':
      return { op, bits: num(o['bits'], 'modes.bits') };
    default:
      // Unlike an unknown message, an unknown op cannot be skipped: it changes
      // the grid, and carrying on would leave this client silently wrong about
      // what the host shows. Refuse, and let the caller ask for a keyframe.
      throw new WireError(`unknown delta op ${JSON.stringify(op)}`);
  }
}

export function parseDelta(v: unknown): Delta {
  const o = obj(v, 'Delta');
  return {
    attrs: arr(o['attrs'], 'Delta.attrs').map(parseAttrDef),
    ops: arr(o['ops'], 'Delta.ops').map(parseDeltaOp),
  };
}

// ---------------------------------------------------------------------------
// Host messages
// ---------------------------------------------------------------------------

export interface SessionAddr {
  readonly host: string;
  readonly session: bigint;
}

export interface Keyframe {
  readonly t: 'keyframe';
  readonly session: SessionAddr;
  readonly seq: bigint;
  readonly cols: number;
  readonly rows: number;
  readonly rows_data: readonly RowPayload[];
  readonly attrs: readonly AttrDef[];
  readonly cursor: CursorState;
  readonly modes: number;
}

export interface Update {
  readonly t: 'update';
  readonly session: SessionAddr;
  readonly base: bigint;
  readonly seq: bigint;
  readonly delta: Delta;
}

/**
 * A message this client does not model, kept rather than thrown.
 *
 * The daemon may be newer, and `zest-proto`'s own test
 * `an_unknown_field_does_not_break_an_older_peer` establishes that direction as
 * supported. A session-list message a decoder does not understand is not a
 * reason to drop the connection carrying the grid.
 */
export interface UnknownMessage {
  readonly t: string;
  readonly raw: Record<string, unknown>;
}

export type HostMessage = Keyframe | Update | UnknownMessage;

/**
 * Narrowing helpers.
 *
 * `UnknownMessage.t` is a bare `string`, so `msg.t === 'keyframe'` cannot rule
 * it out — a `string` might be `'keyframe'` as far as the type system knows.
 * Testing for the `raw` field is what actually separates a message this client
 * models from one it merely carried.
 */
export function isKeyframe(m: HostMessage): m is Keyframe {
  return m.t === 'keyframe' && !('raw' in m);
}

export function isUpdate(m: HostMessage): m is Update {
  return m.t === 'update' && !('raw' in m);
}

export function parseSessionAddr(v: unknown): SessionAddr {
  const o = obj(v, 'SessionAddr');
  return { host: str(o['host'], 'SessionAddr.host'), session: big(o['session'], 'SessionAddr.session') };
}

/** Decode one frame body into a host message. */
export function parseHostMessage(v: unknown): HostMessage {
  const o = obj(v, 'HostMessage');
  const t = str(o['t'], 'HostMessage.t');

  switch (t) {
    case 'keyframe':
      return {
        t,
        session: parseSessionAddr(o['session']),
        seq: big(o['seq'], 'keyframe.seq'),
        cols: num(o['cols'], 'keyframe.cols'),
        rows: num(o['rows'], 'keyframe.rows'),
        rows_data: arr(o['rows_data'], 'keyframe.rows_data').map(parseRowPayload),
        attrs: arr(o['attrs'], 'keyframe.attrs').map(parseAttrDef),
        cursor: parseCursorState(o['cursor']),
        // `#[serde(default)]`, so a host that predates it sends no key.
        modes: o['modes'] === undefined ? 0 : num(o['modes'], 'keyframe.modes'),
      };
    case 'update':
      return {
        t,
        session: parseSessionAddr(o['session']),
        base: big(o['base'], 'update.base'),
        seq: big(o['seq'], 'update.seq'),
        delta: parseDelta(o['delta']),
      };
    default:
      return { t, raw: o };
  }
}

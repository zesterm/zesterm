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

/**
 * How a command ended. Internally tagged on `state`.
 *
 * `exit_code: null` is **not zero**: a shell that emits OSC 133 D without the
 * status parameter is common, and rendering a green tick for a command that
 * actually failed is worse than rendering nothing. The mockup's rule — "the UI
 * must never compute or cache a block's outcome locally" — starts here.
 */
export type BlockState =
  | { readonly state: 'prompt' }
  | { readonly state: 'running' }
  | { readonly state: 'finished'; readonly exit_code: number | null };

/**
 * One command and its output, as the host has it.
 *
 * Line ids are `bigint` to match `RowPayload.line` — a client compares the two
 * to decide which rows a block covers, and two integer types that must agree
 * is a conversion bug waiting for a session long enough to matter.
 * `end_line: null` means the command is still producing output and must render
 * to the bottom rather than waiting.
 */
export interface BlockPayload {
  /** Stable for the life of the session; the key an upsert replaces on. */
  readonly id: number;
  readonly prompt_line: bigint;
  readonly output_line: bigint | null;
  readonly end_line: bigint | null;
  readonly state: BlockState;
  readonly command: string;
  readonly cwd: string;
  /**
   * Wall clock at OSC 133;C / D, epoch milliseconds. Plain `number`, safely:
   * epoch millis stay under 2^53 for the next 285,000 years. Absent from a
   * host that predates them.
   */
  readonly started_ms?: number;
  readonly ended_ms?: number;
}

export interface Delta {
  readonly attrs: readonly AttrDef[];
  readonly ops: readonly DeltaOp[];
  /**
   * Keyed and order-independent, unlike `ops` — and applied **after** them,
   * because a block names absolute line ids the rows in the same batch
   * establish. A field rather than a `DeltaOp` variant on purpose: `DeltaOp`
   * is tagged, an unknown tag fails the whole message on an older peer, and a
   * defaulted field degrades to "no blocks" instead.
   */
  readonly blocks: readonly BlockPayload[];
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

function parseBlockState(v: unknown): BlockState {
  const o = obj(v, 'BlockState');
  const state = str(o['state'], 'BlockState.state');
  switch (state) {
    case 'prompt':
    case 'running':
      return { state };
    case 'finished': {
      const code = o['exit_code'];
      return { state, exit_code: code === null ? null : num(code, 'finished.exit_code') };
    }
    default:
      // Like a delta op and unlike a message: a state this client cannot read
      // is one it would render wrongly, and "wrongly" here means lying about
      // whether a command succeeded.
      throw new WireError(`unknown block state ${JSON.stringify(state)}`);
  }
}

export function parseBlockPayload(v: unknown): BlockPayload {
  const o = obj(v, 'BlockPayload');
  const optLine = (key: string): bigint | null =>
    o[key] === null || o[key] === undefined ? null : big(o[key], `BlockPayload.${key}`);
  const optMs = (key: string): number | undefined =>
    o[key] === null || o[key] === undefined ? undefined : num(o[key], `BlockPayload.${key}`);
  const payload: BlockPayload = {
    id: num(o['id'], 'BlockPayload.id'),
    prompt_line: big(o['prompt_line'], 'BlockPayload.prompt_line'),
    output_line: optLine('output_line'),
    end_line: optLine('end_line'),
    state: parseBlockState(o['state']),
    command: str(o['command'], 'BlockPayload.command'),
    cwd: str(o['cwd'], 'BlockPayload.cwd'),
  };
  // `exactOptionalPropertyTypes`: absent means absent, not `undefined`-valued.
  const started = optMs('started_ms');
  const ended = optMs('ended_ms');
  return {
    ...payload,
    ...(started === undefined ? {} : { started_ms: started }),
    ...(ended === undefined ? {} : { ended_ms: ended }),
  };
}

export function parseDelta(v: unknown): Delta {
  const o = obj(v, 'Delta');
  return {
    attrs: arr(o['attrs'], 'Delta.attrs').map(parseAttrDef),
    ops: arr(o['ops'], 'Delta.ops').map(parseDeltaOp),
    // `skip_serializing_if = "Vec::is_empty"`: no blocks means no key.
    blocks:
      o['blocks'] === undefined ? [] : arr(o['blocks'], 'Delta.blocks').map(parseBlockPayload),
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
  /** Every block the host holds — a keyframe replaces, a delta upserts. */
  readonly blocks: readonly BlockPayload[];
  /** The session's title at this instant; `''` from a host that predates it. */
  readonly title: string;
}

export interface Update {
  readonly t: 'update';
  readonly session: SessionAddr;
  readonly base: bigint;
  readonly seq: bigint;
  readonly delta: Delta;
}

export interface Welcome {
  readonly t: 'welcome';
  readonly version: number;
  /** 64 lowercase hex characters — the host's public key. */
  readonly host: string;
  readonly label: string;
}

export interface Challenge {
  readonly t: 'challenge';
  readonly version: number;
  readonly host: string;
  readonly label: string;
  /** 64 hex characters; all zeroes reads as absent and must be refused. */
  readonly nonce: string;
  /** 128 hex characters — the host signing first is what lets a client pin. */
  readonly signature: string;
}

export interface AuthPending {
  readonly t: 'auth_pending';
  /** The six digits to show beside the host's approval prompt. */
  readonly code: string;
  readonly expires_in_secs: number;
}

export interface AuthFailed {
  readonly t: 'auth_failed';
  /** A snake_case name from `AuthFailure`; `'denied'` must never be retried. */
  readonly reason: string;
  readonly message: string;
}

export interface PairingRequested {
  readonly t: 'pairing_requested';
  readonly client: string;
  readonly label: string;
  readonly code: string;
  readonly remote: string;
}

export interface SessionInfo {
  readonly addr: SessionAddr;
  readonly title: string;
  readonly cwd: string;
  readonly cols: number;
  readonly rows: number;
  /** What the phone's blocks-first view switches on. */
  readonly alt_screen: boolean;
  readonly attached: boolean;
}

export interface Sessions {
  readonly t: 'sessions';
  readonly sessions: readonly SessionInfo[];
  /** Set when this listing answers this connection's own `create_session`. */
  readonly created: bigint | null;
}

export interface Scrollback {
  readonly t: 'scrollback';
  readonly session: SessionAddr;
  readonly from_line: bigint;
  readonly rows_data: readonly RowPayload[];
  readonly attrs: readonly AttrDef[];
}

export interface Exited {
  readonly t: 'exited';
  readonly session: SessionAddr;
  readonly code: number | null;
}

export interface ErrorMessage {
  readonly t: 'error';
  readonly session: SessionAddr | null;
  readonly message: string;
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

export type HostMessage =
  | Keyframe
  | Update
  | Welcome
  | Challenge
  | AuthPending
  | AuthFailed
  | PairingRequested
  | Sessions
  | Scrollback
  | Exited
  | ErrorMessage
  | UnknownMessage;

/**
 * Narrowing helpers.
 *
 * `UnknownMessage.t` is a bare `string`, so `msg.t === 'keyframe'` cannot rule
 * it out — a `string` might be `'keyframe'` as far as the type system knows.
 * Testing for the `raw` field is what actually separates a message this client
 * models from one it merely carried.
 */
function modeled<T extends HostMessage & { t: string }>(tag: T['t']) {
  return (m: HostMessage): m is T => m.t === tag && !('raw' in m);
}

export const isKeyframe = modeled<Keyframe>('keyframe');
export const isUpdate = modeled<Update>('update');
export const isWelcome = modeled<Welcome>('welcome');
export const isChallenge = modeled<Challenge>('challenge');
export const isAuthPending = modeled<AuthPending>('auth_pending');
export const isAuthFailed = modeled<AuthFailed>('auth_failed');
export const isPairingRequested = modeled<PairingRequested>('pairing_requested');
export const isSessions = modeled<Sessions>('sessions');
export const isScrollback = modeled<Scrollback>('scrollback');
export const isExited = modeled<Exited>('exited');
export const isErrorMessage = modeled<ErrorMessage>('error');

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
        // Likewise additive; empty travels as absent.
        blocks:
          o['blocks'] === undefined
            ? []
            : arr(o['blocks'], 'keyframe.blocks').map(parseBlockPayload),
        title: o['title'] === undefined ? '' : str(o['title'], 'keyframe.title'),
      };
    case 'update':
      return {
        t,
        session: parseSessionAddr(o['session']),
        base: big(o['base'], 'update.base'),
        seq: big(o['seq'], 'update.seq'),
        delta: parseDelta(o['delta']),
      };
    case 'welcome':
      return {
        t,
        version: num(o['version'], 'welcome.version'),
        host: str(o['host'], 'welcome.host'),
        label: str(o['label'], 'welcome.label'),
      };
    case 'challenge':
      return {
        t,
        version: num(o['version'], 'challenge.version'),
        host: str(o['host'], 'challenge.host'),
        label: str(o['label'], 'challenge.label'),
        nonce: str(o['nonce'], 'challenge.nonce'),
        signature: str(o['signature'], 'challenge.signature'),
      };
    case 'auth_pending':
      return {
        t,
        code: str(o['code'], 'auth_pending.code'),
        expires_in_secs: num(o['expires_in_secs'], 'auth_pending.expires_in_secs'),
      };
    case 'auth_failed':
      return {
        t,
        reason: str(o['reason'], 'auth_failed.reason'),
        message: str(o['message'], 'auth_failed.message'),
      };
    case 'pairing_requested':
      return {
        t,
        client: str(o['client'], 'pairing_requested.client'),
        label: str(o['label'], 'pairing_requested.label'),
        code: str(o['code'], 'pairing_requested.code'),
        remote: str(o['remote'], 'pairing_requested.remote'),
      };
    case 'sessions':
      return {
        t,
        sessions: arr(o['sessions'], 'sessions.sessions').map(parseSessionInfo),
        // `skip_serializing_if = "Option::is_none"`: absent unless this
        // listing answers the receiver's own create.
        created: o['created'] === undefined || o['created'] === null
          ? null
          : big(o['created'], 'sessions.created'),
      };
    case 'scrollback':
      return {
        t,
        session: parseSessionAddr(o['session']),
        from_line: big(o['from_line'], 'scrollback.from_line'),
        rows_data: arr(o['rows_data'], 'scrollback.rows_data').map(parseRowPayload),
        attrs: o['attrs'] === undefined ? [] : arr(o['attrs'], 'scrollback.attrs').map(parseAttrDef),
      };
    case 'exited':
      return {
        t,
        session: parseSessionAddr(o['session']),
        code: o['code'] === null || o['code'] === undefined ? null : num(o['code'], 'exited.code'),
      };
    case 'error':
      return {
        t,
        session:
          o['session'] === null || o['session'] === undefined
            ? null
            : parseSessionAddr(o['session']),
        message: str(o['message'], 'error.message'),
      };
    default:
      return { t, raw: o };
  }
}

export function parseSessionInfo(v: unknown): SessionInfo {
  const o = obj(v, 'SessionInfo');
  return {
    addr: parseSessionAddr(o['addr']),
    title: str(o['title'], 'SessionInfo.title'),
    cwd: str(o['cwd'], 'SessionInfo.cwd'),
    cols: num(o['cols'], 'SessionInfo.cols'),
    rows: num(o['rows'], 'SessionInfo.rows'),
    alt_screen: bool(o['alt_screen'], 'SessionInfo.alt_screen'),
    attached: bool(o['attached'], 'SessionInfo.attached'),
  };
}

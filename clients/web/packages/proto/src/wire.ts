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
 * `Seq`, `SessionId` and the line ids are all `number`, matching the bindings
 * and the wire alike (#14): `rmp_serde` writes the narrowest encoding that
 * fits, so a `Seq` of 3 arrives as a MessagePack fixint and reaches JavaScript
 * as a plain `number` — and no realistic session pushes any of them past 2^53.
 * The one deliberate exception is a line id: the encoder pads blank rows with
 * `i64::MIN`, which MessagePack delivers as a bigint and `lineNum` converts,
 * exactly, because it is a power of two. Anything else beyond ±2^53 is refused
 * loudly rather than rounded — a silently imprecise id would file rows into
 * the wrong block, which is worse than no frame at all.
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

/**
 * A line id: a `number`, however wide the wire's encoding was.
 *
 * The encoder pads blank rows with `i64::MIN` — the one id outside ±2^53 a
 * host actually sends — which the MessagePack layer faithfully hands over as a
 * bigint. It is a power of two, so `Number` holds it exactly; any bigint that
 * does not survive the round trip is refused rather than rounded, because an
 * id off by one files rows into the wrong block.
 */
function lineNum(v: unknown, what: string): number {
  if (typeof v === 'number') return v;
  if (typeof v === 'bigint' && BigInt(Number(v)) === v) return Number(v);
  if (typeof v === 'bigint') {
    throw new WireError(`${what}: ${v} does not fit a JavaScript number exactly`);
  }
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
  readonly line: number;
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
 * Line ids are `number` to match `RowPayload.line` — a client compares the two
 * to decide which rows a block covers, and two integer types that must agree
 * is a conversion bug waiting to happen.
 * `end_line: null` means the command is still producing output and must render
 * to the bottom rather than waiting.
 */
export interface BlockPayload {
  /** Stable for the life of the session; the key an upsert replaces on. */
  readonly id: number;
  readonly prompt_line: number;
  readonly output_line: number | null;
  readonly end_line: number | null;
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
  /**
   * Where the command ran — branch, venv, cluster — as of its start (#429).
   * Empty strings mean unsaid; absent means the host never stamped one.
   * Display only, like every context fact.
   */
  readonly context?: BlockContextPayload;
  /**
   * The client the daemon saw write the input that started this command,
   * as 64 lowercase hex characters. Absent when it recorded nobody.
   *
   * Unlike `command` and the exit status this is not the shell's word —
   * nothing running in the terminal can change whose id a block carries. It
   * is still not authorization: OSC 133 decides *when* a block opens, so a
   * block nobody typed carries whoever wrote last.
   */
  readonly author?: string;
}

export interface BlockContextPayload {
  readonly branch: string;
  readonly venv: string;
  readonly kube: string;
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
  // any simply has no key — `marks?` in the binding since #15. Filled here, so
  // consumers hold the required form.
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
    line: lineNum(o['line'], 'RowPayload.line'),
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

function parseBlockContext(v: unknown): BlockContextPayload {
  const o = obj(v, 'BlockContextPayload');
  const opt = (key: string): string =>
    o[key] === undefined ? '' : str(o[key], `BlockContextPayload.${key}`);
  return { branch: opt('branch'), venv: opt('venv'), kube: opt('kube') };
}

export function parseBlockPayload(v: unknown): BlockPayload {
  const o = obj(v, 'BlockPayload');
  const optLine = (key: string): number | null =>
    o[key] === null || o[key] === undefined ? null : lineNum(o[key], `BlockPayload.${key}`);
  const optMs = (key: string): number | undefined =>
    o[key] === null || o[key] === undefined ? undefined : num(o[key], `BlockPayload.${key}`);
  const payload: BlockPayload = {
    id: num(o['id'], 'BlockPayload.id'),
    prompt_line: lineNum(o['prompt_line'], 'BlockPayload.prompt_line'),
    output_line: optLine('output_line'),
    end_line: optLine('end_line'),
    state: parseBlockState(o['state']),
    command: str(o['command'], 'BlockPayload.command'),
    cwd: str(o['cwd'], 'BlockPayload.cwd'),
  };
  // `exactOptionalPropertyTypes`: absent means absent, not `undefined`-valued.
  const started = optMs('started_ms');
  const ended = optMs('ended_ms');
  const context =
    o['context'] === undefined || o['context'] === null
      ? undefined
      : parseBlockContext(o['context']);
  const author =
    o['author'] === undefined || o['author'] === null
      ? undefined
      : str(o['author'], 'BlockPayload.author');
  return {
    ...payload,
    ...(started === undefined ? {} : { started_ms: started }),
    ...(ended === undefined ? {} : { ended_ms: ended }),
    ...(context === undefined ? {} : { context }),
    ...(author === undefined ? {} : { author }),
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
  readonly session: number;
}

export interface Keyframe {
  readonly t: 'keyframe';
  readonly session: SessionAddr;
  readonly seq: number;
  readonly cols: number;
  readonly rows: number;
  readonly rows_data: readonly RowPayload[];
  readonly attrs: readonly AttrDef[];
  readonly cursor: CursorState;
  readonly modes: number;
  /** Every block the host holds — a keyframe replaces, a delta upserts. */
  readonly blocks: readonly BlockPayload[];
  /**
   * The id from which `blocks` is authoritative.
   *
   * Rises past what scrollback eviction took, which a client keeping longer
   * history is entitled to keep; falls to what a screen clear destroyed, which
   * it must drop. `0` from a host that predates it, which replaces wholesale —
   * what this client already did.
   */
  readonly blocks_from: number;
  /** The session's title at this instant; `''` from a host that predates it. */
  readonly title: string;
  /**
   * How many times ED 3 has destroyed the host's scrollback. Monotonic; a
   * replica shadows it and drops its own history when it advances, so a `cls`
   * reaches a client that missed the moment. `0` from a host that predates
   * it, which never advances the shadow — such a host simply never announces
   * a clear. (#314)
   */
  readonly history_clears: number;
}

export interface Update {
  readonly t: 'update';
  readonly session: SessionAddr;
  readonly base: number;
  readonly seq: number;
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
  /**
   * 64 hex characters — the host's ephemeral X25519 public key.
   *
   * **This message is the switch.** Everything after a challenge is sealed, in
   * both directions; everything up to and including it is plaintext. Positional
   * rather than per-message-type, because a table of which variants are
   * encrypted is a table two implementations can disagree about.
   */
  readonly dh: string;
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
  /**
   * Seconds the code stays comparable, mirroring `auth_pending`. `0` from a
   * daemon that predates the field — unknown, not already-expired.
   */
  readonly expires_in_secs: number;
  /**
   * `true` is the tombstone: the request left the queue (answered, expired,
   * or the device hung up), so close whatever prompt showed it. A marker
   * rather than a second message so an older peer never meets an unknown
   * tag; a tombstone carries only `client`.
   */
  readonly resolved: boolean;
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
  /**
   * What the session is standing in — branch, kube context, pins — computed
   * by the daemon that owns it. `null` from a daemon that predates the field:
   * "did not say", never "no context".
   */
  readonly context: SessionContext | null;
  /** A command is running right now, by the tail block's word. */
  readonly busy: boolean;
}

/**
 * Where a session stands, as its daemon sees it. Display only — half of it
 * is the filesystem as of a debounce window ago, half is shell-emitted
 * escapes anything that can print can forge; each fact's `source` says which.
 */
export interface SessionContext {
  readonly git: GitContext | null;
  readonly facts: readonly ContextFact[];
  /** Moves when the context does, so chrome can skip unchanged rebuilds. */
  readonly revision: number;
}

export interface GitContext {
  /** The branch name, or the short hash when detached. */
  readonly branch: string;
  readonly detached: boolean;
  /** `null` until something actually asks git — unknown, not clean. */
  readonly dirty: boolean | null;
  /**
   * How many files say so, when dirty (#432). `null` when clean or unknown
   * — a `0` beside `dirty: true` would be two fields disagreeing in public.
   */
  readonly changed: number | null;
}

export interface ContextFact {
  /** `venv`, `kube`, `node`, `ssh_host`, … — an unknown key is one chip fewer. */
  readonly key: string;
  readonly value: string;
  readonly source: ContextSource;
}

/** Who said so, and therefore how far it can be trusted. */
export type ContextSource = 'daemon_probe' | 'shell_report';

/** One launch target a machine publishes (#262). */
export interface HostProfile {
  readonly name: string;
  /**
   * Already folded through *that host's* `profiles.defaults`. Empty means the
   * far host's own default shell — the same convention `create_session.command`
   * uses, so a launch passes it straight through.
   */
  readonly command: string;
  readonly starting_directory: string;
  readonly icon: string;
  readonly color_scheme: string;
  readonly tab_color: number | null;
}

/**
 * What a machine can offer: what it *is*, and what it can launch (#262).
 *
 * Facts, not a capability matrix — nothing here is matched on to decide
 * whether a feature exists. An empty string means "unknown", and the row is
 * simply not drawn rather than shown as a dash.
 */
export interface HostOffer {
  readonly os: string;
  readonly arch: string;
  readonly os_version: string;
  readonly default_shell: string;
  readonly profiles: readonly HostProfile[];
}

export interface Sessions {
  readonly t: 'sessions';
  readonly sessions: readonly SessionInfo[];
  /** Set when this listing answers this connection's own `create_session`. */
  readonly created: number | null;
  /**
   * What this machine offers, when there is something new to say (#262).
   *
   * `null` covers three cases on purpose, and the client wants one branch for
   * all of them: this connection did not set `watch_hosts`, the host publishes
   * nothing, or nothing has changed since the last message. A daemon that
   * predates the field lands here too. Sticky on the reader's side — an
   * ordinary session push carries none, and clearing on one would blank a
   * launcher's rows every time somebody opened a shell.
   */
  readonly offer: HostOffer | null;
}

export interface Scrollback {
  readonly t: 'scrollback';
  readonly session: SessionAddr;
  readonly from_line: number;
  readonly rows_data: readonly RowPayload[];
  readonly attrs: readonly AttrDef[];
}

export interface Exited {
  readonly t: 'exited';
  readonly session: SessionAddr;
  readonly code: number | null;
}

/**
 * Why a session is asking to be noticed. Mirrors `AttentionCause` in the
 * generated bindings.
 */
export type AttentionCause = 'bell' | 'notify';

/**
 * The session asked to be noticed: `BEL`, `OSC 9`, or `OSC 777;notify` (#383).
 *
 * Only ever arrives on a connection that set `watch_signals`. There is no
 * matching "unread" bit on the host: with two devices watching one shell there
 * is no answer to who should clear one, so the host reports the moment and
 * every viewer keeps its own idea of what it has seen.
 */
export interface Attention {
  readonly t: 'attention';
  readonly session: SessionAddr;
  readonly cause: AttentionCause;
}

/** The flavour of a determinate {@link Progress}. */
export type ProgressState = 'normal' | 'error' | 'warning';

/** How a long job is going, as `OSC 9;4` reports it. */
export type Progress =
  | { readonly kind: 'none' }
  | { readonly kind: 'at'; readonly percent: number; readonly state: ProgressState }
  | { readonly kind: 'indeterminate' };

/**
 * A long job in this session reported how it is going (#385).
 *
 * Behind `watch_signals` like {@link Attention}, and a host message for the
 * same reason: it describes the session, not the grid.
 *
 * **Unlike attention this is state**, and that is why they are two messages
 * despite arriving through the same escape sequence. The host keeps a
 * per-subscriber shadow of what it last sent here — so attaching halfway
 * through a build tells you the bar is at 60% — and keeps no memory at all of
 * a bell.
 */
export interface ProgressMessage {
  readonly t: 'progress';
  readonly session: SessionAddr;
  readonly progress: Progress;
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
  | Attention
  | ProgressMessage
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
export const isAttention = modeled<Attention>('attention');
export const isProgress = modeled<ProgressMessage>('progress');
export const isErrorMessage = modeled<ErrorMessage>('error');

/**
 * An unknown cause reads as `bell` rather than throwing.
 *
 * A newer host that has learned a third way to ask for attention should leave
 * this client slightly less informative, not unable to decode the frame — the
 * same rule `AttrDef.flags` and `DeltaOp::Modes` already follow. The dot is
 * the same dot either way; only the word for it would be wrong.
 */
export function parseAttentionCause(v: unknown): AttentionCause {
  return v === 'notify' ? 'notify' : 'bell';
}

/**
 * An unreadable progress report reads as `none` rather than throwing.
 *
 * The same rule `parseAttentionCause` follows, and here it is also what keeps
 * a spinner from turning forever: this is *latched* state, so "ignore what you
 * cannot read" would leave the last good value on screen indefinitely.
 */
export function parseProgress(v: unknown): Progress {
  if (typeof v !== 'object' || v === null) return { kind: 'none' };
  const o = v as Record<string, unknown>;
  if (o['kind'] === 'indeterminate') return { kind: 'indeterminate' };
  if (o['kind'] !== 'at') return { kind: 'none' };
  const raw = typeof o['percent'] === 'number' ? o['percent'] : 0;
  const percent = Math.max(0, Math.min(100, Math.trunc(raw)));
  const state = o['state'];
  return {
    kind: 'at',
    percent,
    state: state === 'error' || state === 'warning' ? state : 'normal',
  };
}

export function parseSessionAddr(v: unknown): SessionAddr {
  const o = obj(v, 'SessionAddr');
  return { host: str(o['host'], 'SessionAddr.host'), session: num(o['session'], 'SessionAddr.session') };
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
        seq: num(o['seq'], 'keyframe.seq'),
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
        blocks_from:
          o['blocks_from'] === undefined ? 0 : num(o['blocks_from'], 'keyframe.blocks_from'),
        title: o['title'] === undefined ? '' : str(o['title'], 'keyframe.title'),
        history_clears:
          o['history_clears'] === undefined
            ? 0
            : num(o['history_clears'], 'keyframe.history_clears'),
      };
    case 'update':
      return {
        t,
        session: parseSessionAddr(o['session']),
        base: num(o['base'], 'update.base'),
        seq: num(o['seq'], 'update.seq'),
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
        dh: str(o['dh'], 'challenge.dh'),
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
        // `#[serde(default)]` on the host: absent from an older daemon.
        expires_in_secs:
          o['expires_in_secs'] === undefined
            ? 0
            : num(o['expires_in_secs'], 'pairing_requested.expires_in_secs'),
        resolved:
          o['resolved'] === undefined ? false : bool(o['resolved'], 'pairing_requested.resolved'),
      };
    case 'sessions':
      return {
        t,
        sessions: arr(o['sessions'], 'sessions.sessions').map(parseSessionInfo),
        // `skip_serializing_if = "Option::is_none"`: absent unless this
        // listing answers the receiver's own create.
        created: o['created'] === undefined || o['created'] === null
          ? null
          : num(o['created'], 'sessions.created'),
        // Same `skip_serializing_if` shape, and absent is the common case:
        // every ordinary session push omits it.
        offer:
          o['offer'] === undefined || o['offer'] === null ? null : parseHostOffer(o['offer']),
      };
    case 'scrollback':
      return {
        t,
        session: parseSessionAddr(o['session']),
        from_line: lineNum(o['from_line'], 'scrollback.from_line'),
        rows_data: arr(o['rows_data'], 'scrollback.rows_data').map(parseRowPayload),
        attrs: o['attrs'] === undefined ? [] : arr(o['attrs'], 'scrollback.attrs').map(parseAttrDef),
      };
    case 'exited':
      return {
        t,
        session: parseSessionAddr(o['session']),
        code: o['code'] === null || o['code'] === undefined ? null : num(o['code'], 'exited.code'),
      };
    case 'attention':
      return {
        t,
        session: parseSessionAddr(o['session']),
        cause: parseAttentionCause(o['cause']),
      };
    case 'progress':
      return { t, session: parseSessionAddr(o['session']), progress: parseProgress(o['progress']) };
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

export function parseHostProfile(v: unknown): HostProfile {
  const o = obj(v, 'HostProfile');
  return {
    name: str(o['name'], 'HostProfile.name'),
    // Every field but `name` is `#[serde(default)]` on the Rust side, so an
    // absent one is an empty value rather than a malformed message.
    command: o['command'] === undefined ? '' : str(o['command'], 'HostProfile.command'),
    starting_directory:
      o['starting_directory'] === undefined
        ? ''
        : str(o['starting_directory'], 'HostProfile.starting_directory'),
    icon: o['icon'] === undefined ? '' : str(o['icon'], 'HostProfile.icon'),
    color_scheme:
      o['color_scheme'] === undefined ? '' : str(o['color_scheme'], 'HostProfile.color_scheme'),
    tab_color:
      o['tab_color'] === undefined || o['tab_color'] === null
        ? null
        : num(o['tab_color'], 'HostProfile.tab_color'),
  };
}

export function parseHostOffer(v: unknown): HostOffer {
  const o = obj(v, 'HostOffer');
  return {
    os: o['os'] === undefined ? '' : str(o['os'], 'HostOffer.os'),
    arch: o['arch'] === undefined ? '' : str(o['arch'], 'HostOffer.arch'),
    os_version: o['os_version'] === undefined ? '' : str(o['os_version'], 'HostOffer.os_version'),
    default_shell:
      o['default_shell'] === undefined ? '' : str(o['default_shell'], 'HostOffer.default_shell'),
    profiles:
      o['profiles'] === undefined
        ? []
        : arr(o['profiles'], 'HostOffer.profiles').map(parseHostProfile),
  };
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
    // Absent from an older daemon (`skip_serializing_if`), and that must
    // decode as "did not say" rather than fail the whole Sessions push —
    // the HostOffer rule.
    context:
      o['context'] === undefined || o['context'] === null
        ? null
        : parseSessionContext(o['context']),
    busy: o['busy'] === undefined ? false : bool(o['busy'], 'SessionInfo.busy'),
  };
}

function parseSessionContext(v: unknown): SessionContext {
  const o = obj(v, 'SessionContext');
  return {
    git: o['git'] === undefined || o['git'] === null ? null : parseGitContext(o['git']),
    facts:
      o['facts'] === undefined ? [] : arr(o['facts'], 'SessionContext.facts').map(parseContextFact),
    revision: o['revision'] === undefined ? 0 : num(o['revision'], 'SessionContext.revision'),
  };
}

function parseGitContext(v: unknown): GitContext {
  const o = obj(v, 'GitContext');
  return {
    branch: str(o['branch'], 'GitContext.branch'),
    detached: bool(o['detached'], 'GitContext.detached'),
    dirty:
      o['dirty'] === undefined || o['dirty'] === null ? null : bool(o['dirty'], 'GitContext.dirty'),
    changed:
      o['changed'] === undefined || o['changed'] === null
        ? null
        : num(o['changed'], 'GitContext.changed'),
  };
}

function parseContextFact(v: unknown): ContextFact {
  const o = obj(v, 'ContextFact');
  const source = str(o['source'], 'ContextFact.source');
  return {
    key: str(o['key'], 'ContextFact.key'),
    value: str(o['value'], 'ContextFact.value'),
    // `source` is a trust label, so an unknown spelling (a newer daemon's
    // third variant) degrades to the *least* trusted reading rather than
    // failing the whole Sessions push — wrong in the safe direction.
    source: source === 'daemon_probe' ? 'daemon_probe' : 'shell_report',
  };
}

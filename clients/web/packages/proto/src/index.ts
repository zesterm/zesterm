/**
 * The zesterm wire protocol in TypeScript.
 *
 * A client never interprets VT. It applies grid deltas the host has already
 * parsed — see ADR-004 — so there is no terminal emulator in here and there
 * must never be one. What this package does is: strip framing, read MessagePack,
 * check the shapes, and apply the ops in the order they arrived.
 *
 * Checked against `crates/zest-proto/fixtures/`, which is the same corpus the
 * Rust decoder is replayed through, compared against the host terminal's own
 * cells rather than against the Rust decoder's reading of them.
 */

export { decode, MsgpackError } from './msgpack.ts';
export { encode } from './msgpack-encode.ts';
export { FrameReader, FrameError, MAX_FRAME, encodeFrame } from './frame.ts';
export {
  type ClientMessage,
  type SessionAddrLike,
  type IntLike,
  encodeClientMessage,
  bytesToHex,
  hexToBytes,
} from './wire-client.ts';
export {
  type Color,
  DEFAULT_COLOR,
  ColorError,
  parseColor,
  colorsEqual,
  colorToString,
} from './color.ts';
export {
  CellFlags,
  Modes,
  KNOWN_CELL_FLAGS,
  KNOWN_MODES,
  hasFlag,
  knownCellFlags,
  knownModes,
  cellFlagNames,
} from './flags.ts';
export {
  type AttrDef,
  type CellMarks,
  type Run,
  type RowPayload,
  type CursorState,
  type DeltaOp,
  type Delta,
  type BlockState,
  type BlockPayload,
  type SessionAddr,
  type SessionInfo,
  type Keyframe,
  type Update,
  type Welcome,
  type Challenge,
  type AuthPending,
  type AuthFailed,
  type PairingRequested,
  type Sessions,
  type Scrollback,
  type Exited,
  type ErrorMessage,
  type UnknownMessage,
  type HostMessage,
  WireError,
  parseHostMessage,
  parseDelta,
  parseDeltaOp,
  parseRowPayload,
  parseRun,
  parseAttrDef,
  parseCursorState,
  parseSessionAddr,
  parseSessionInfo,
  parseBlockPayload,
  isKeyframe,
  isUpdate,
  isWelcome,
  isChallenge,
  isAuthPending,
  isAuthFailed,
  isPairingRequested,
  isSessions,
  isScrollback,
  isExited,
  isErrorMessage,
} from './wire.ts';
export { type Cell, BLANK, expandRow, rowText } from './cells.ts';
export { GridView, type KeyframeState, NO_LINE } from './grid-view.ts';

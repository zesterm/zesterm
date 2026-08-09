/**
 * The data-plane client: dial, prove, attach, apply, survive.
 *
 * Platform-blind by construction — the `Dial` seam is the only thing a
 * browser, the Node sidecar, or the Lynx phone implement differently, which
 * is what lets one behavioural port of `remote.rs` serve all three.
 */

export { type ByteLink, type ByteLinkHandlers, type Dial } from './link.ts';
export { type Clock, type TimerHandle, systemClock } from './clock.ts';
export {
  HandshakeDriver,
  PROTOCOL_VERSION,
  type HandshakeOptions,
  type HandshakeState,
} from './handshake.ts';
export {
  SessionClient,
  dirtyRowsOf,
  ACK_INTERVAL_MS,
  REDIAL_MIN_MS,
  REDIAL_MAX_MS,
  type SessionClientOptions,
  type SessionEvents,
  type ConnectionState,
  type DirtyRows,
} from './session-client.ts';
export {
  ConnectionClient,
  type ConnectionClientOptions,
  type ConnectionEvents,
} from './connection-client.ts';

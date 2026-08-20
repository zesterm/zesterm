/**
 * The client's half of the wire: `ClientMessage`, typed and encoded.
 *
 * `encodeClientMessage` returns a complete frame — length prefix, MessagePack
 * body — ready to hand to a transport. The wire map for every variant is
 * constructed *here*, key by key in the Rust declaration order, rather than
 * encoding whatever object a caller built: `to_vec_named` writes struct fields
 * in declaration order with the `t` tag first, and byte-for-byte parity with
 * it is what lets a golden fixture assert equality instead of acceptance.
 *
 * # Integers
 *
 * `SessionId` and `Seq` accept `bigint` (what `wire.ts` hands out) and
 * `number` alike; the encoder writes the narrowest form either way, which is
 * exactly what the host's own encoder does.
 */

import { encodeFrame } from './frame.ts';
import { encode } from './msgpack-encode.ts';
import type { SessionAddr } from './wire.ts';

/** Accepts what `wire.ts` produces (`bigint`) and what callers type (`number`). */
export type IntLike = number | bigint;

export type ClientMessage =
  | {
      readonly t: 'hello';
      readonly version: number;
      /** 64 lowercase hex characters — the client's public key. */
      readonly client: string;
      readonly label: string;
      /** 64 lowercase hex characters; all zeroes reads as absent and is refused. */
      readonly nonce: string;
      /**
       * 64 lowercase hex characters — this connection's ephemeral X25519
       * public key. Signed into the transcript, so the Ed25519 signatures
       * already being exchanged certify it. All zeroes reads as absent and is
       * refused rather than agreed with.
       */
      readonly dh: string;
      readonly watch_sessions: boolean;
      /**
       * Ask to hear about devices waiting for approval
       * (`pairing_requested` pushes). Honoured on loopback connections
       * only — a browser client is never one, so this is `false` everywhere
       * today; it exists because the encoder is held byte-equal to the
       * host's, which always writes the field.
       */
      readonly watch_pairings: boolean;
      /**
       * Ask what this machine can offer — its facts and its own profiles —
       * and to be told when that changes (#262). Honoured on every transport,
       * unlike `watch_pairings`: reading a machine's launch targets from
       * somewhere else is the whole point.
       */
      readonly watch_hosts: boolean;
      /**
       * Ask to be told when an attached session asks to be noticed — it rang,
       * or asked for a desktop notification (#383).
       *
       * Not politeness. A `host_message` tag a client cannot decode is not
       * skipped, it ends the connection, so the daemon sends `attention` only
       * to a client that set this. Setting it is this build saying it is new
       * enough to survive them.
       */
      readonly watch_signals: boolean;
    }
  | {
      readonly t: 'auth';
      /** 128 lowercase hex characters. */
      readonly signature: string;
    }
  | { readonly t: 'pairing_decision'; readonly client: string; readonly approve: boolean }
  /**
   * Join the machine to an account (issue #227). Loopback-only, like
   * `pairing_decision`; a browser client never sends it — the type exists
   * because the encoder is held byte-equal to the host's goldens, which
   * cover every variant.
   */
  | { readonly t: 'enroll'; readonly code: string }
  | { readonly t: 'request_keyframe'; readonly session: SessionAddrLike }
  | { readonly t: 'list_sessions' }
  | {
      readonly t: 'create_session';
      readonly command: string;
      readonly cwd: string;
      readonly cols: number;
      readonly rows: number;
    }
  | {
      readonly t: 'attach';
      readonly session: SessionAddrLike;
      readonly cols: number;
      readonly rows: number;
      /**
       * Take no part in the size arbitration.
       *
       * The session runs at the smallest attached client, so a subscriber with
       * no pane -- an agent, a probe -- otherwise pins it at whatever size it
       * invented, for as long as it watches (#274). Still send `cols`/`rows`:
       * a daemon predating the flag counts them as an ordinary vote, which
       * pins rather than shrinks.
       */
      readonly observe?: boolean;
    }
  | { readonly t: 'detach'; readonly session: SessionAddrLike }
  | { readonly t: 'input'; readonly session: SessionAddrLike; readonly bytes: Uint8Array }
  | {
      readonly t: 'resize';
      readonly session: SessionAddrLike;
      readonly cols: number;
      readonly rows: number;
    }
  | { readonly t: 'ack'; readonly session: SessionAddrLike; readonly seq: IntLike }
  | {
      readonly t: 'request_scrollback';
      readonly session: SessionAddrLike;
      readonly from_line: IntLike;
      readonly count: number;
    }
  | { readonly t: 'close_session'; readonly session: SessionAddrLike };

/** `wire.ts`'s `SessionAddr` (bigint session) or a caller's plain-number one. */
export interface SessionAddrLike {
  readonly host: string;
  readonly session: IntLike;
}

/** `SessionAddr` in wire order: `host`, then `session`. */
function addr(a: SessionAddrLike): Record<string, unknown> {
  return { host: a.host, session: a.session };
}

export type { SessionAddr };

/**
 * Encode a message into a complete, length-prefixed frame.
 *
 * The switch rebuilds each variant's map in declaration order on purpose —
 * TypeScript objects *usually* arrive in this order already, but "usually" is
 * not a wire format.
 */
export function encodeClientMessage(msg: ClientMessage): Uint8Array {
  return encodeFrame(encodeClientMessageBody(msg));
}

/**
 * The MessagePack body, without the length prefix.
 *
 * Split out because since protocol 3 the bytes behind the prefix are
 * ciphertext, so the prefix has to be written *after* sealing — and it must
 * describe the ciphertext, which is 16 bytes longer than this.
 */
export function encodeClientMessageBody(msg: ClientMessage): Uint8Array {
  let wire: Record<string, unknown>;
  switch (msg.t) {
    case 'hello':
      wire = {
        t: msg.t,
        version: msg.version,
        client: msg.client,
        label: msg.label,
        nonce: msg.nonce,
        dh: msg.dh,
        watch_sessions: msg.watch_sessions,
        watch_pairings: msg.watch_pairings,
        watch_hosts: msg.watch_hosts,
        watch_signals: msg.watch_signals,
      };
      break;
    case 'auth':
      wire = { t: msg.t, signature: msg.signature };
      break;
    case 'pairing_decision':
      wire = { t: msg.t, client: msg.client, approve: msg.approve };
      break;
    case 'enroll':
      wire = { t: msg.t, code: msg.code };
      break;
    case 'request_keyframe':
      wire = { t: msg.t, session: addr(msg.session) };
      break;
    case 'list_sessions':
      wire = { t: msg.t };
      break;
    case 'create_session':
      wire = { t: msg.t, command: msg.command, cwd: msg.cwd, cols: msg.cols, rows: msg.rows };
      break;
    case 'attach':
      wire = {
        t: msg.t,
        session: addr(msg.session),
        cols: msg.cols,
        rows: msg.rows,
        // Always written, never conditional on being true: `rmp_serde`'s
        // `to_vec_named` emits every field, and byte parity with the Rust
        // encoder is what `client-messages.json` checks.
        observe: msg.observe ?? false,
      };
      break;
    case 'detach':
      wire = { t: msg.t, session: addr(msg.session) };
      break;
    case 'input':
      wire = { t: msg.t, session: addr(msg.session), bytes: msg.bytes };
      break;
    case 'resize':
      wire = { t: msg.t, session: addr(msg.session), cols: msg.cols, rows: msg.rows };
      break;
    case 'ack':
      wire = { t: msg.t, session: addr(msg.session), seq: msg.seq };
      break;
    case 'request_scrollback':
      wire = {
        t: msg.t,
        session: addr(msg.session),
        from_line: msg.from_line,
        count: msg.count,
      };
      break;
    case 'close_session':
      wire = { t: msg.t, session: addr(msg.session) };
      break;
  }
  return encode(wire);
}

// ---------------------------------------------------------------------------
// Hex, for the identity fields
// ---------------------------------------------------------------------------

/**
 * Lowercase hex, two characters per byte — `zest-proto/src/hex.rs`. Ids,
 * nonces and signatures all travel this way; a raw byte array on the wire
 * would be a 32-element list, unreadable in a log line.
 */
export function bytesToHex(bytes: Uint8Array): string {
  let out = '';
  for (const b of bytes) out += b.toString(16).padStart(2, '0');
  return out;
}

/** Strict inverse: even length, hex only, never padded. */
export function hexToBytes(hex: string): Uint8Array {
  if (hex.length % 2 !== 0) throw new Error(`hex of odd length ${hex.length}`);
  const out = new Uint8Array(hex.length / 2);
  for (let i = 0; i < out.length; i++) {
    const byte = Number.parseInt(hex.slice(i * 2, i * 2 + 2), 16);
    if (Number.isNaN(byte)) throw new Error(`not hex at ${i * 2}: ${hex.slice(i * 2, i * 2 + 2)}`);
    out[i] = byte;
  }
  return out;
}

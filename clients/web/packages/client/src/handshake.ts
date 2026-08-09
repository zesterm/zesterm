/**
 * The client's half of Hello → Challenge → Auth → Welcome, as a pure state
 * machine: host messages in, client messages out, no I/O and no clock. The
 * session layer owns encoding and transport; this owns the *order* — and the
 * order is the security property: the host proves itself first, and nothing
 * is signed for a host that failed to.
 */

import {
  answerChallenge,
  ChallengeError,
  type ClientIdentity,
} from '@zesterm/auth';
import {
  bytesToHex,
  type ClientMessage,
  type HostMessage,
  isAuthFailed,
  isAuthPending,
  isChallenge,
  isWelcome,
} from '@zesterm/proto';

export const PROTOCOL_VERSION = 2;

export type HandshakeState =
  | { readonly phase: 'connecting' }
  | { readonly phase: 'awaiting-approval'; readonly code: string; readonly expiresInSecs: number }
  | { readonly phase: 'welcomed'; readonly host: string; readonly hostLabel: string }
  | { readonly phase: 'failed'; readonly reason: string; readonly message: string; readonly retryable: boolean };

export interface HandshakeOptions {
  readonly identity: ClientIdentity;
  readonly label: string;
  readonly watchSessions: boolean;
  /** Pin the host id an advertisement or directory claimed; omit for a hand-typed address. */
  readonly expectedHost?: string;
  /** For tests: a fixed nonce instead of a random one. */
  readonly nonce?: Uint8Array;
}

/**
 * Refusals a client must not retry — `AuthFailure::is_retryable`'s other
 * half, mirrored so a denied device does not hammer the host's rate limiter
 * into locking the *user* out.
 */
const NEVER_RETRY = new Set(['denied', 'version']);

export class HandshakeDriver {
  #options: HandshakeOptions;
  #nonce: string;
  #state: HandshakeState = { phase: 'connecting' };

  constructor(options: HandshakeOptions) {
    this.#options = options;
    // A fresh nonce per dial is what scopes the host's proof to this
    // connection; reusing one would make a captured challenge replayable.
    const nonce = options.nonce ?? crypto.getRandomValues(new Uint8Array(32));
    this.#nonce = bytesToHex(nonce);
  }

  get state(): HandshakeState {
    return this.#state;
  }

  /** The first message on the wire. */
  hello(): ClientMessage {
    return {
      t: 'hello',
      version: PROTOCOL_VERSION,
      client: this.#options.identity.clientId,
      label: this.#options.label,
      nonce: this.#nonce,
      watch_sessions: this.#options.watchSessions,
    };
  }

  /**
   * Feed one host message. Returns messages to send, if any. Messages that
   * are not the handshake's business (a keyframe racing the welcome cannot
   * happen, but session pushes after `welcomed` can) return nothing and are
   * the caller's to route.
   */
  onMessage(msg: HostMessage): ClientMessage[] {
    if (isChallenge(msg)) {
      try {
        const signature = answerChallenge({
          identity: this.#options.identity,
          transcript: {
            version: msg.version,
            host: msg.host,
            client: this.#options.identity.clientId,
            hostNonce: msg.nonce,
            clientNonce: this.#nonce,
            hostLabel: msg.label,
            clientLabel: this.#options.label,
          },
          hostSignature: msg.signature,
          ...(this.#options.expectedHost === undefined
            ? {}
            : { expectedHost: this.#options.expectedHost }),
        });
        return [{ t: 'auth', signature }];
      } catch (e) {
        // A host that cannot prove itself is not a host to keep talking to —
        // and crucially, nothing was signed for it.
        this.#state = {
          phase: 'failed',
          reason: 'host-unproven',
          message: e instanceof ChallengeError ? e.message : String(e),
          retryable: false,
        };
        return [];
      }
    }
    if (isAuthPending(msg)) {
      this.#state = {
        phase: 'awaiting-approval',
        code: msg.code,
        expiresInSecs: msg.expires_in_secs,
      };
      return [];
    }
    if (isWelcome(msg)) {
      this.#state = { phase: 'welcomed', host: msg.host, hostLabel: msg.label };
      return [];
    }
    if (isAuthFailed(msg)) {
      this.#state = {
        phase: 'failed',
        reason: msg.reason,
        message: msg.message,
        retryable: !NEVER_RETRY.has(msg.reason),
      };
      return [];
    }
    return [];
  }
}

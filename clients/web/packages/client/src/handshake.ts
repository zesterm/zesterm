/**
 * The client's half of Hello → Challenge → Auth → Welcome, as a pure state
 * machine: host messages in, client messages out, no I/O and no clock. The
 * session layer owns encoding and transport; this owns the *order* — and the
 * order is the security property: the host proves itself first, and nothing
 * is signed for a host that failed to.
 *
 * `onMessage` is async because a non-extractable device key signs through
 * `crypto.subtle` and cannot do otherwise — see `@zesterm/auth`'s
 * `ClientSigner`. Verification stays synchronous, so the ordering above is
 * unchanged: everything that can refuse a host has already refused it by the
 * time this returns a promise at all.
 */

import { answerChallenge, ChallengeError, type ClientSigner } from '@zesterm/auth';
import { generateEphemeral, type Ephemeral, type SecureChannel } from '@zesterm/auth/secure';
import {
  bytesToHex,
  type ClientMessage,
  type HostMessage,
  isAuthFailed,
  isAuthPending,
  isChallenge,
  isWelcome,
} from '@zesterm/proto';

export const PROTOCOL_VERSION = 3;

export type HandshakeState =
  | { readonly phase: 'connecting' }
  | { readonly phase: 'awaiting-approval'; readonly code: string; readonly expiresInSecs: number }
  | { readonly phase: 'welcomed'; readonly host: string; readonly hostLabel: string }
  | { readonly phase: 'failed'; readonly reason: string; readonly message: string; readonly retryable: boolean };

export interface HandshakeOptions {
  readonly signer: ClientSigner;
  readonly label: string;
  readonly watchSessions: boolean;
  /**
   * Ask what this machine offers — its facts and its own profiles (#262).
   *
   * Unlike `watch_pairings` this is honoured on every transport, so a browser
   * really does get an answer. Defaults to off: a connection opened to attach
   * to one session has no use for the far machine's whole profile list.
   */
  readonly watchHosts?: boolean;
  /** Pin the host id an advertisement or directory claimed; omit for a hand-typed address. */
  readonly expectedHost?: string;
  /** For tests: a fixed nonce instead of a random one. */
  readonly nonce?: Uint8Array;
  /** For tests and fixtures: a fixed ephemeral instead of a fresh one. */
  readonly ephemeral?: Ephemeral;
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
  #ephemeral: Ephemeral;
  #channel: SecureChannel | null = null;
  #state: HandshakeState = { phase: 'connecting' };

  constructor(options: HandshakeOptions) {
    this.#options = options;
    // A fresh nonce per dial is what scopes the host's proof to this
    // connection; reusing one would make a captured challenge replayable.
    const nonce = options.nonce ?? crypto.getRandomValues(new Uint8Array(32));
    this.#nonce = bytesToHex(nonce);
    // Likewise fresh per dial, and for a second reason: the ephemeral dying
    // with the connection is what makes the traffic forward-secret. A redial
    // is a whole new handshake, never a resumption, so there is nothing for a
    // relay to preserve across a drop.
    this.#ephemeral = options.ephemeral ?? generateEphemeral();
  }

  get state(): HandshakeState {
    return this.#state;
  }

  /**
   * This connection's channel, once the challenge has been answered.
   *
   * `null` before then, which is exactly the positional seal switch: the
   * caller seals nothing until it has this, and everything once it does.
   */
  get channel(): SecureChannel | null {
    return this.#channel;
  }

  /** The first message on the wire. */
  hello(): ClientMessage {
    return {
      t: 'hello',
      version: PROTOCOL_VERSION,
      client: this.#options.signer.clientId,
      label: this.#options.label,
      nonce: this.#nonce,
      dh: this.#ephemeral.publicKey,
      watch_sessions: this.#options.watchSessions,
      // Never from a browser: approvals are loopback authority, and the
      // daemon would silently not subscribe us anyway.
      watch_pairings: false,
      watch_hosts: this.#options.watchHosts ?? false,
      // Unconditional: this is a build that can decode `attention`, and the
      // flag is what tells the daemon so. It is not a preference — an
      // undecodable frame ends a connection rather than being skipped, which
      // is why the host has to be told rather than left to guess.
      watch_signals: true,
    };
  }

  /**
   * Feed one host message. Resolves with the messages to send, if any.
   * Messages that are not the handshake's business (a keyframe racing the
   * welcome cannot happen, but session pushes after `welcomed` can) return
   * nothing and are the caller's to route.
   *
   * **Callers must await one call before making the next**, and must send the
   * replies in that order. Only the challenge actually suspends, but a caller
   * that fires two of these off in parallel can have the state machine move
   * under a pending signature — and would then attach before authenticating.
   * `SessionClient` and `ConnectionClient` queue their frames for this reason.
   */
  async onMessage(msg: HostMessage): Promise<ClientMessage[]> {
    if (isChallenge(msg)) {
      try {
        const transcript = {
          version: msg.version,
          host: msg.host,
          client: this.#options.signer.clientId,
          hostNonce: msg.nonce,
          clientNonce: this.#nonce,
          hostLabel: msg.label,
          clientLabel: this.#options.label,
          hostDh: msg.dh,
          clientDh: this.#ephemeral.publicKey,
        };
        const signature = await answerChallenge({
          signer: this.#options.signer,
          transcript,
          hostSignature: msg.signature,
          ...(this.#options.expectedHost === undefined
            ? {}
            : { expectedHost: this.#options.expectedHost }),
        });
        // **After** `answerChallenge`, which verifies the host. Deriving first
        // would be harmless in itself, but it would put the key's existence
        // before the proof in the reader's eye — and this ordering is the
        // security property the whole file is arranged around.
        this.#channel = this.#ephemeral.agree(msg.dh, transcript, 'client');
        return [{ t: 'auth', signature }];
      } catch (e) {
        // A host that cannot prove itself is not a host to keep talking to —
        // and crucially, nothing was signed for it.
        //
        // The await also puts a *signer* failure here: a device key the
        // browser has evicted, or a `crypto.subtle` that will not sign after
        // all. Not retryable either — redialling reaches the same broken
        // key — but named separately, because reporting it as `host-unproven`
        // sends someone to inspect a daemon that did nothing wrong.
        const unproven = e instanceof ChallengeError;
        this.#state = {
          phase: 'failed',
          reason: unproven ? 'host-unproven' : 'signer-failed',
          message: unproven ? e.message : `this device could not sign: ${String(e)}`,
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

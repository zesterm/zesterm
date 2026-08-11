/**
 * The Durable Object. One per host, and the only place a daemon and a browser
 * ever meet.
 *
 * It now speaks the control-link handshake: a daemon dials `/v1/control`, is
 * challenged, proves it holds the key its `HostId` names, and its link is
 * parked. A browser dialling `/v1/attach` is told which of the two things is
 * wrong — nobody is home, or the pipe does not exist yet.
 *
 * **Nothing lives in instance fields.** The object is evicted between messages,
 * so tags and attachments are what survive: which sockets are control links is
 * `getWebSockets('role:control')`, and which of them authenticated is each
 * socket's attachment. The nonce is in there too, which is the part that looks
 * safe to keep in a field and is not — see `room/control.ts`. The guard is
 * `test/control.test.ts`, which runs its whole suite twice, the second time
 * building a new `RelayRoom` before every single handler call.
 *
 * `fetch` itself has no unit test and cannot have one: `WebSocketPair` and
 * `WebSocketRequestResponsePair` are workerd globals with no standalone
 * equivalent. Everything after the socket exists is therefore a method that
 * takes a `Sock` — `openControlLink`, `webSocketMessage`, `refuseAttach` — so
 * the fake platform drives all of it, and `fetch` is left holding the two
 * `new`s and the routing around them.
 */

import { getPublicKeyAsync } from '@noble/ed25519';
import { fromHex, hex } from '@zesterm/cloud-shared';

import type { Env } from './env.ts';
import { ATTACH_PATH, CONTROL_PATH } from './routes.ts';
import {
  attachRefusal,
  challengeMessage,
  closeCodeFor,
  CLOSE_CONTROL_PROTOCOL,
  CLOSE_CONTROL_REPLACED,
  CONTROL_HANDSHAKE_TTL_MS,
  errorMessage,
  helloSignatureIsValid,
  KEEPALIVE_REQUEST,
  KEEPALIVE_RESPONSE,
  newNonce,
  NONCE_BYTES,
  parseHello,
  readControlAttachment,
  readyControlLinks,
  readyMessage,
  TAG_CONTROL,
  type ControlAttachment,
  type ControlErrorCode,
} from './room/control.ts';
import { hostIsEnrolled, touchHost } from './room/hosts.ts';
import type { AutoResponsePair, RoomState, Sock } from './room/state.ts';

/** An Ed25519 seed. Always 32 bytes. */
const RELAY_KEY_LEN = 32;

/**
 * A compile-time proof that the hand-declared `RoomState` really is a subset of
 * the platform's `DurableObjectState`.
 *
 * `room/state.ts` is declared rather than imported so the tests need no
 * workerd. The failure that buys is a declared subset which has drifted from
 * the real interface — tests passing against a shape production does not have,
 * which is worse than having no tests. This alias costs nothing and fails the
 * typecheck the moment the two disagree.
 */
type Subset<Narrow, Wide extends Narrow> = Wide;
export type _PlatformIsARoomState = Subset<RoomState, DurableObjectState>;

export class RelayRoom {
  readonly #state: RoomState;
  readonly #env: Env;

  constructor(state: RoomState, env: Env) {
    this.#state = state;
    this.#env = env;
  }

  async fetch(request: Request): Promise<Response> {
    if (request.headers.get('upgrade')?.toLowerCase() !== 'websocket') {
      return new Response('expected a WebSocket upgrade', { status: 426 });
    }

    const url = new URL(request.url);
    if (url.pathname !== CONTROL_PATH && url.pathname !== ATTACH_PATH) {
      return new Response('not found', { status: 404 });
    }

    const { 0: client, 1: server } = new WebSocketPair();
    if (url.pathname === CONTROL_PATH) {
      // The host comes from the query string the Worker addressed this object
      // with, and that is the *only* thing binding the object's name to a
      // machine: `index.ts` derives `idFromName('host:' + host)` from the same
      // string. The `hello` is checked against it rather than believed.
      await this.openControlLink(
        server,
        url.searchParams.get('host') ?? '',
        new WebSocketRequestResponsePair(KEEPALIVE_REQUEST, KEEPALIVE_RESPONSE),
      );
    } else {
      this.refuseAttach(server);
    }

    return new Response(null, { status: 101, webSocket: client });
  }

  /**
   * Accept a daemon's control link and challenge it.
   *
   * `keepalive` is passed in rather than constructed here for the reason at the
   * top of this file: it is a workerd global, and this method is otherwise the
   * whole of the connect path that a test can drive.
   */
  async openControlLink(
    ws: Sock,
    host: string,
    keepalive: AutoResponsePair,
    now: number = Date.now(),
  ): Promise<void> {
    // Ahead of every refusal below rather than only on the happy path. It is
    // object-wide and idempotent, and a room whose first dial was refused would
    // otherwise pay a request for every ping the next daemon sends.
    this.#state.setWebSocketAutoResponse(keepalive);
    this.#state.acceptWebSocket(ws, [TAG_CONTROL]);

    const seed = fromHex(this.#env.RELAY_SIGNING_KEY ?? '', RELAY_KEY_LEN);
    if (seed === null) {
      this.#refuse(ws, 'unconfigured');
      return;
    }

    const nonce = newNonce();
    // Written before the challenge is sent, not after. The daemon's answer can
    // arrive before this method's next line runs, and an attachment written
    // afterwards would race a `hello` against the only record of what it must
    // answer.
    ws.serializeAttachment({ s: 'challenged', host, nonce, at: now } satisfies ControlAttachment);
    // One scalar multiplication per dial, on the connect path. Caching it in
    // storage would trade that for a write, which is the more expensive of the
    // two and the one ADR-009 asks to avoid.
    ws.send(challengeMessage({ nonce, relayKey: hex(await getPublicKeyAsync(seed)) }));
  }

  /**
   * The daemon's `hello`, and nothing else this build understands.
   *
   * `now` is a parameter with a default the platform never passes, so the
   * challenge's own expiry is testable without a clock. `index.ts`'s
   * `attachVerdict` takes it the same way.
   */
  async webSocketMessage(
    ws: Sock,
    message: string | ArrayBuffer,
    now: number = Date.now(),
  ): Promise<void> {
    const attachment = readControlAttachment(ws);
    if (attachment === null) {
      // Not a control link this build wrote — a socket from a previous version
      // of the attachment shape, or one that was never challenged. There is
      // nothing to answer it with.
      ws.close(CLOSE_CONTROL_PROTOCOL, 'not a control link');
      return;
    }
    if (attachment.s === 'ready') {
      // A parked link says nothing until the relay asks it to dial back, and
      // this build never asks. Hanging up rather than ignoring, because the
      // only thing that reaches here is a daemon speaking a protocol this
      // relay does not, and a silently-dropped frame is the harder half of
      // that to diagnose from the other end.
      this.#refuse(ws, 'malformed');
      return;
    }

    const parsed = parseHello(message);
    if (!parsed.ok) {
      this.#refuse(ws, parsed.error);
      return;
    }

    // The challenge first, because a stale one is not a signature failure and
    // telling a daemon with a slow link that its key is bad sends it to
    // re-enrol when it should simply redial.
    if (now - attachment.at > CONTROL_HANDSHAKE_TTL_MS || now < attachment.at) {
      this.#refuse(ws, 'stale');
      return;
    }

    // This object is one host's. A `hello` naming another machine is not an
    // authentication failure — it may be a perfectly good signature — it is a
    // daemon that dialled the wrong room, and saying so is what stops that
    // being debugged as a crypto problem.
    if (parsed.hello.host !== attachment.host) {
      this.#refuse(ws, 'wrong-room');
      return;
    }

    const nonce = fromHex(attachment.nonce, NONCE_BYTES);
    if (nonce === null || !(await helloSignatureIsValid(parsed.hello, nonce))) {
      this.#refuse(ws, 'bad-signature');
      return;
    }

    // Possession of the key is not a claim on an account: it says which machine
    // this is, not that the machine is still ours. → `room/hosts.ts`.
    const enrolled = await hostIsEnrolled({
      db: this.#env.DB,
      storage: this.#state.storage,
      host: attachment.host,
      now,
    });
    if (!enrolled) {
      this.#refuse(ws, 'unknown-host');
      return;
    }

    await touchHost(this.#env.DB, attachment.host, now);

    // One control link per host. A daemon that changed networks leaves a socket
    // the platform has not yet reaped, and two parked links would make "which
    // one do I dial back through" a question with no answer.
    //
    // Done *before* this link is marked ready, so the loop cannot reach it
    // whatever `getWebSockets` hands back — betting a hang-up on object
    // identity surviving a hibernation boundary is not a bet worth making.
    for (const stale of readyControlLinks(this.#state)) {
      // The attachment is cleared as well as the socket closed. "Ready" is a
      // fact this room keeps, not one the platform does, and whether a socket
      // that has just been told to close still appears in `getWebSockets`
      // depends on a close handshake with a peer that may be gone — an
      // assumption, and one that would show up as an attach being told the host
      // is present when it has been hung up on.
      stale.serializeAttachment(null);
      stale.close(CLOSE_CONTROL_REPLACED, 'replaced by a newer control link');
    }

    ws.serializeAttachment({
      s: 'ready',
      host: attachment.host,
      // Unsigned, and kept only as something to put in a log line. See `Hello`.
      label: parsed.hello.label,
      at: now,
    } satisfies ControlAttachment);
    ws.send(readyMessage());
  }

  /**
   * A browser whose ticket verified, and the two things that can still be
   * wrong.
   *
   * "Present" here means a link the platform still holds and that authenticated
   * — which is as fresh as the platform's own detection of a peer that vanished
   * without closing. Dial-back is what turns that into a fact: a host that is
   * listed and does not answer the `open` fails the attach with a reason.
   *
   * No `webSocketClose` handler anywhere in this class, deliberately: the
   * pairing is re-derived from `getWebSockets` on every call, so a socket that
   * goes away needs no bookkeeping — and bookkeeping is the thing that would
   * have to live in a field.
   */
  refuseAttach(ws: Sock): void {
    const refusal = attachRefusal(this.#state);
    // The hibernatable accept, not `server.accept()`, even though this socket
    // is closed immediately. The two differ in whether the object stays awake
    // holding the connection, and picking the cheap one only once there is
    // traffic to justify it is how a room ends up never hibernating.
    this.#state.acceptWebSocket(ws);
    ws.close(refusal.code, refusal.reason);
  }

  /** The error frame, then the hang-up. The frame carries which; the code, how bad. */
  #refuse(ws: Sock, code: ControlErrorCode): void {
    ws.send(errorMessage(code));
    ws.close(closeCodeFor(code), code);
  }
}

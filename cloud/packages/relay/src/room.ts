/**
 * The Durable Object. One per host, and the only place a daemon and a browser
 * ever meet.
 *
 * A daemon dials `/v1/control`, is challenged, proves it holds the key its
 * `HostId` names, and its link is parked. A browser dialling `/v1/attach`
 * causes a *pipe* to come into existence: the object mints an id, sends `open`
 * down the parked link, the daemon dials `/v1/pipe` for that id alone, and from
 * then on this class is a byte pump between the two sockets. → ADR-009, and
 * `room/pipe.ts` for the id's role as the host leg's only credential.
 *
 * **Nothing lives in instance fields** — with one exception, `#dialling`, whose
 * own comment argues for itself. The object is evicted between messages, so
 * tags and attachments are what survive: which sockets are control links is
 * `getWebSockets('role:control')`, which of them authenticated is each socket's
 * attachment, and which two sockets are one pipe is `getWebSockets('pipe:<id>')`
 * on every single message. The nonce is in there too, which is the part that
 * looks safe to keep in a field and is not — see `room/control.ts`. The guard
 * is `test/control.test.ts` and `test/pipe.test.ts`, which run their whole
 * suites twice, the second time building a new `RelayRoom` before every handler
 * call.
 *
 * `fetch` itself has no unit test and cannot have one: `WebSocketPair` and
 * `WebSocketRequestResponsePair` are workerd globals with no standalone
 * equivalent. Everything after the socket exists is therefore a method that
 * takes a `Sock` — `openControlLink`, `openAttach`, `openPipeLeg`,
 * `webSocketMessage`, `webSocketClose` — so the fake platform drives all of it,
 * and `fetch` is left holding the `new`s and the routing around them.
 */

import { getPublicKeyAsync } from '@noble/ed25519';
import { fromHex, hex } from '@zesterm/cloud-shared';

import type { Env } from './env.ts';
import { attachJti, ATTACH_PATH, CONTROL_PATH, PIPE_PATH } from './routes.ts';
import {
  challengeMessage,
  closeCodeFor,
  CLOSE_CONTROL_PROTOCOL,
  CLOSE_CONTROL_REPLACED,
  CLOSE_HOST_ABSENT,
  CONTROL_HANDSHAKE_TTL_MS,
  errorMessage,
  helloSignatureIsValid,
  KEEPALIVE_REQUEST,
  KEEPALIVE_RESPONSE,
  newNonce,
  NONCE_BYTES,
  parseHello,
  readControlAttachment,
  readyControlLink,
  readyControlLinks,
  readyMessage,
  TAG_CONTROL,
  type ControlAttachment,
  type ControlErrorCode,
} from './room/control.ts';
import { hostIsEnrolled, touchHost } from './room/hosts.ts';
import { PIPE_DIAL_TIMEOUT_MS } from './room/limits.ts';
import {
  CLOSE_PIPE_DIAL_TIMEOUT,
  CLOSE_PIPE_FLOOD,
  CLOSE_PIPE_PEER_GONE,
  CLOSE_TICKET_REPLAYED,
  messageSize,
  newPipeId,
  openMessage,
  pipeMembership,
  pipePeer,
  pipeTag,
  PipeBudgets,
  ROLE_CLIENT,
  ROLE_HOST,
  type PipeMembership,
} from './room/pipe.ts';
import { claimAttachTicket, sweepSpentTickets } from './room/replay.ts';
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

  /**
   * Pipes whose `open` has gone out and whose host has not dialled back yet.
   *
   * **The one instance field in this class, and it looks exactly like the bug
   * every other line here is shaped to avoid.** It is legal for one reason: a
   * Durable Object cannot be evicted while a `fetch` is in flight, the awaiting
   * `fetch` *is* the attach, and the host's dial arrives as a second `fetch` on
   * that same — therefore live — instance. Nothing here outlives the `await`
   * that put it there; every path deletes its own entry before resolving.
   *
   * The rule it is an exception to is about state that must survive the gap
   * *between* two messages, which is where the object really is evicted. See
   * the challenge nonce in `room/control.ts` for one that looks even safer and
   * is not.
   */
  readonly #dialling = new Map<string, () => void>();

  /**
   * Token buckets per pipe leg, rebuilt full after any eviction.
   *
   * Instance memory on purpose, and `PipeBudgets` argues why at length: the
   * alternative is a storage write per message, which ADR-009 forbids outright,
   * to defend a limit whose only lost state is state that would have refilled
   * during the very idle gap that lost it.
   */
  readonly #budgets = new PipeBudgets();

  constructor(state: RoomState, env: Env) {
    this.#state = state;
    this.#env = env;
  }

  async fetch(request: Request): Promise<Response> {
    if (request.headers.get('upgrade')?.toLowerCase() !== 'websocket') {
      return new Response('expected a WebSocket upgrade', { status: 426 });
    }

    const url = new URL(request.url);

    if (url.pathname === CONTROL_PATH) {
      const { 0: client, 1: server } = new WebSocketPair();
      // The host comes from the query string the Worker addressed this object
      // with, and that is the *only* thing binding the object's name to a
      // machine: `index.ts` derives `idFromName('host:' + host)` from the same
      // string. The `hello` is checked against it rather than believed.
      await this.openControlLink(
        server,
        url.searchParams.get('host') ?? '',
        new WebSocketRequestResponsePair(KEEPALIVE_REQUEST, KEEPALIVE_RESPONSE),
      );
      return new Response(null, { status: 101, webSocket: client });
    }

    if (url.pathname === PIPE_PATH) {
      const { 0: client, 1: server } = new WebSocketPair();
      if (!this.openPipeLeg(server, url.searchParams.get('pipe') ?? '')) {
        // A status, where every browser refusal is a close code. The asymmetry
        // is the far end: this one is `zest-daemon`, which reads the upgrade
        // response, and a browser cannot — see `index.ts`. An id that names no
        // pending pipe is a dial that arrived after the deadline or a guess,
        // and neither is worth a socket.
        return new Response('no such pipe', { status: 404 });
      }
      return new Response(null, { status: 101, webSocket: client });
    }

    if (url.pathname === ATTACH_PATH) {
      const { 0: client, 1: server } = new WebSocketPair();
      // **Awaited, so the 101 goes out only once the pipe has two ends.** The
      // browser's `open` fires when this response arrives, and `SessionClient`
      // sends its `Hello` immediately after — so a 101 written before the host
      // leg exists produces bytes with nowhere to live but an instance field or
      // an attachment, which is either the hibernation bug or a storage write
      // on what is about to be the data path. Awaiting removes the problem
      // rather than relocating it.
      //
      // `?jti=` is the id of a ticket the Worker has already verified — the
      // room is handed the one field it needs, and never the ticket, so it
      // cannot be mistaken for a second verifier. It is spent here rather than
      // at the edge because only this object is single-instance per host;
      // `room/replay.ts` argues that at length.
      await this.openAttach(server, attachJti(url));
      return new Response(null, { status: 101, webSocket: client });
    }

    return new Response('not found', { status: 404 });
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
   * A pipe leg's bytes, or the daemon's `hello`, and nothing else.
   *
   * `now` is a parameter with a default the platform never passes, so the
   * challenge's own expiry and the pipe's token buckets are testable without a
   * clock. `index.ts`'s `attachVerdict` takes it the same way.
   */
  async webSocketMessage(
    ws: Sock,
    message: string | ArrayBuffer,
    now: number = Date.now(),
  ): Promise<void> {
    // The pipe first, because it is the hot path and because it is decided by
    // tags — fixed at `acceptWebSocket` and unrewritable, so a pipe leg cannot
    // become anything else however the control-link state moves.
    const member = pipeMembership(this.#state, ws);
    if (member !== null) {
      this.#pump(member, ws, message, now);
      return;
    }

    const attachment = readControlAttachment(ws);
    if (attachment === null) {
      // Not a control link this build wrote — a socket from a previous version
      // of the attachment shape, or one that was never challenged. There is
      // nothing to answer it with.
      ws.close(CLOSE_CONTROL_PROTOCOL, 'not a control link');
      return;
    }
    if (attachment.s === 'ready') {
      // A parked link is one-way after the handshake: the relay sends `open`
      // and the daemon answers by dialling `/v1/pipe`, never by replying here.
      // Hanging up rather than ignoring, because the only thing that reaches
      // this line is a daemon speaking a protocol this relay does not, and a
      // silently-dropped frame is the harder half of that to diagnose from the
      // other end.
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
   * A browser whose ticket verified, and the pipe it causes to exist.
   *
   * The ticket verified at the edge; what is decided here is whether it has
   * been spent, which is the half of the check that needs state — see
   * `room/replay.ts`.
   *
   * "Present" means a control link the platform still holds and that
   * authenticated — which is only as fresh as the platform's own detection of a
   * peer that vanished without closing. The dial-back is what turns that into a
   * fact: a host that is listed and does not answer the `open` fails the attach
   * with `CLOSE_PIPE_DIAL_TIMEOUT` rather than leaving a browser attached to a
   * machine that is not there.
   *
   * `timeoutMs` is a parameter with a default the platform never passes, for
   * the same reason `now` is one on `webSocketMessage`: the ten-second deadline
   * is testable without ten seconds.
   */
  async openAttach(
    ws: Sock,
    jti: string,
    now: number = Date.now(),
    timeoutMs: number = PIPE_DIAL_TIMEOUT_MS,
  ): Promise<void> {
    // First, and ahead of every line below it: a replayed ticket must cost the
    // host nothing — no `open` frame down its control link, no pipe id, no
    // ten-second timer, no socket it has to dial. Spending the id is one
    // storage write on the *attach* path, which is not the path ADR-009's cost
    // model is about.
    //
    // Spent here rather than once the pipe is up, so a ticket is burnt even by
    // an attach that then fails. That direction is the cheap one, and it costs
    // the browser nothing: `relay-dial.ts` mints inside the `Dial` itself, so
    // every redial already carries a fresh ticket. Spending only on success
    // would leave a captured one good for another try each time the host
    // happened to be asleep.
    const claim = await claimAttachTicket({ storage: this.#state.storage, jti, now });
    if (claim !== 'claimed') {
      // Accepted before it is closed, as the host-absent branch below is and
      // for the reason given there.
      this.#state.acceptWebSocket(ws);
      const why =
        claim === 'spent'
          ? 'this attach ticket has been spent'
          : 'this attach ticket cannot be spent';
      ws.close(CLOSE_TICKET_REPLAYED, why);
      return;
    }

    const control = readyControlLink(this.#state);
    if (control === null) {
      // The hibernatable accept, not `server.accept()`, even though this socket
      // is closed immediately. The two differ in whether the object stays awake
      // holding the connection, and picking the cheap one only once there is
      // traffic to justify it is how a room ends up never hibernating.
      this.#state.acceptWebSocket(ws);
      ws.close(CLOSE_HOST_ABSENT, 'no control link for this host');
      return;
    }

    const id = newPipeId();
    // Tagged and accepted *before* the `open` goes out, because the daemon's
    // dial can land before this method's next line runs. A host leg that
    // arrived first would otherwise find a pipe with one end.
    this.#state.acceptWebSocket(ws, [pipeTag(id), ROLE_CLIENT]);
    // Guarded, because `getWebSockets` can hand back a socket the platform has
    // not finished closing — this file's own tests model that state, and
    // `control.ts` refuses to rest the "is a host present" answer on a close
    // handshake with a peer that may be gone. An uncaught throw here rejects
    // `openAttach`, so the browser gets a 500 instead of a close code, and a
    // 500 is the one answer it cannot read.
    try {
      control.send(openMessage({ pipe: id, exp: now + timeoutMs }));
    } catch {
      this.#dialling.delete(id);
      ws.close(CLOSE_HOST_ABSENT, 'no control link for this host');
      return;
    }

    if (!(await this.#dialBack(id, timeoutMs))) {
      ws.close(CLOSE_PIPE_DIAL_TIMEOUT, 'the host did not dial back');
    }
  }

  /**
   * The daemon's second connection, for one pipe.
   *
   * Returns whether the id named a pipe that is still waiting for it. `false`
   * is a dial that missed its deadline, a second dial for an id already
   * claimed, or a guess — all of which are the same answer, because
   * distinguishing them would tell a guesser which half of the id was right.
   */
  openPipeLeg(ws: Sock, id: string): boolean {
    const arrived = this.#dialling.get(id);
    if (arrived === undefined) return false;
    // Accepted before the attach is released, so the browser's 101 cannot go
    // out ahead of a socket the pump would fail to find.
    this.#state.acceptWebSocket(ws, [pipeTag(id), ROLE_HOST]);
    arrived();
    return true;
  }

  /**
   * Either end of a pipe going away ends the pipe.
   *
   * There was deliberately no close handler here while the room only parked
   * control links: the pairing is re-derived from `getWebSockets` every time,
   * so a socket that goes away needs no bookkeeping. A pipe changes that, and
   * not for bookkeeping either — the *survivor* has to be told, or a daemon
   * holds a socket and a `serve()` thread for a browser tab that closed hours
   * ago, which is exactly what `CLOSE_PIPE_PEER_GONE` is for.
   *
   * A control link still needs nothing: a replacement hangs up its predecessor
   * at `hello` time.
   */
  webSocketClose(ws: Sock): void {
    const member = pipeMembership(this.#state, ws);
    if (member === null) return;
    this.#endPipe(member, pipePeer(this.#state, member));
  }

  /**
   * A socket that failed rather than closed, which is the same news.
   *
   * The runtime is not obliged to follow an error with a close, and the failure
   * that would leave behind is silent: a half of a pipe held open for a peer
   * that is never coming back.
   */
  webSocketError(ws: Sock): void {
    this.webSocketClose(ws);
  }

  /**
   * The replay set's sweep, and at present the object has no other alarm.
   *
   * Takes no clock, unlike every other method here: the platform calls this one
   * itself, with an `AlarmInvocationInfo` in the first position. A `now`
   * parameter defaulted the way `webSocketMessage`'s is would therefore be
   * handed an *object* in production and arithmetic on it would silently
   * produce `NaN` — so the clock is read here and the sweep, which does take
   * one, is what the tests drive.
   *
   * A second user of the alarm would need a dispatcher: there is one alarm per
   * object, and whoever adds the second must schedule both, not replace this.
   *
   * Unguarded, unlike every storage call on the attach path. A throw here is
   * retried by the platform, which is what housekeeping wants; a throw there
   * would hand a browser a 500 it cannot read.
   */
  async alarm(): Promise<void> {
    await sweepSpentTickets(this.#state.storage, Date.now());
  }

  /**
   * Bytes across, and nothing else.
   *
   * The peer is looked up from the platform **on every message**. Caching it
   * in a field is the hibernation bug ADR-009 names; caching it in an
   * attachment is a storage write per message, which its cost model forbids.
   * The lookup is a tag scan over the sockets this object holds, which is a
   * handful.
   */
  #pump(member: PipeMembership, ws: Sock, message: string | ArrayBuffer, now: number): void {
    const peer = pipePeer(this.#state, member);
    if (peer === null) {
      ws.close(CLOSE_PIPE_PEER_GONE, 'the other end of this pipe is gone');
      this.#budgets.forget(member.tag);
      return;
    }

    if (!this.#budgets.admit(member, messageSize(message), now)) {
      // Both ends, and the flooder is told which it was. The pipe is over
      // either way, and leaving the innocent end open would leave it waiting
      // on a peer that has been hung up on.
      ws.close(CLOSE_PIPE_FLOOD, 'too many messages, or too many bytes, too fast');
      this.#endPipe(member, peer);
      return;
    }

    // Whole, never chunked. `MAX_FRAME` is 8 MiB of plaintext against a 32 MiB
    // edge ceiling, and billing is per message — splitting an 8 MiB scrollback
    // response into 32 would multiply its cost by 32 on exactly the responses a
    // split was meant to protect. → ADR-009.
    try {
      peer.send(message);
    } catch {
      // A peer that is closing but still listed is the same thing to this
      // pipe as a peer that is gone, and it must be *this* -- an uncaught
      // throw on the data path rejects the message handler, which strands the
      // other leg holding a pipe with nothing at the far end.
      this.#endPipe(member, null);
      ws.close(CLOSE_PIPE_PEER_GONE, 'the other end of this pipe is gone');
    }
  }

  /** The survivor is told, and the pipe's buckets go with it. */
  #endPipe(member: PipeMembership, peer: Sock | null): void {
    peer?.close(CLOSE_PIPE_PEER_GONE, 'the other end of this pipe is gone');
    this.#budgets.forget(member.tag);
  }

  /**
   * Park until the host dials for `id`, or until the deadline.
   *
   * The resolver goes in `#dialling` — see that field for why an instance
   * field is legal in this one place and nowhere else. Both outcomes delete
   * their own entry, so a pipe that timed out cannot be claimed afterwards by a
   * dial that finally arrives.
   */
  #dialBack(id: string, timeoutMs: number): Promise<boolean> {
    return new Promise((resolve) => {
      const timer = setTimeout(() => {
        this.#dialling.delete(id);
        resolve(false);
      }, timeoutMs);
      this.#dialling.set(id, () => {
        clearTimeout(timer);
        this.#dialling.delete(id);
        resolve(true);
      });
    });
  }

  /** The error frame, then the hang-up. The frame carries which; the code, how bad. */
  #refuse(ws: Sock, code: ControlErrorCode): void {
    ws.send(errorMessage(code));
    ws.close(closeCodeFor(code), code);
  }
}

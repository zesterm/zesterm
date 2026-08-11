/**
 * A `RelayRoom` under test, with the switch that makes its suites worth
 * writing: **run everything twice**, once against one live object and once
 * against an object thrown away between every single call.
 *
 * ADR-009's rule is that nothing may live in instance fields, because the
 * runtime evicts the object between messages — and the bugs it prevents pass
 * every test written against a single instance. Two of them are known and both
 * were checked by writing them: a challenge nonce in a field fails nine of
 * `control.test.ts`'s sixteen tests in the evicting run and none in the live
 * one, while "is a host present" in a field is the opposite shape and only the
 * `getWebSocketsCalls` assertion catches it.
 *
 * Shared by `control.test.ts` and `pipe.test.ts` rather than copied into each,
 * because the eviction switch is the thing both suites are actually about and
 * two copies of it is two places for one of them to quietly stop evicting.
 */

import assert from 'node:assert/strict';

import { getPublicKeyAsync, signAsync } from '@noble/ed25519';
import { fromHex, hex, signingPreimage } from '@zesterm/cloud-shared';

import type { Env } from '../src/env.ts';
import { RelayRoom } from '../src/room.ts';
import { KEEPALIVE_REQUEST, KEEPALIVE_RESPONSE, NONCE_BYTES } from '../src/room/control.ts';
import { FakeDb, FakePlatform, FakeSock } from './fake-platform.ts';

export const NOW = 1_700_000_000_000;

/** The relay's own key. Its public half is what a daemon is to pin, once one exists. */
export const RELAY_SEED = new Uint8Array(32).fill(3);
export const RELAY_KEY = hex(await getPublicKeyAsync(RELAY_SEED));

/** The machine. Deterministic, so a failure names the same id every run. */
export const HOST_SEED = new Uint8Array(32).fill(11);
export const HOST = hex(await getPublicKeyAsync(HOST_SEED));

/** Another real key pair, for the impostor cases. */
export const STRANGER_SEED = new Uint8Array(32).fill(13);
export const STRANGER = hex(await getPublicKeyAsync(STRANGER_SEED));

/** What `fetch` hands the room; a plain object here, since only the strings matter. */
const KEEPALIVE = { request: KEEPALIVE_REQUEST, response: KEEPALIVE_RESPONSE };

/**
 * A relay under test.
 *
 * `evicting` decides whether every call gets a brand-new `RelayRoom` over the
 * same durable data — sockets, tags, attachments and storage — which is what
 * the runtime leaves behind between messages.
 */
export class Relay {
  readonly platform = new FakePlatform();
  readonly db = new FakeDb();

  readonly #evicting: boolean;
  readonly #env: Env;
  #room: RelayRoom | null = null;

  constructor(evicting: boolean, signingKey: string | undefined) {
    this.#evicting = evicting;
    this.db.enrolled.add(HOST);
    this.#env = {
      RELAY_ROOM: {
        idFromName: () => {
          throw new Error('a room does not address rooms');
        },
        get: () => {
          throw new Error('a room does not address rooms');
        },
      },
      DB: this.db,
      ...(signingKey === undefined ? {} : { RELAY_SIGNING_KEY: signingKey }),
    };
  }

  room(): RelayRoom {
    if (this.#evicting) return new RelayRoom(this.platform.state(), this.#env);
    this.#room ??= new RelayRoom(this.platform.state(), this.#env);
    return this.#room;
  }

  /**
   * Throw the instance away **whatever the mode**, keeping the durable data.
   *
   * For the one property that must hold in both runs: a test that only evicts
   * in the evicting mode is a test whose comment is half true.
   */
  evict(): void {
    this.#room = null;
  }

  async dial(label = 'daemon', args: { host?: string; now?: number } = {}): Promise<FakeSock> {
    const ws = new FakeSock(label);
    await this.room().openControlLink(ws, args.host ?? HOST, KEEPALIVE, args.now ?? NOW);
    return ws;
  }

  async say(ws: FakeSock, message: string | ArrayBuffer, now = NOW): Promise<void> {
    await this.room().webSocketMessage(ws, message, now);
  }

  /**
   * A browser attaching, resolved.
   *
   * `openAttach` awaits the host's dial-back, so this is async where the
   * pre-pipe version was not — and the short `timeoutMs` is what keeps a test
   * that expects no dial-back from waiting out the real deadline.
   */
  async attach(label = 'browser', timeoutMs = 20): Promise<FakeSock> {
    const ws = new FakeSock(label);
    await this.room().openAttach(ws, NOW, timeoutMs);
    return ws;
  }

  /**
   * A whole pipe: a browser attaches, the parked host dials back, both legs
   * come back live.
   *
   * The dial-back runs on the **same instance** as the attach in both modes,
   * and that is not the eviction guard going soft. It is the one moment the
   * platform cannot evict — a Durable Object stays alive while a `fetch` is in
   * flight, the attach *is* one, and `#dialling` is the single instance field
   * whose legality rests on exactly that. Evicting between the two halves would
   * test something production never does. Everything *after* this returns is
   * fair game: `say` and `close` go through `room()`, which evicts.
   */
  async pipe(
    control: FakeSock,
    opts: { client?: string; host?: string; timeoutMs?: number } = {},
  ): Promise<Pipe> {
    const room = this.room();
    const client = new FakeSock(opts.client ?? 'browser');
    // Deliberately not awaited yet: `openAttach` runs to its first `await`
    // synchronously, so by the next line the `open` frame is down the control
    // link and the id is dialable — which is the order the daemon sees.
    const attaching = room.openAttach(client, NOW, opts.timeoutMs ?? 500);
    const id = pipeOf(control);
    const host = new FakeSock(opts.host ?? 'host-leg');
    assert.ok(room.openPipeLeg(host, id), `${host.label} was refused pipe ${id}`);
    await attaching;
    assert.equal(client.closed, null, `${client.label} was closed instead of piped`);
    return { id, client, host, room };
  }

  /** The platform reporting a leg's close, on whatever instance the mode gives. */
  close(ws: FakeSock): void {
    this.room().webSocketClose(ws);
  }

  /** The same news arriving as an error, which the runtime need not follow with a close. */
  fail(ws: FakeSock): void {
    this.room().webSocketError(ws);
  }
}

/** Both ends of one live pipe, and the instance that paired them. */
export interface Pipe {
  readonly id: string;
  readonly client: FakeSock;
  readonly host: FakeSock;
  /**
   * The instance that owns it.
   *
   * Only for the two things that are legitimately instance-scoped: the pending
   * dial (`openPipeLeg`) and the token buckets. Anything on the data path
   * should go through `Relay.say`, which honours the mode.
   */
  readonly room: RelayRoom;
}

/**
 * The pipe id the room most recently published down a control link.
 *
 * The last `open` rather than the first, because a host serves many browsers
 * and a test that read frame zero would silently keep dialling the first pipe
 * it ever opened.
 */
export function pipeOf(control: FakeSock): string {
  const open = frames(control).findLast((frame) => frame['t'] === 'open');
  assert.ok(open, `${control.label} was never asked to open a pipe`);
  return String(open['pipe']);
}

export function frames(ws: FakeSock): Array<Record<string, unknown>> {
  return ws.sent.map((raw) => JSON.parse(String(raw)) as Record<string, unknown>);
}

export function lastFrame(ws: FakeSock): Record<string, unknown> {
  const all = frames(ws);
  assert.ok(all.length > 0, `${ws.label} sent nothing`);
  return all[all.length - 1]!;
}

export function nonceOf(ws: FakeSock): string {
  return String(frames(ws)[0]?.['nonce']);
}

export async function hello(
  nonce: string,
  overrides: { seed?: Uint8Array; host?: string; label?: string; v?: number } = {},
): Promise<string> {
  const nonceBytes = fromHex(nonce, NONCE_BYTES);
  assert.ok(nonceBytes, 'the challenge must carry 64 hex characters');
  const signature = await signAsync(
    signingPreimage('host', 'auth', nonceBytes),
    overrides.seed ?? HOST_SEED,
  );
  return JSON.stringify({
    t: 'hello',
    v: overrides.v ?? 1,
    host: overrides.host ?? HOST,
    label: overrides.label ?? 'andy-mac',
    sig: hex(signature),
  });
}

/** Park an authenticated control link, the ordinary way. */
export async function parked(relay: Relay, label = 'daemon'): Promise<FakeSock> {
  const ws = await relay.dial(label);
  await relay.say(ws, await hello(nonceOf(ws)));
  assert.equal(lastFrame(ws)['t'], 'ready', `${label} failed to park`);
  return ws;
}

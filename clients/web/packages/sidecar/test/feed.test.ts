/**
 * The daemon feed against a scripted daemon: real wire bytes in, actor state
 * out, with the real actors host in the middle.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import * as ed from '@noble/ed25519';
import { sha512 } from '@noble/hashes/sha2.js';
import { createHost, memoryStorage } from '@sigx/actors/host';
import { authTranscript, generateIdentity, preimage } from '@zesterm/auth';
import type { ByteLink, ByteLinkHandlers } from '@zesterm/client';
import { LOCAL_DIRECTORY_KEY, SessionDirectory } from '@zesterm/control';
import { bytesToHex, decode, encode, encodeFrame, FrameReader, hexToBytes } from '@zesterm/proto';

import { startFeed } from '../src/index.ts';

ed.hashes.sha512 = sha512;

const quiet = { sweepIntervalMs: 60_000, reminderTickMs: 60_000, callTimeoutMs: 0 };
const HOST_SEED = '7a'.repeat(32);

/** Just enough daemon to welcome a client and push a listing. */
class ScriptedDaemon {
  link: {
    handlers: ByteLinkHandlers;
    sent: Array<Record<string, unknown>>;
    deliver: (shape: Record<string, unknown>) => void;
  } | null = null;

  readonly identity = generateIdentity(HOST_SEED);

  get dial() {
    return (handlers: ByteLinkHandlers): ByteLink => {
      const frames = new FrameReader();
      const sent: Array<Record<string, unknown>> = [];
      this.link = {
        handlers,
        sent,
        deliver: (shape) => handlers.onMessage(encodeFrame(encode(shape))),
      };
      return {
        send: (bytes) => {
          frames.feed(bytes);
          for (;;) {
            const body = frames.next();
            if (!body) return;
            sent.push(decode(body) as Record<string, unknown>);
          }
        },
        close: () => handlers.onClose(),
      };
    };
  }

  welcome(): void {
    const link = this.link;
    assert.ok(link, 'nothing dialled');
    link.handlers.onOpen();
    const hello = link.sent.find((m) => m['t'] === 'hello');
    assert.ok(hello, 'the feed must say hello');
    assert.equal(hello['watch_sessions'], true, 'the feed exists to watch the list');
    const bytes = authTranscript({
      version: hello['version'] as number,
      host: this.identity.clientId,
      client: hello['client'] as string,
      hostNonce: '9b'.repeat(32),
      clientNonce: hello['nonce'] as string,
      hostLabel: 'scripted',
      clientLabel: hello['label'] as string,
    });
    link.deliver({
      t: 'challenge',
      version: 2,
      host: this.identity.clientId,
      label: 'scripted',
      nonce: '9b'.repeat(32),
      signature: bytesToHex(ed.sign(preimage('host', 'auth', bytes), hexToBytes(HOST_SEED))),
    });
    link.deliver({ t: 'welcome', version: 2, host: this.identity.clientId, label: 'scripted' });
  }

  push(sessions: Array<{ id: number; title: string }>): void {
    this.link?.deliver({
      t: 'sessions',
      sessions: sessions.map((s) => ({
        addr: { host: '2e'.repeat(32), session: s.id },
        title: s.title,
        cwd: '/w',
        cols: 100,
        rows: 30,
        alt_screen: false,
        attached: false,
      })),
    });
  }
}

test('a daemon push lands in the directory, projected to plain JSON', async () => {
  const host = createHost({
    actors: [SessionDirectory],
    storage: memoryStorage(),
    defaults: quiet,
  });
  await host.start();
  const daemon = new ScriptedDaemon();
  const stop = startFeed({
    host,
    dial: daemon.dial,
    dataPlane: { host: '127.0.0.1', port: 7718 },
  });

  try {
    daemon.welcome();
    daemon.push([
      { id: 1, title: 'zsh' },
      { id: 2, title: 'vim' },
    ]);
    // The feed's writes are fire-and-forget actor calls; give the microtask
    // queue one beat.
    await new Promise((r) => setTimeout(r, 10));

    const view = await host.actor(SessionDirectory, LOCAL_DIRECTORY_KEY).list();
    assert.equal(view.connected, true);
    assert.equal(view.dataPlane?.port, 7718, 'browsers learn the data plane from the feed');
    assert.deepEqual(
      view.sessions.map((s) => [s.session, s.title]),
      [
        ['1', 'zsh'],
        ['2', 'vim'],
      ],
      'session ids cross the actor wire as strings, not bigints',
    );

    // The daemon closes one; the next push replaces the listing.
    daemon.push([{ id: 2, title: 'vim' }]);
    await new Promise((r) => setTimeout(r, 10));
    const after = await host.actor(SessionDirectory, LOCAL_DIRECTORY_KEY).list();
    assert.deepEqual(after.sessions.map((s) => s.session), ['2']);

    // The link drops: the directory says so instead of serving ghosts.
    daemon.link?.handlers.onClose();
    await new Promise((r) => setTimeout(r, 10));
    const dropped = await host.actor(SessionDirectory, LOCAL_DIRECTORY_KEY).list();
    assert.equal(dropped.connected, false);
    assert.equal(dropped.sessions.length, 0);
  } finally {
    stop();
    await host.stop();
  }
});

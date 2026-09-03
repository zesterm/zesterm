/**
 * The long-lived per-machine connection: what it asks for, and what it does
 * with the answer.
 *
 * Real bytes throughout — the fake daemon signs a real challenge and the
 * listings are encoded and sealed like a daemon's — so the offer runs the
 * whole decode path (`parseHostOffer` included) rather than being handed to
 * the client as an object it never had to read off the wire (#352).
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import type { HostOffer } from '@zesterm/proto';

import { ConnectionClient } from '../src/index.ts';
import { FakeClock, FakeDaemon, flush, testSigner } from './harness.ts';

const OFFER = {
  os: 'windows',
  arch: 'x86_64',
  os_version: '10.0.26220',
  default_shell: 'pwsh.exe',
  profiles: [
    {
      name: 'Ubuntu',
      command: 'wsl.exe -d Ubuntu',
      starting_directory: '~',
      icon: '',
      color_scheme: '',
      tab_color: 3,
    },
  ],
};

function connect(daemon: FakeDaemon, clock: FakeClock) {
  const offers: HostOffer[] = [];
  const listings: number[] = [];
  const client = new ConnectionClient({
    dial: daemon.dial,
    signer: testSigner(),
    label: 'test',
    clock,
    events: {
      onHostOffer: (offer) => offers.push(offer),
      onSessions: (sessions) => listings.push(sessions.length),
    },
  });
  client.connect();
  return { client, offers, listings };
}

test('the fleet connection asks what the machine offers', async () => {
  // The flag is the whole reason the daemon answers at all: without it the
  // offer is never sent, and every consumer downstream sees null for ever
  // with nothing in the transport to say why.
  const daemon = new FakeDaemon();
  const { client } = connect(daemon, new FakeClock());
  daemon.current.open();
  await flush();
  const hello = daemon.current.lastOfType('hello');
  assert.ok(hello !== undefined, 'the client says hello as soon as the link opens');
  assert.equal(hello?.['watch_hosts'], true);
  assert.equal(hello?.['watch_sessions'], true, 'and still the listing it was built for');
  // Approvals stay loopback authority; a browser asking would be ignored
  // anyway, and asking for something that is refused is a bad habit to have
  // in the one connection every machine holds open.
  assert.equal(hello?.['watch_pairings'], false);
  client.close();
});

test('an offer is delivered once, and not again on the pushes that omit it', async () => {
  // `offer: null` means "nothing new to say", not "no profiles". A consumer
  // that heard about it on every listing would have to re-derive which is
  // which — and the natural reading of a null clears the launcher on every
  // ordinary session push, which is every time anyone opens a shell.
  const daemon = new FakeDaemon();
  const { client, offers, listings } = connect(daemon, new FakeClock());
  await daemon.completeHandshake();

  daemon.current.deliver({ t: 'sessions', sessions: [], created: null, offer: OFFER });
  await flush();
  assert.equal(offers.length, 1);
  assert.equal(offers[0]?.profiles[0]?.command, 'wsl.exe -d Ubuntu');
  assert.equal(offers[0]?.profiles[0]?.tab_color, 3);
  assert.equal(offers[0]?.os_version, '10.0.26220');

  daemon.current.deliver({ t: 'sessions', sessions: [], created: null });
  daemon.current.deliver({ t: 'sessions', sessions: [], created: null, offer: null });
  await flush();
  assert.equal(offers.length, 1, 'an absent offer is silence, not an empty one');
  assert.equal(listings.length, 3, 'while every listing is still a listing');
  client.close();
});

test('a daemon that predates the offer is served exactly as before', async () => {
  // The additive-field rule, from the consumer's side: every field of the
  // offer is optional and the offer itself is absent from an older daemon's
  // `sessions`. That must decode — a message this client cannot read tears
  // the connection down, so a strict parse here would make a new client
  // unable to talk to an old machine at all.
  const daemon = new FakeDaemon();
  const { client, offers, listings } = connect(daemon, new FakeClock());
  await daemon.completeHandshake();

  daemon.current.deliver({ t: 'sessions', sessions: [] });
  await flush();
  assert.equal(listings.length, 1);
  assert.equal(offers.length, 0);

  // And a *new* daemon that cannot answer some of it sends empty strings
  // rather than omitting the fields — but a client must survive either.
  daemon.current.deliver({
    t: 'sessions',
    sessions: [],
    created: null,
    offer: { profiles: [{ name: 'bare' }] },
  });
  await flush();
  assert.equal(offers.length, 1);
  assert.equal(offers[0]?.os, '');
  assert.equal(offers[0]?.profiles[0]?.name, 'bare');
  assert.equal(offers[0]?.profiles[0]?.tab_color, null);
  client.close();
});

test('a search is refused before the welcome and sent after it, and its answer arrives decoded', async () => {
  // Before `welcome` the channel does not exist yet, so a frame written then
  // would be plaintext on a connection about to be sealed; the boolean is
  // what lets a caller count "hosts asked" truthfully.
  const daemon = new FakeDaemon();
  const answers: string[] = [];
  const sessions: (number | null)[] = [];
  const client = new ConnectionClient({
    dial: daemon.dial,
    signer: testSigner(),
    label: 'test',
    clock: new FakeClock(),
    events: {
      onBlockMatches: (reply) => {
        answers.push(reply.query);
        for (const m of reply.matches) sessions.push(m.session);
      },
      onError: (message) => answers.push(`error:${message}`),
    },
  });
  client.connect();
  daemon.current.open();
  await flush();
  assert.equal(client.searchBlocks('cargo', 40), false, 'not before the welcome');
  assert.equal(daemon.current.lastOfType('search_blocks'), undefined);

  await daemon.completeHandshake();
  assert.equal(client.searchBlocks('cargo', 40), true);
  const sent = daemon.current.lastOfType('search_blocks');
  assert.deepEqual(sent, { t: 'search_blocks', query: 'cargo', limit: 40 });

  daemon.current.deliver({
    t: 'block_matches',
    query: 'cargo',
    matches: [
      {
        host: 'ab'.repeat(32),
        session: 7,
        block: 1,
        title: '',
        command: 'cargo build',
        command_truncated: false,
        cwd: '/',
        state: { state: 'finished', exit_code: 0 },
        started_ms: 1,
        ended_ms: 2,
        context: null,
        author: null,
      },
      {
        host: 'ab'.repeat(32),
        session: null,
        block: 2,
        title: '',
        command: 'cargo test',
        command_truncated: false,
        cwd: '/',
        state: { state: 'finished', exit_code: null },
        started_ms: null,
        ended_ms: null,
        context: null,
        author: null,
      },
    ],
    truncated: false,
    sessions: 1,
  });
  await flush();
  assert.deepEqual(answers, ['cargo'], 'the reply reached its own event and not onError');
  assert.deepEqual(sessions, [7, null], 'a stored row keeps its null session through the decode');
  client.close();
});

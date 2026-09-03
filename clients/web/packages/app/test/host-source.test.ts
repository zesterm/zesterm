/**
 * The launcher's answer to "which machines, and how do I reach them".
 *
 * `Shell` asked its *directory* — one machine's session list — so it could
 * only ever name that one machine. The seam is what makes the shell
 * host-plural by construction rather than by a list that happens to hold one,
 * and `localHostSource` has to keep answering exactly as the inlined lookups
 * did or the loopback client changes behaviour for the benefit of the hosted
 * one (#332).
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import type { ClientSigner } from '@zesterm/auth';
import type { ConnectionEvents } from '@zesterm/client';
import type { DirectoryView, HostFacts, SessionEntry } from '@zesterm/control';

import type { DirectorySource, DirectoryStatus } from '../src/directory-source.ts';
import { liveHostSource, localHostSource, type SearchLink } from '../src/host-source.ts';
import type { LiveDirectory } from '../src/live-directory.ts';

const HOST = 'ab'.repeat(32);
const OTHER = 'cd'.repeat(32);

const entry = (host: string, session: string): SessionEntry => ({
  host,
  session,
  title: 'zsh',
  cwd: '/src',
  cols: 80,
  rows: 24,
  altScreen: false,
  attached: false,
  busy: false,
  context: null,
});

const facts = (...names: string[]): HostFacts => ({
  os: 'macos',
  arch: 'aarch64',
  osVersion: '25.5.0',
  defaultShell: '/bin/zsh',
  launchTargets: names.map((name) => ({
    name,
    command: `/bin/${name}`,
    startingDirectory: '',
    icon: '',
    colorScheme: '',
    tabColor: null,
  })),
});

const VIEW: DirectoryView = {
  connected: true,
  host: { id: HOST, label: 'mac' },
  sessions: [],
  dataPlane: { kind: 'ws', host: '127.0.0.1', port: 7718 },
  facts: null,
  lastCreated: null,
};

const WITH_SESSIONS: DirectoryView = {
  ...VIEW,
  sessions: [entry(HOST, '1'), entry(HOST, '2')],
};

/** A reader stuck on one status, which is all the seam reads. */
const reading = (status: DirectoryStatus) => () => status;

/**
 * A signer that would fail loudly if anything reached it.
 *
 * The create tests here *do* call `create`, and they all refuse before a
 * connection is opened — a wrong host id, or a directory that is not ready —
 * so nothing ever signs. Rejecting rather than returning a fake signature is
 * what makes that an assertion rather than an assumption.
 */
const SIGNER: ClientSigner = {
  clientId: 'ff'.repeat(32),
  sign: () => Promise.reject(new Error('the tests never get this far')),
};

/** `localHostSource` with the signer these tests do not use. */
const local = (status: DirectoryStatus) => localHostSource(reading(status), SIGNER);

test('a ready directory offers its own machine, and how to reach it', () => {
  const source = local({ kind: 'ready', view: VIEW });
  assert.deepEqual(source.hosts(), [{ id: HOST, label: 'mac' }]);
  assert.notEqual(source.dialFor(HOST), null, 'and it is dialable');
});

test('a directory that is not ready offers nothing at all', () => {
  // No placeholder row, and no host id that resolves to a dial: the directory
  // is still connecting, and a launcher offering a machine nobody has heard
  // from is offering a click that cannot work.
  for (const status of [
    { kind: 'pending' } as const,
    { kind: 'offline' } as const,
    { kind: 'pairing', code: '123456' } as const,
    { kind: 'error', message: 'nope' } as const,
  ]) {
    const source = local(status);
    assert.deepEqual(source.hosts(), [], `${status.kind} lists nothing`);
    assert.equal(source.dialFor(HOST), null, `${status.kind} dials nothing`);
  }
});

test('a ready directory with no host yet is the same as not ready', () => {
  // `DirectoryView.host` is nullable: the actor exists and the daemon has not
  // said who it is. Reachable on loopback at startup.
  const source = local({ kind: 'ready', view: { ...VIEW, host: null } });
  assert.deepEqual(source.hosts(), []);
  assert.equal(source.dialFor(HOST), null);
});

test('an id the directory does not hold gets no dial', () => {
  // **The failure this exists to prevent**, and it is invisible on loopback:
  // a launcher row a frame behind a directory change would dial *this*
  // machine while naming another. There is only one machine here, so the
  // mistake would show up first on the hosted path — as a session opened on
  // the wrong computer.
  const source = local({ kind: 'ready', view: VIEW });
  assert.equal(source.dialFor(OTHER), null, 'a stale id names nothing rather than the wrong thing');
  assert.equal(source.dialFor(''), null);
});

test('a machine with no dialable plane is listed and not dialable', () => {
  // Two different questions, and the seam answers them separately: "is this
  // one of my machines" and "can I reach it right now". A relay plane with no
  // relay access is exactly that case — the row exists, the click does not.
  const source = local({
    kind: 'ready',
    view: { ...VIEW, dataPlane: { kind: 'relay', hostId: HOST } },
  });
  assert.deepEqual(source.hosts(), [{ id: HOST, label: 'mac' }], 'still your machine');
  assert.equal(source.dialFor(HOST), null, 'but nothing here can reach it');
});

test('the seam re-reads the directory rather than caching it', () => {
  // Built once in setup and read in the render fn — so a directory that
  // becomes ready later must change the answer without the source being
  // rebuilt, or the launcher stays empty for the life of the shell.
  let status: DirectoryStatus = { kind: 'pending' };
  const source = localHostSource(() => status, SIGNER);
  assert.deepEqual(source.hosts(), []);
  status = { kind: 'ready', view: VIEW };
  assert.deepEqual(source.hosts(), [{ id: HOST, label: 'mac' }], 'the next read sees it');
});

test('sessions() is every session the shell knows about', () => {
  // The palette searches this, so it is "the fleet's sessions" rather than
  // "a machine's". On loopback that is one machine's; the seam is what lets
  // the hosted path answer with several without `Shell` changing.
  const source = local({ kind: 'ready', view: WITH_SESSIONS });
  assert.deepEqual(
    source.sessions().map((e) => e.session),
    ['1', '2'],
  );
});

test('sessions() is empty rather than absent while nothing is ready', () => {
  // The palette maps over this every keystroke; an `undefined` here would be a
  // crash in a search box.
  const source = local({ kind: 'pending' });
  assert.deepEqual(source.sessions(), []);
});

test('the empty list is the same list every time, and cannot be mutated', () => {
  // `sessions()` is read by the palette on every keystroke *and* by the route
  // watch. A fresh `[]` per call is an allocation on a hot path and, where a
  // watch compares dependencies by reference, a value never equal to itself —
  // a watch that re-fires forever while a machine is still connecting.
  const source = local({ kind: 'pending' });
  assert.equal(source.sessions(), source.sessions(), 'same reference');

  // And shared, so it has to be immutable: one caller pushing into it would
  // hand every other caller a list that is not empty.
  assert.throws(() => {
    (source.sessions() as SessionEntry[]).push(entry(HOST, '1'));
  });
});

test('find() takes both halves of the pair, and the host half is load-bearing', () => {
  // **A session id is unique to its machine, not across the fleet.** Matching
  // on the id alone opens whichever host answered first — invisible on
  // loopback, where there is one host and it always matches, and on the hosted
  // path it is a URL opening a session on the wrong computer. So the check
  // lives here rather than in whichever caller remembers it.
  const source = local({ kind: 'ready', view: WITH_SESSIONS });
  assert.equal(source.find(HOST, '1')?.session, '1');
  assert.equal(source.find(OTHER, '1'), null, 'right session id, wrong machine');
  assert.equal(source.find(HOST, '9'), null, 'right machine, no such session');
});

test('find() answers null rather than throwing while nothing is ready', () => {
  const source = local({ kind: 'pending' });
  assert.equal(source.find(HOST, '1'), null);
});

test('a local create refuses a machine this source does not hold', () => {
  // The same id check `dialFor` makes. Without it a create aimed at another
  // machine lands on the only one there is — which on loopback *works*, and is
  // exactly why the mistake would ship: it produces a session, just not where
  // the caller asked.
  const source = local({ kind: 'ready', view: VIEW });
  return assert.rejects(
    () => source.create(OTHER, { command: '', cwd: '', cols: 80, rows: 24 }),
    /not dialable/,
    'a create names a machine, and a wrong name is a refusal',
  );
});

test('a local create refuses while nothing is ready', async () => {
  const source = local({ kind: 'pending' });
  await assert.rejects(() => source.create(HOST, { command: '', cwd: '', cols: 80, rows: 24 }));
});

/** Not read by the rules under test; the stub has to return something. */
const PENDING_SOURCE: DirectorySource = () => () => ({ kind: 'pending' });

/**
 * The hosted seam, over a hand-written `LiveDirectory`.
 *
 * Only the four members this source touches — the real one owns sockets,
 * timers and a redial ladder, none of which these rules depend on.
 */
function fakeLive(
  snapshots: {
    id: string;
    label: string;
    online: boolean;
    sessions: readonly SessionEntry[];
    facts?: HostFacts;
  }[],
): LiveDirectory & { created: { hostId: string; size: { cols: number; rows: number } }[] } {
  const created: { hostId: string; size: { cols: number; rows: number } }[] = [];
  return {
    created,
    searchBlocks: () => 0,
    blockSearch: () => ({ query: '', hits: [], hostsAsked: 0, hostsAnswered: 0 }),
    snapshots: () =>
      snapshots.map((s) => ({
        host: { id: s.id, label: s.label },
        presence: s.online ? ({ kind: 'online' } as const) : ({ kind: 'offline' } as const),
        sessions: s.sessions,
        facts: s.facts ?? null,
        lastCreated: null,
      })),
    createSession: (hostId, size) => {
      created.push({ hostId, size });
      const found = snapshots.find((s) => s.id === hostId);
      if (found === undefined || !found.online) {
        return Promise.reject(new Error('that machine is not connected'));
      }
      return Promise.resolve(entry(hostId, 'new'));
    },
    setHosts: () => {},
    sourceFor: () => PENDING_SOURCE,
    statusFor: () => ({ kind: 'pending' }),
    close: () => {},
  };
}

const RELAY = { origin: 'https://relay.example', mintTicket: () => Promise.resolve('t') };

test('the hosted seam lists every machine, asleep ones included', () => {
  // #334's rule, and the reason it is not "the machines you can reach": hiding
  // a sleeping machine would make the fleet appear to shrink whenever the
  // network hiccuped. Over the relay most of a fleet is asleep most of the
  // time — that is the ordinary case, not a fault.
  const live = fakeLive([
    { id: HOST, label: 'mac', online: true, sessions: [] },
    { id: OTHER, label: 'pi', online: false, sessions: [] },
  ]);
  assert.deepEqual(liveHostSource(live, RELAY).hosts(), [
    { id: HOST, label: 'mac' },
    { id: OTHER, label: 'pi' },
  ]);
});

test('the hosted seam dials only a machine that is answering', () => {
  // A *row* for a sleeping machine is honest; a *dial* to one is a click that
  // hangs until it times out, which is the affordance rule inverted.
  const live = fakeLive([
    { id: HOST, label: 'mac', online: true, sessions: [] },
    { id: OTHER, label: 'pi', online: false, sessions: [] },
  ]);
  const source = liveHostSource(live, RELAY);
  assert.notEqual(source.dialFor(HOST), null);
  assert.equal(source.dialFor(OTHER), null, 'asleep is not dialable');
  assert.equal(source.dialFor('ee'.repeat(32)), null, 'and neither is unknown');
});

test('the hosted seam cannot dial without relay access', () => {
  // Every hosted dial is a relay dial — mixed content rules out the LAN
  // entirely — so a deployment with no relay reaches nothing, and says so
  // rather than producing a `Dial` that cannot mint a ticket.
  const live = fakeLive([{ id: HOST, label: 'mac', online: true, sessions: [] }]);
  assert.equal(liveHostSource(live, null).dialFor(HOST), null);
});

test('the hosted seam finds a session by host AND id, across machines', () => {
  // The case that cannot exist on loopback: two machines, both with a session
  // called `1`. Matching on the id alone opens whichever answered first.
  const live = fakeLive([
    { id: HOST, label: 'mac', online: true, sessions: [entry(HOST, '1')] },
    { id: OTHER, label: 'pi', online: true, sessions: [entry(OTHER, '1')] },
  ]);
  const source = liveHostSource(live, RELAY);
  assert.equal(source.find(HOST, '1')?.host, HOST);
  assert.equal(source.find(OTHER, '1')?.host, OTHER);
  assert.equal(source.find(HOST, '2'), null);
  assert.deepEqual(
    source.sessions().map((e) => e.host),
    [HOST, OTHER],
    'and sessions() is every machine’s, which is what the palette searches',
  );
});

test('a hosted create goes through the connection already watching that machine', async () => {
  // The whole point of putting `create` on the seam. Going through `dialFor`
  // here would mint a second relay pipe, a second ticket and a second
  // handshake, then discard all of it — per create, for a machine the browser
  // is already talking to.
  const live = fakeLive([{ id: HOST, label: 'mac', online: true, sessions: [] }]);
  const created = await liveHostSource(live, RELAY).create(HOST, { command: '', cwd: '', cols: 80, rows: 24 });
  assert.deepEqual(live.created, [{ hostId: HOST, size: { command: '', cwd: '', cols: 80, rows: 24 } }]);
  assert.equal(created.session, 'new');
});

test('a hosted create on a sleeping machine rejects rather than hanging', async () => {
  // A promise that never settles is a launcher row that spins for ever.
  const live = fakeLive([{ id: OTHER, label: 'pi', online: false, sessions: [] }]);
  await assert.rejects(() => liveHostSource(live, RELAY).create(OTHER, { command: '', cwd: '', cols: 80, rows: 24 }));
});

test('the hosted seam hands back the same session list until it really changes', () => {
  // `LiveDirectory.snapshots()` builds a fresh array on every call by design,
  // so flattening it naively hands back a new array each time — and
  // `routeWatch` depends on this value. Where a watch compares dependencies by
  // reference that is one which is never equal to itself: it re-fires on every
  // tick, for ever, and only on the hosted path.
  const one = entry(HOST, '1');
  const snapshots = [{ id: HOST, label: 'mac', online: true, sessions: [one] }];
  const live = fakeLive(snapshots);
  const source = liveHostSource(live, RELAY);

  const first = source.sessions();
  assert.equal(source.sessions(), first, 'same list while nothing has changed');

  // A machine gaining a session is a change, and must be seen.
  snapshots[0]!.sessions = [one, entry(HOST, '2')];
  const second = source.sessions();
  assert.notEqual(second, first, 'a new session is a new list');
  assert.equal(second.length, 2);
  assert.equal(source.sessions(), second, 'and then stable again');

  // …and so is losing one, which a length-only check would miss in the
  // other direction.
  snapshots[0]!.sessions = [one];
  const third = source.sessions();
  assert.equal(third.length, 1);
  assert.notEqual(third, second);
});

test('an empty fleet is the shared empty list, on the hosted path too', () => {
  const live = fakeLive([{ id: HOST, label: 'mac', online: true, sessions: [] }]);
  const source = liveHostSource(live, RELAY);
  assert.equal(source.sessions(), source.sessions());
  assert.throws(() => {
    (source.sessions() as SessionEntry[]).push(entry(HOST, '1'));
  }, 'and it is frozen, because it is shared with every other empty source');
});

test('a created entry names the machine it was asked for', () => {
  // The shell dials `entry.host` rather than the id it asked about, so the tab
  // and its connection agree by construction. This pins the other half: a
  // source must not answer a create with an entry belonging to some other
  // machine, or the tab would be right and the *request* would have been
  // ignored.
  const live = fakeLive([{ id: HOST, label: 'mac', online: true, sessions: [] }]);
  return liveHostSource(live, RELAY)
    .create(HOST, { command: '', cwd: '', cols: 80, rows: 24 })
    .then((created) => {
      assert.equal(created.host, HOST);
    });
});

test('a machine that has said nothing is not a machine with nothing to launch', () => {
  // Null and `[]` are different answers and the launcher owes them different
  // rows: an older daemon, or one this shell has not reached, says nothing;
  // a machine with an empty profile table says so. Collapsing the two draws
  // "we cannot reach it" as "it has nothing".
  assert.equal(local({ kind: 'ready', view: VIEW }).factsOf(HOST), null);
  assert.deepEqual(
    local({ kind: 'ready', view: { ...VIEW, facts: facts() } }).factsOf(HOST)?.launchTargets,
    [],
  );
  assert.equal(local({ kind: 'pending' }).factsOf(HOST), null);
});

test('facts are answered for the machine that published them and no other', () => {
  // The loopback source holds one machine, so an id check here can never fail
  // — which is exactly why it has to be written rather than left to the one
  // caller who remembers. The mistake it prevents lands on the hosted path.
  const source = local({ kind: 'ready', view: { ...VIEW, facts: facts('wsl') } });
  assert.equal(source.factsOf(HOST)?.launchTargets[0]?.name, 'wsl');
  assert.equal(source.factsOf(OTHER), null);
});

test('each machine answers with its own launch targets', () => {
  const live = fakeLive([
    { id: HOST, label: 'mac', online: true, sessions: [], facts: facts('zsh') },
    { id: OTHER, label: 'forge', online: true, sessions: [], facts: facts('wsl', 'pwsh') },
  ]);
  const source = liveHostSource(live, RELAY);
  assert.deepEqual(
    source.factsOf(OTHER)?.launchTargets.map((t) => t.name),
    ['wsl', 'pwsh'],
  );
  assert.deepEqual(
    source.factsOf(HOST)?.launchTargets.map((t) => t.name),
    ['zsh'],
  );
  // A machine the account does not hold is null, not the first one's.
  assert.equal(source.factsOf('ef'.repeat(32)), null);
});

test('an offline machine still lists, and still says what it offers', () => {
  // Two questions with different answers, the rule #334 settled: a machine
  // whose relay is unreachable is still yours. Its facts are the last thing
  // it said, and `LiveDirectory` — not this seam — is what clears them when
  // the link goes, so a source that dropped them here would disagree with
  // the store it reads from.
  const live = fakeLive([
    { id: HOST, label: 'mac', online: false, sessions: [], facts: facts('zsh') },
  ]);
  const source = liveHostSource(live, RELAY);
  assert.deepEqual(source.hosts(), [{ id: HOST, label: 'mac' }]);
  assert.equal(source.dialFor(HOST), null);
  assert.equal(source.factsOf(HOST)?.launchTargets.length, 1);
});

test('every session the pane can list is a session the seam can dial', () => {
  // The invariant the fleet pane rests on, and the one #376 broke: it draws a
  // row per session and asks the seam how to reach it. The pane used to answer
  // that itself, from a `DataPlane` plus an optional relay, so a caller with
  // no relay to give produced a full list in which nothing could be clicked.
  //
  // Written over the entries rather than over the machines because that is the
  // shape the pane walks — a row resolves on `entry.host`, never on the
  // machine the pane was opened for.
  const live = fakeLive([
    { id: HOST, label: 'mac', online: true, sessions: [entry(HOST, '1'), entry(HOST, '2')] },
    { id: OTHER, label: 'forge', online: true, sessions: [entry(OTHER, '9')] },
  ]);
  const source = liveHostSource(live, RELAY);
  for (const e of source.sessions()) {
    assert.notEqual(
      source.dialFor(e.host),
      null,
      `session ${e.session} is listed on ${e.host} and must be clickable`,
    );
  }

  // And the converse, which is what makes the row honestly disabled rather
  // than a click that hangs: a machine that stopped answering keeps no dial.
  const asleep = liveHostSource(
    fakeLive([{ id: HOST, label: 'mac', online: false, sessions: [entry(HOST, '1')] }]),
    RELAY,
  );
  assert.equal(asleep.dialFor(HOST), null);
});

test('the loopback pane can dial the sessions it lists too', () => {
  // Same invariant on the path that was never broken — worth pinning because
  // the fix routes BOTH panes through the seam, and a loopback regression
  // would otherwise only show up in a browser.
  const source = local({ kind: 'ready', view: WITH_SESSIONS });
  for (const e of source.sessions()) {
    assert.notEqual(source.dialFor(e.host), null, `session ${e.session} must be clickable`);
  }
});

/** The loopback search connection, as a test sees it. */
class FakeSearchLink implements SearchLink {
  connects = 0;
  closes = 0;
  connected = false;
  readonly searches: Array<{ query: string; limit: number }> = [];
  connect(): void {
    this.connects += 1;
  }
  close(): void {
    this.closes += 1;
  }
  searchBlocks(query: string, limit: number): boolean {
    if (!this.connected) return false;
    this.searches.push({ query, limit });
    return true;
  }
}

test('the loopback search opens nothing until the directory is ready, then one connection for every query', () => {
  const opened: FakeSearchLink[] = [];
  const open = () => {
    const link = new FakeSearchLink();
    opened.push(link);
    return link;
  };

  const notReady = localHostSource(reading({ kind: 'pending' }), SIGNER, null, open);
  assert.equal(notReady.searchBlocks('x'), 0, 'nothing to ask yet');
  assert.equal(opened.length, 0, 'and nothing was dialled to find that out');

  const source = localHostSource(reading({ kind: 'ready', view: VIEW }), SIGNER, null, open);
  assert.equal(source.searchBlocks('ca'), 0, 'asked before the handshake: the frame did not go out');
  assert.equal(opened.length, 1, 'the connection is opened on the first question');
  assert.equal(opened[0]?.connects, 1);

  opened[0]!.connected = true;
  assert.equal(source.searchBlocks('cargo'), 1, 'and counts once it is up');
  assert.equal(source.searchBlocks('cargo b'), 1);
  assert.equal(opened.length, 1, 'one connection across many queries — never a handshake per keystroke');
  assert.deepEqual(
    opened[0]?.searches.map((s) => s.query),
    ['cargo', 'cargo b'],
  );
  assert.equal(source.blockSearch().hostsAsked, 1);

  source.close();
  assert.equal(opened[0]?.closes, 1, 'the shell’s unmount closes what the source opened');
});

test('a question asked before the welcome is sent when the welcome lands', () => {
  // ⌘K asks for the newest blocks on the way in, and on loopback that very
  // call opens the connection — so the frame cannot go out yet. Without a
  // re-send the palette shows no fleet rows until the person types.
  const opened: FakeSearchLink[] = [];
  const captured: { events: ConnectionEvents | null } = { events: null };
  const source = localHostSource(reading({ kind: 'ready', view: VIEW }), SIGNER, null, (_dial, ev) => {
    captured.events = ev;
    const link = new FakeSearchLink();
    opened.push(link);
    return link;
  });
  assert.equal(source.searchBlocks(''), 0, 'the frame could not go out yet');
  assert.equal(source.blockSearch().hostsAsked, 0);
  const events = captured.events;
  assert.ok(events, 'the connection was opened');

  opened[0]!.connected = true;
  events.onConnection?.({ phase: 'connected' });
  assert.deepEqual(opened[0]?.searches, [{ query: '', limit: 40 }], 'the held question went out on the welcome');
  assert.equal(source.blockSearch().hostsAsked, 1, 'and the count says so');

  // A later question replaces the held one; nothing is re-sent twice.
  events.onConnection?.({ phase: 'connected' });
  assert.equal(opened[0]?.searches.length, 1, 'a second welcome has nothing held');
});

/**
 * The hosted directory's supervision, driven with no network and no crypto.
 *
 * The seam taken is `OpenLink`, and that choice is the point: `ConnectionClient`
 * already runs against a scripted daemon in its own package, so re-proving its
 * handshake here would test that package twice and this file not at all. What
 * *is* only here is the lifecycle — when a machine is called asleep, how often
 * it is asked again, whose connection a create rides, and what happens to a
 * list when its link goes away — and every test below is one of those.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import type { Clock, ConnectionEvents, TimerHandle } from '@zesterm/client';
import { REDIAL_MAX_MS } from '@zesterm/client';
import type { SessionInfo } from '@zesterm/proto';

import {
  CREATE_TIMEOUT_MS,
  ladderElapsedMs,
  liveDirectory,
  PROBE_MAX_MS,
  PROBE_MIN_MS,
  REACH_TIMEOUT_MS,
  type DirectoryLink,
  type LiveHost,
  type OpenLink,
} from '../src/live-directory.ts';

const MAC: LiveHost = { id: 'ab'.repeat(32), label: 'andy-mac' };
const PC: LiveHost = { id: 'cd'.repeat(32), label: 'andy-pc' };

/** Time the tests advance by hand, so a 30 s probe costs no wall clock. */
class FakeClock implements Clock {
  #now = 1_000_000;
  #timers = new Map<number, { at: number; fn: () => void }>();
  #next = 1;

  now(): number {
    return this.#now;
  }

  schedule(fn: () => void, ms: number): TimerHandle {
    const id = this.#next++;
    this.#timers.set(id, { at: this.#now + ms, fn });
    return id;
  }

  cancel(handle: TimerHandle): void {
    this.#timers.delete(handle as number);
  }

  advance(ms: number): void {
    const target = this.#now + ms;
    for (;;) {
      const due = [...this.#timers.entries()]
        .filter(([, t]) => t.at <= target)
        .sort((a, b) => a[1].at - b[1].at)[0];
      if (due === undefined) break;
      this.#timers.delete(due[0]);
      this.#now = due[1].at;
      due[1].fn();
    }
    this.#now = target;
  }

  get pending(): number {
    return this.#timers.size;
  }

  /** How long until the next timer fires — the probe interval, asserted directly. */
  nextIn(): number | undefined {
    const waits = [...this.#timers.values()].map((t) => t.at - this.#now);
    return waits.length === 0 ? undefined : Math.min(...waits);
  }
}

/** A connection the test plays both ends of. */
class FakeLink implements DirectoryLink {
  connects = 0;
  closes = 0;
  readonly creates: Array<{ command: string; cwd: string; cols: number; rows: number }> = [];
  readonly events: ConnectionEvents;

  constructor(events: ConnectionEvents) {
    this.events = events;
  }

  connect(): void {
    this.connects += 1;
    this.events.onConnection?.({ phase: 'connecting' });
  }

  close(): void {
    this.closes += 1;
  }

  createSession(opts: { command: string; cwd: string; cols: number; rows: number }): void {
    // The whole spec, not just the size: what a create *runs* is now the
    // launcher's business, and a harness that dropped it would let the
    // command be lost between here and the daemon without a test noticing.
    this.creates.push({ ...opts });
  }

  /** The full handshake outcome, as `ConnectionClient` reports it. */
  welcome(): void {
    this.events.onConnection?.({ phase: 'connected' });
  }

  drop(attempt = 1): void {
    this.events.onConnection?.({ phase: 'reconnecting', attempt });
  }

  sessions(infos: readonly SessionInfo[], created: bigint | null = null): void {
    this.events.onSessions?.(infos, created);
  }

  /**
   * The offer, on its own — which is how it arrives. `ConnectionClient` fires
   * it only when a `Sessions` carried one, so a test that could not push a
   * listing without an offer could not reproduce the sticky case at all.
   */
  offer(...profiles: string[]): void {
    this.events.onHostOffer?.({
      os: 'linux',
      arch: 'x86_64',
      os_version: '6.8.0',
      default_shell: '/bin/bash',
      profiles: profiles.map((name) => ({
        name,
        command: `/bin/${name}`,
        starting_directory: '',
        icon: '',
        color_scheme: '',
        tab_color: null,
      })),
    });
  }
}

/** Every link handed out, in order, per machine. */
class Links {
  readonly all: FakeLink[] = [];
  readonly openLink: OpenLink;
  #byHost = new Map<string, FakeLink[]>();

  constructor(open?: OpenLink) {
    this.openLink =
      open ??
      ((host, events) => {
        const link = new FakeLink(events);
        this.all.push(link);
        const list = this.#byHost.get(host.id) ?? [];
        list.push(link);
        this.#byHost.set(host.id, list);
        return link;
      });
  }

  for(hostId: string): FakeLink[] {
    return this.#byHost.get(hostId) ?? [];
  }

  current(hostId: string): FakeLink {
    const link = this.for(hostId).at(-1);
    if (link === undefined) throw new Error('nothing dialled that machine');
    return link;
  }
}

function info(session: bigint, title = 'shell'): SessionInfo {
  return {
    addr: { host: MAC.id, session },
    title,
    cwd: '/home/andy',
    cols: 120,
    rows: 32,
    alt_screen: false,
    attached: false,
  };
}

function harness(open?: OpenLink) {
  const clock = new FakeClock();
  const links = new Links(open);
  const directory = liveDirectory({ openLink: links.openLink, clock });
  return { clock, links, directory };
}

// --- the deadline, and the arithmetic it rests on ---------------------------

test('the reach timeout outlives the whole redial ladder, so a restarting daemon is waited for', () => {
  // Five retries is where `ConnectionClient`'s 200 ms → 5 s ladder hits its
  // ceiling; past that it repeats at 5 s for ever. The window has to cover the
  // first and stop inside the second, or "asleep" means either "did not answer
  // in a blink" or "never".
  assert.ok(
    REACH_TIMEOUT_MS > ladderElapsedMs(5),
    `a machine must get the whole ladder (${ladderElapsedMs(5)}ms) before it is called asleep`,
  );
  assert.ok(
    REACH_TIMEOUT_MS < ladderElapsedMs(5) + REDIAL_MAX_MS,
    'and must be stood down before the ladder starts repeating at its ceiling',
  );
});

test('a machine that never answers becomes asleep, not an error, and its link is closed', () => {
  const { clock, links, directory } = harness();
  directory.setHosts([MAC]);

  assert.equal(directory.statusFor(MAC.id).kind, 'pending', 'it is connecting until it is not');

  // The relay accepts the socket and closes it 4404; from here that is simply
  // a handshake that never completes, however many times the ladder retries.
  clock.advance(REACH_TIMEOUT_MS);

  assert.equal(
    directory.statusFor(MAC.id).kind,
    'offline',
    'a laptop with its lid shut is the common case and must not read as a fault',
  );
  assert.equal(
    links.current(MAC.id).closes,
    1,
    'the ladder is stopped, not left redialling: over the relay every redial is a ticket mint and a room wake-up',
  );
});

test('an asleep machine is probed on a widening interval, never in a tight loop', () => {
  const { clock, links, directory } = harness();
  directory.setHosts([MAC]);
  clock.advance(REACH_TIMEOUT_MS);

  assert.equal(clock.nextIn(), PROBE_MIN_MS, 'the first probe waits the floor, not the ladder');

  clock.advance(PROBE_MIN_MS);
  assert.equal(links.for(MAC.id).length, 2, 'a probe is a fresh connection, not a resumed one');
  assert.equal(directory.statusFor(MAC.id).kind, 'pending', 'and the row says it is trying again');

  clock.advance(REACH_TIMEOUT_MS);
  assert.equal(
    clock.nextIn(),
    PROBE_MIN_MS * 2,
    'a machine that stays absent is asked less often, or ten sleeping laptops are a permanent load on the relay',
  );
});

test('the probe interval stops widening at its ceiling, so a woken machine still appears', () => {
  const { clock, links, directory } = harness();
  directory.setHosts([MAC]);

  // Enough failed probes to run the doubling past the ceiling.
  for (let i = 0; i < 12; i++) {
    clock.advance(REACH_TIMEOUT_MS);
    const wait = clock.nextIn();
    assert.ok(wait !== undefined && wait <= PROBE_MAX_MS, 'no wait may exceed the ceiling');
    clock.advance(wait);
  }
  assert.ok(links.for(MAC.id).length >= 12, 'it keeps checking; it just checks slowly');
});

test('answering resets the budget, so a machine that sleeps again gets the full ladder back', () => {
  const { clock, links, directory } = harness();
  directory.setHosts([MAC]);

  // Asleep, then asleep again: the wait has doubled.
  clock.advance(REACH_TIMEOUT_MS);
  clock.advance(PROBE_MIN_MS);
  clock.advance(REACH_TIMEOUT_MS);
  assert.equal(clock.nextIn(), PROBE_MIN_MS * 2, 'precondition: the wait has widened');

  // Now it wakes and answers.
  clock.advance(PROBE_MIN_MS * 2);
  links.current(MAC.id).welcome();
  assert.equal(directory.statusFor(MAC.id).kind, 'ready', 'precondition: it is online');

  links.current(MAC.id).drop();
  clock.advance(REACH_TIMEOUT_MS);
  assert.equal(
    clock.nextIn(),
    PROBE_MIN_MS,
    'having answered is what earns the short interval back — otherwise a machine that woke once is punished for the hours it was asleep',
  );
});

test('a link that drops after being online is given the ladder again before it is stood down', () => {
  const { clock, links, directory } = harness();
  directory.setHosts([MAC]);
  const link = links.current(MAC.id);
  link.welcome();

  link.drop();
  assert.equal(directory.statusFor(MAC.id).kind, 'pending', 'a drop is reconnecting, not asleep');
  clock.advance(REACH_TIMEOUT_MS - 1);
  assert.equal(
    directory.statusFor(MAC.id).kind,
    'pending',
    'a daemon restarting must be waited for, not stood down after one dropped socket',
  );
  clock.advance(1);
  assert.equal(directory.statusFor(MAC.id).kind, 'offline', 'and stood down once the ladder is spent');
});

// --- what the list actually says --------------------------------------------

test('sessions arrive as directory entries, projected the way the sidecar projects them', () => {
  const { links, directory } = harness();
  directory.setHosts([MAC]);
  const link = links.current(MAC.id);
  link.welcome();
  link.sessions([info(7n, 'vim')]);

  const status = directory.statusFor(MAC.id);
  assert.equal(status.kind, 'ready');
  const view = status.kind === 'ready' ? status.view : null;
  assert.deepEqual(
    view?.sessions[0],
    {
      host: MAC.id,
      session: '7',
      title: 'vim',
      cwd: '/home/andy',
      cols: 120,
      rows: 32,
      altScreen: false,
      attached: false,
    },
    'a bigint session id reaches the UI as a string, as it does on loopback — the two must agree or a row opens the wrong session',
  );
  assert.deepEqual(
    view?.dataPlane,
    { kind: 'relay', hostId: MAC.id },
    'a hosted page may not open a ws:// address on someone else’s LAN, so the plane is always the relay',
  );
});

test('a dropped link takes its session list with it', () => {
  const { links, directory } = harness();
  directory.setHosts([MAC]);
  const link = links.current(MAC.id);
  link.welcome();
  link.sessions([info(7n)]);
  link.drop();

  const status = directory.statusFor(MAC.id);
  assert.equal(status.kind, 'pending');
  // Proven by going back online with an empty listing being impossible to tell
  // from a stale one otherwise: what is asserted is that the entries are gone
  // the moment the link is.
  link.welcome();
  const online = directory.statusFor(MAC.id);
  assert.deepEqual(
    online.kind === 'ready' ? online.view.sessions : null,
    [],
    'a list from before the drop, shown under a live banner, is a list that offers sessions that may not exist',
  );
});

test('a pairing request is surfaced with its code, not swallowed as “connecting”', () => {
  const { links, directory } = harness();
  directory.setHosts([MAC]);
  links.current(MAC.id).events.onConnection?.({ phase: 'awaiting-approval', code: '4821' });

  const status = directory.statusFor(MAC.id);
  assert.equal(
    status.kind,
    'pairing',
    'enrolling a browser with the account does not make its key trusted by a daemon; the first attach stops here',
  );
  assert.equal(
    status.kind === 'pairing' ? status.code : null,
    '4821',
    'the code is the entire instruction — a wait without it is unexplained',
  );
});

test('a machine waiting for approval is not stood down as asleep', () => {
  const { clock, links, directory } = harness();
  directory.setHosts([MAC]);
  links.current(MAC.id).events.onConnection?.({ phase: 'awaiting-approval', code: '4821' });
  clock.advance(REACH_TIMEOUT_MS * 4);

  assert.equal(
    directory.statusFor(MAC.id).kind,
    'pairing',
    'someone is walking to that machine to read a code; a timeout would cancel the thing they were asked to do',
  );
});

test('a refusal that will not change is an error and is not probed again', () => {
  const { clock, links, directory } = harness();
  directory.setHosts([MAC]);
  links.current(MAC.id).events.onConnection?.({
    phase: 'failed',
    reason: 'denied',
    message: 'this device was denied',
  });

  const status = directory.statusFor(MAC.id);
  assert.equal(status.kind, 'error', 'a denied device is not a sleeping machine');
  assert.equal(status.kind === 'error' ? status.message : null, 'this device was denied');

  clock.advance(PROBE_MAX_MS * 4);
  assert.equal(
    links.for(MAC.id).length,
    1,
    'hammering a host that said no is how the rate limiter ends up punishing the user',
  );
});

test('no relay means every machine says so, once, rather than dialling nothing for ever', () => {
  const { clock, directory } = harness(() => null);
  directory.setHosts([MAC]);

  const status = directory.statusFor(MAC.id);
  assert.equal(status.kind, 'error');
  assert.match(
    status.kind === 'error' ? status.message : '',
    /relay/,
    'a deployment with no relay is a configuration, and the message has to name it or the row is a mystery',
  );
  clock.advance(PROBE_MAX_MS * 4);
  assert.equal(clock.pending, 0, 'and nothing is retried: no amount of waiting configures a relay');
});

// --- the host set ------------------------------------------------------------

test('a second registry read does not open a second pipe to every machine', () => {
  const { links, directory } = harness();
  directory.setHosts([MAC, PC]);
  directory.setHosts([MAC, PC]);

  assert.equal(links.for(MAC.id).length, 1, 'the fleet screen refetches /api/hosts after a revoke');
  assert.equal(links.for(PC.id).length, 1);
});

test('a revoked machine has its connection closed and leaves the store', () => {
  const { links, directory } = harness();
  directory.setHosts([MAC, PC]);
  const dropped = links.current(PC.id);
  directory.setHosts([MAC]);

  assert.equal(dropped.closes, 1, 'a machine that is no longer in the account is not one to hold a pipe to');
  assert.equal(
    directory.statusFor(PC.id).kind,
    'error',
    'and it is gone from the store rather than frozen at its last state',
  );
  assert.deepEqual(
    directory.snapshots().map((s) => s.host.id),
    [MAC.id],
    'the listing follows the account, in the order the account gave',
  );
});

test('closing the screen closes every connection and leaves no timer running', () => {
  const { clock, links, directory } = harness();
  directory.setHosts([MAC, PC]);
  links.current(MAC.id).welcome();
  directory.close();

  assert.equal(links.current(MAC.id).closes, 1);
  assert.equal(links.current(PC.id).closes, 1);
  assert.equal(
    clock.pending,
    0,
    'a fleet screen left behind a route change would otherwise keep probing the whole account',
  );
});

// --- create rides the connection that is already there -----------------------

test('creating a session reuses the watching connection', async () => {
  const { links, directory } = harness();
  directory.setHosts([MAC]);
  const link = links.current(MAC.id);
  link.welcome();

  const created = directory.createSession(MAC.id, { command: '', cwd: '', cols: 120, rows: 32 });
  assert.equal(
    links.for(MAC.id).length,
    1,
    'over the relay a second connection is a second ticket, a second pipe and a second handshake for one message',
  );
  assert.deepEqual(link.creates, [{ command: '', cwd: '', cols: 120, rows: 32 }]);

  link.sessions([info(9n)], 9n);
  const entry = await created;
  assert.equal(entry.session, '9', 'the create resolves from the listing that answers it');
});

test('a create the daemon answers with an error is rejected, not left to time out', async () => {
  const { clock, links, directory } = harness();
  directory.setHosts([MAC]);
  const link = links.current(MAC.id);
  link.welcome();

  const created = directory.createSession(MAC.id, { command: '', cwd: '', cols: 80, rows: 24 });
  link.events.onError?.('the shell could not be spawned');
  // Past the give-up timer on purpose. A swallowed error would otherwise leave
  // this promise unsettled and hang the suite rather than fail it — and the
  // assertion that matters is *which* message came back, since the timeout
  // would eventually reject too and prove nothing.
  clock.advance(CREATE_TIMEOUT_MS * 2);
  await assert.rejects(
    created,
    /could not be spawned/,
    'a daemon that says why must not be turned into a fifteen-second silence',
  );
});

test('a create on a machine that is not connected is refused at once', async () => {
  const { directory } = harness();
  directory.setHosts([MAC]);
  await assert.rejects(
    directory.createSession(MAC.id, { command: '', cwd: '', cols: 80, rows: 24 }),
    /not connected/,
    'there is no connection to write it down, and queueing it would start a session at an unknown time',
  );
});

test('a create still in flight when the link drops is rejected rather than left pending', async () => {
  const { links, directory } = harness();
  directory.setHosts([MAC]);
  const link = links.current(MAC.id);
  link.welcome();

  const created = directory.createSession(MAC.id, { command: '', cwd: '', cols: 80, rows: 24 });
  link.drop();
  await assert.rejects(created, /dropped/, 'the connection that was going to answer it is gone');
});

test('a create nobody answers gives up on the clock', async () => {
  const { clock, links, directory } = harness();
  directory.setHosts([MAC]);
  links.current(MAC.id).welcome();

  const created = directory.createSession(MAC.id, { command: '', cwd: '', cols: 80, rows: 24 });
  clock.advance(REACH_TIMEOUT_MS * 3);
  await assert.rejects(created, /in time/, 'a promise nothing settles is a button that stays busy for ever');
});

// --- what a machine offers ---------------------------------------------------

test('the offer survives every session push that does not carry one', () => {
  // The daemon sends the offer on the first listing and again only when its
  // config reloads; every ordinary session push omits it. Clearing on a push
  // with nothing new to say would empty the launcher the moment anyone opened
  // a shell, which is the failure this stickiness exists to prevent.
  const { links, directory } = harness();
  directory.setHosts([MAC]);
  const link = links.current(MAC.id);
  link.welcome();
  link.offer('wsl', 'pwsh');
  assert.deepEqual(
    directory.snapshots()[0]?.facts?.launchTargets.map((t) => t.name),
    ['wsl', 'pwsh'],
  );

  link.sessions([info(1n)]);
  assert.deepEqual(
    directory.snapshots()[0]?.facts?.launchTargets.map((t) => t.name),
    ['wsl', 'pwsh'],
    'a listing with no offer means nothing changed about the machine',
  );

  // And a reload replaces it wholesale rather than merging — the daemon's
  // list is the truth, and a merge would resurrect a deleted profile.
  link.offer('wsl');
  assert.deepEqual(
    directory.snapshots()[0]?.facts?.launchTargets.map((t) => t.name),
    ['wsl'],
  );
});

test('a machine that stops answering stops offering', () => {
  // Stronger than the rule for sessions: a stale session row is a listing that
  // may be wrong, while a stale launch target is a row that MUST fail when
  // pressed. The next connection republishes on its first listing, so this
  // costs a blank menu for as long as the machine is actually gone.
  const { clock, links, directory } = harness();
  directory.setHosts([MAC]);
  const link = links.current(MAC.id);
  link.welcome();
  link.offer('wsl');

  link.drop();
  assert.equal(directory.snapshots()[0]?.facts, null, 'a dropped link publishes nothing');

  clock.advance(REACH_TIMEOUT_MS);
  assert.equal(directory.snapshots()[0]?.presence.kind, 'offline');
  assert.equal(directory.snapshots()[0]?.facts, null);

  // Back up: the fresh connection's own offer is what fills it again.
  clock.advance(clock.nextIn() ?? 0);
  const second = links.current(MAC.id);
  second.welcome();
  second.offer('pwsh');
  assert.deepEqual(
    directory.snapshots()[0]?.facts?.launchTargets.map((t) => t.name),
    ['pwsh'],
  );
});

test('each machine keeps its own offer', () => {
  // One store, two machines: the offer is keyed by the connection that carried
  // it, so a second machine's reload cannot overwrite the first's targets.
  const { links, directory } = harness();
  directory.setHosts([MAC, PC]);
  links.current(MAC.id).welcome();
  links.current(MAC.id).offer('zsh');
  links.current(PC.id).welcome();
  links.current(PC.id).offer('wsl', 'pwsh');

  const byId = new Map(directory.snapshots().map((s) => [s.host.id, s]));
  assert.deepEqual(byId.get(MAC.id)?.facts?.launchTargets.map((t) => t.name), ['zsh']);
  assert.deepEqual(byId.get(PC.id)?.facts?.launchTargets.map((t) => t.name), ['wsl', 'pwsh']);
});

test('a create carries the profile it was asked for, verbatim', () => {
  // What the machine published, sent back to it. The daemon resolved the
  // profile through its own defaults before publishing (ADR-014), so anything
  // this client rewrote on the way would be a second, worse resolution.
  const { links, directory } = harness();
  directory.setHosts([MAC]);
  const link = links.current(MAC.id);
  link.welcome();

  void directory.createSession(MAC.id, {
    command: 'wsl.exe -d Ubuntu',
    cwd: '/home/andy',
    cols: 143,
    rows: 41,
  });
  assert.deepEqual(link.creates, [
    { command: 'wsl.exe -d Ubuntu', cwd: '/home/andy', cols: 143, rows: 41 },
  ]);
});

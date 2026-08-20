/**
 * The session list — the mockup's screen 1, minimum viable cut.
 *
 * The mechanism is a `DirectorySource`, created in this component's setup and
 * read in its render fn. On loopback that source is one live actor read: the
 * sidecar's feed writes the directory and the read re-runs after every turn
 * that mutated it, whoever caused it — a session created from the desktop
 * shows up here without this page doing anything. On the hosted path it is a
 * connection this tab holds to one machine. `directory-source.ts` says why the
 * source is a seam rather than either read written out here.
 *
 * **This component still cannot tell which world it is in**, and that is the
 * property worth keeping. What it knows is a `DataPlane` and how to turn one
 * into a `Dial` — `dial-for.ts`, one function, both planes — so opening a
 * session is the same code whether the socket goes to `127.0.0.1` or through
 * the relay. It asks for a `Dial` rather than a URL for exactly that reason: a
 * relay plane never becomes a URL at all, because a ticket has to be minted
 * first.
 */

import { component, signal } from 'sigx';
import type { Dial } from '@zesterm/client';
import { type DirectoryView, type SessionEntry } from '@zesterm/control';

import { sessionRows, tabIdOf } from '../chrome-model.ts';
import { runExclusive } from '../busy-guard.ts';
import { describeDeviceKey, type DeviceKeyKind } from '../device-key.ts';
import type { DirectorySource, DirectoryStatus } from '../directory-source.ts';

export interface OpenTarget {
  readonly entry: SessionEntry;
  /** Reusable: `SessionClient` calls it again on every redial. */
  readonly dial: Dial;
}

export const SessionList = component<{
  /** Where the list comes from; see `directory-source.ts`. */
  source: DirectorySource;
  /**
   * How to reach one machine, by id — `HostSource.dialFor`, handed straight
   * down.
   *
   * **Required, and not a `RelayAccess` this component turns into a dial.**
   * It used to derive its own from `view.dataPlane` plus an *optional* relay
   * prop, which meant a caller could build a working-looking list whose every
   * row was disabled: forgetting the relay and correctly having none (loopback)
   * are the same call. That is exactly what happened when the hosted path moved
   * into `Shell` — every session became unclickable and nothing in the build
   * or the suite objected (#376).
   *
   * One place knows how to reach a machine, it is already tested, and being
   * required makes the compiler the guard that a suite with no DOM cannot be.
   */
  dialFor: (hostId: string) => Dial | null;
  /** The heading. The hosted path names the machine; loopback has only one. */
  title?: string;
  /** What "still connecting" is called. Loopback is reaching its sidecar. */
  connectingLabel?: string;
  /**
   * Which kind of key this device signs with, said out loud.
   *
   * A browser that fell back to the seed is working, not secure, and the two
   * look identical from here — a screen that showed the same reassurance
   * either way would be lying on exactly the device where it matters.
   */
  deviceKind?: DeviceKeyKind;
  onOpen?: (target: OpenTarget) => void;
  /**
   * Start a session. The whole view, because the two worlds start one
   * differently: loopback opens a short-lived second connection, and the
   * hosted path writes down the connection already watching the machine —
   * over the relay a second connection is a second ticket and a second pipe.
   *
   * Return the promise if you have one: it is what keeps the button held for
   * as long as the create is actually running. See `create` below.
   */
  onCreate?: (view: DirectoryView) => void | Promise<unknown>;
}>((ctx) => {
  const directory = ctx.props.source();
  const creating = signal({ busy: false });

  /**
   * `null` is "not reachable from here", which is what disables the create
   * button: a machine that is asleep, or a directory that has not yet said
   * *which* machine it is.
   *
   * That second case used to leave the button live, and `Shell`'s handler had
   * to answer it with an error message. A button that cannot work is better
   * disabled than apologetic.
   */
  const dialOf = (view: DirectoryView): Dial | null =>
    view.host === null ? null : ctx.props.dialFor(view.host.id);

  /**
   * Held until the create *settles*, not until the call returns.
   *
   * Creating a session is asynchronous everywhere — a round trip to the daemon
   * on loopback, a ticket and a pipe over the relay — so a flag cleared in a
   * `finally` on the synchronous call guards nothing at all: the second click
   * lands while the first is still in flight and the account gets two shells it
   * did not ask for. That is why `onCreate` may return a promise; a handler
   * that returns nothing is taken at its word and unlocks at once.
   */
  const create = (view: DirectoryView): void => {
    if (dialOf(view) === null) return;
    runExclusive(creating, () => ctx.props.onCreate?.(view));
  };

  const linkState = (status: DirectoryStatus) => {
    switch (status.kind) {
      case 'pending':
        return (
          <span class="link-state">{ctx.props.connectingLabel ?? 'reaching the sidecar…'}</span>
        );
      case 'pairing':
        // The code is the whole message: it has to be compared against what
        // the machine is showing, and a banner without it is a wait with no
        // instructions.
        return (
          <span class="link-state degraded">
            waiting for approval on the machine — compare code {status.code}
          </span>
        );
      case 'offline':
        // Not `degraded`: a machine with its lid shut is the ordinary state of
        // most of a fleet, and a screen that paints it as a fault is one
        // people stop reading.
        return <span class="link-state asleep">asleep — nothing is parked at the relay</span>;
      case 'error':
        return <span class="link-state degraded">{status.message}</span>;
      case 'ready':
        return status.view.connected ? (
          <span class="link-state ok">daemon connected</span>
        ) : (
          // The directory said so itself rather than going quiet — the
          // honest banner the mockup's degraded states call for.
          <span class="link-state degraded">daemon unreachable — reconnecting</span>
        );
    }
  };

  const sessions = (status: DirectoryStatus) => {
    switch (status.kind) {
      case 'pending':
        return <p class="empty">loading…</p>;
      case 'pairing':
        return <p class="empty">this browser has not been approved on that machine yet</p>;
      case 'offline':
        return <p class="empty">nothing to show until this machine wakes up</p>;
      case 'error':
        // The status carries what actually went wrong, and the two worlds fail
        // differently — a sidecar that is not answering, versus a relay that
        // refused this browser. One fixed sentence is right for at most one of
        // them and misleading for the other.
        return <p class="empty">{status.message}</p>;
      case 'ready': {
        const view = status.view;
        return (
          <>
            <ul class="sessions">
              {view.sessions.length === 0 ? (
                <li class="empty">
                  {view.connected ? 'no sessions — start one' : 'no sessions to show'}
                </li>
              ) : (
                // Which row is clickable, and which machine it dials, are
                // `sessionRows`' two decisions — pure, and tested, because a
                // rule living in this markup is one no test here can reach.
                sessionRows(view.sessions, ctx.props.dialFor).map(({ entry: s, dial }) => (
                  // The machine AND the session. A session id is unique to its
                  // own daemon and not across the fleet, so keying on it alone
                  // would collide the moment this pane showed two machines —
                  // and `tabIdOf` is the pair every other surface already
                  // identifies a session by.
                  <li key={tabIdOf(s.host, s.session)}>
                    <button
                      class="session-row"
                      disabled={dial === null}
                      onClick={() => dial !== null && ctx.props.onOpen?.({ entry: s, dial })}
                    >
                      <span class={`state-dot ${s.altScreen ? 'alt' : 'shell'}`} />
                      <span class="title">{s.title === '' ? 'shell' : s.title}</span>
                      <span class="meta">
                        {s.cwd} · {s.cols}×{s.rows}
                        {s.attached ? ' · attached' : ''}
                      </span>
                    </button>
                  </li>
                ))
              )}
            </ul>
            <footer>
              <button
                class="create"
                disabled={!view.connected || dialOf(view) === null}
                onClick={() => create(view)}
              >
                new session
              </button>
            </footer>
          </>
        );
      }
    }
  };

  return () => {
    const status = directory();
    return (
      <main class="session-list">
        <header>
          <h1>{ctx.props.title ?? 'zesterm'}</h1>
          {linkState(status)}
          {ctx.props.deviceKind === undefined ? null : (
            <span class="key-state">{describeDeviceKey(ctx.props.deviceKind)}</span>
          )}
        </header>

        {sessions(status)}
      </main>
    );
  };
});

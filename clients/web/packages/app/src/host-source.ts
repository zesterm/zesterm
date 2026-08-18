/**
 * Which machines this window can launch on, and how to reach each.
 *
 * The shell asked its *directory* — one machine's session list — for the
 * answer, so it could only ever name that one machine:
 *
 * ```ts
 * if (dir.kind === 'ready' && dir.view.host !== null) return [{ … }];
 * return [];
 * ```
 *
 * That is right on loopback and structurally wrong everywhere else: a
 * directory describes *a* host (see `directory-source.ts` on why the hosted
 * half kept it that way), while "what can I launch on" is a question about the
 * fleet. Two questions, one shape, and the launcher got the narrower answer.
 *
 * **A source is created in a component's setup and read in its render fn**,
 * for the reason `directory-source.ts` sets out at length: sigx registers a
 * live read with the instance that calls it, so a source made by a parent and
 * handed down as an already-made reader would move the subscription's lifetime
 * to that parent. This seam is a plain object rather than a function because
 * it holds no live read of its own — but the *directory* it is built from is
 * one, so `localHostSource` takes the reader and must be called where that
 * reader already lives.
 */

import type { Dial } from '@zesterm/client';

import type { SessionEntry } from '@zesterm/control';

import type { HostChoice } from './chrome-model.ts';
import { dialFor, type RelayAccess } from './dial-for.ts';
import type { DirectoryReader } from './directory-source.ts';

/**
 * One empty list, shared and frozen.
 *
 * `sessions()` is read by the palette on every keystroke *and* by the route
 * watch, so a fresh `[]` per call is an allocation on a hot path — and, where
 * a watch compares its dependencies by reference, a value that is never equal
 * to itself. Whether sigx does that is not something this file should have to
 * know: a stable empty makes the question moot rather than answered.
 *
 * Frozen because it is shared: one caller mutating it would hand every other
 * caller a list that is not empty.
 */
const NO_SESSIONS: readonly SessionEntry[] = Object.freeze([]);

/**
 * The machines a shell shows, and whether each can be reached right now.
 *
 * **Two questions, deliberately not one.** "Is this one of my machines" and
 * "can I dial it this second" have different answers and different lifetimes:
 * a machine whose relay is unreachable is still yours, and hiding its row
 * would make the fleet appear to shrink whenever the network hiccuped. So
 * `hosts()` lists it and `dialFor` answers `null`, and the caller disables the
 * row rather than dropping it — the native app's rule for fleet cards, where
 * `open` is `best_route(h).is_some()` and the card is drawn either way.
 *
 * What that rules out is the row that *must fail*: a listed machine whose
 * click does nothing silently. `dialFor` returning `null` is a source saying
 * "show it, do not let them press it".
 */
export interface HostSource {
  /**
   * Every machine this shell knows about, in a stable order — including ones
   * nothing can currently reach. Reachability is [`HostSource.dialFor`]'s
   * question, not this one's.
   */
  hosts(): readonly HostChoice[];
  /**
   * How to reach one, or `null` when nothing can right now.
   *
   * By id rather than by index, because the caller holds an id from a row that
   * may be a frame behind the list — an index would silently name its
   * neighbour.
   */
  dialFor(hostId: string): Dial | null;
  /**
   * Every session on every machine this shell knows about.
   *
   * The palette searches this, so it is "the fleet's sessions" rather than
   * "a machine's" — the same widening the native app's ⌘K got when it started
   * watching more than one host (#265).
   */
  sessions(): readonly SessionEntry[];
  /**
   * One session, by the pair that names it.
   *
   * **Both halves, always.** A session id is unique to its machine and not
   * across the fleet, so matching on the id alone would open whichever host
   * answered first. That is invisible on loopback — one machine, so the host
   * always matches — and on the hosted path it is a URL opening a session on
   * the wrong computer.
   */
  find(hostId: string, sessionId: string): SessionEntry | null;
}

/**
 * The loopback path: one machine, the directory's own.
 *
 * Exactly today's behaviour, extracted rather than changed — `Shell` asked the
 * directory these three questions inline, and this is the same answers behind
 * a seam the hosted path can implement differently.
 */
export function localHostSource(
  directory: DirectoryReader,
  relay: RelayAccess | null = null,
): HostSource {
  const own = (): { id: string; label: string; dial: Dial | null } | null => {
    const dir = directory();
    if (dir.kind !== 'ready' || dir.view.host === null) return null;
    return {
      id: dir.view.host.id,
      label: dir.view.host.label,
      dial: dialFor(dir.view.dataPlane, relay),
    };
  };
  return {
    hosts: () => {
      const host = own();
      // No host is an empty list, never a placeholder row: the directory is
      // still connecting, and a launcher offering a machine nobody has heard
      // from yet is offering a click that cannot work.
      return host === null ? [] : [{ id: host.id, label: host.label }];
    },
    dialFor: (hostId) => {
      const host = own();
      // The id has to match. A launcher row a frame behind a directory change
      // would otherwise dial *this* machine while naming another — which on
      // loopback is the only machine there is, so the mistake would be
      // invisible here and land on the hosted path instead.
      return host !== null && host.id === hostId ? host.dial : null;
    },
    sessions: () => {
      const dir = directory();
      // The actor's own array while ready — stable between turns, so a
      // reference comparison sees no change until the list really changes.
      return dir.kind === 'ready' ? dir.view.sessions : NO_SESSIONS;
    },
    find: (hostId, sessionId) => {
      const dir = directory();
      if (dir.kind !== 'ready') return null;
      // Host *and* session. On loopback the host check can never fail, which
      // is exactly why it has to be written here rather than left to the one
      // caller who remembers.
      return (
        dir.view.sessions.find((e) => e.host === hostId && e.session === sessionId) ?? null
      );
    },
  };
}

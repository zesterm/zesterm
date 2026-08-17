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

import type { HostChoice } from './chrome-model.ts';
import { dialFor, type RelayAccess } from './dial-for.ts';
import type { DirectoryReader } from './directory-source.ts';

/**
 * The machines a shell can launch on.
 *
 * Deliberately not "the fleet": a machine the account lists but nothing can
 * dial is not a launch target, and a row that must fail is worse than no row —
 * the same rule the native app's fleet cards follow. `dialFor` returning
 * `null` is how a source says so.
 */
export interface HostSource {
  /** Every machine that can be launched on, in a stable order. */
  hosts(): readonly HostChoice[];
  /**
   * How to reach one, or `null` when nothing can right now.
   *
   * By id rather than by index, because the caller holds an id from a row that
   * may be a frame behind the list — an index would silently name its
   * neighbour.
   */
  dialFor(hostId: string): Dial | null;
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
  };
}

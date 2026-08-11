/**
 * The one place a `DataPlane` becomes a `Dial`.
 *
 * Beside `dataPlaneUrl` rather than folded into it: a `ws` plane is a URL and
 * dialling it is one more step, but a `relay` plane never becomes a URL at all
 * — it needs a ticket first — so the two questions have different answers and
 * only one of them is "what string do I open?".
 */

import type { Dial } from '@zesterm/client';
import type { DataPlane } from '@zesterm/control';

import { dataPlaneUrl } from './data-plane-url.ts';
import { relayDial } from './relay-dial.ts';
import { wsDial } from './ws-dial.ts';

/**
 * What a browser needs before it can dial through the edge: where the relay
 * is, and how to get a ticket for it.
 *
 * `null` is the ordinary state, not an error — a page that has not learned its
 * relay origin, or a build with no relay at all, has a fleet it can still
 * reach over `ws`.
 */
export interface RelayAccess {
  /** The relay Worker's origin, e.g. `https://relay.example.com`. */
  readonly origin: string;
  /** Mints a short-lived attach ticket; see `relay-dial.ts` for why injected. */
  readonly mintTicket: (hostId: string) => Promise<string>;
}

/**
 * `null` means "not dialable from here", which is the same answer
 * `dataPlaneUrl` gives and the same one every caller already handles: rows are
 * disabled for it today.
 */
export function dialFor(dataPlane: DataPlane | null, relay: RelayAccess | null): Dial | null {
  if (dataPlane === null) return null;
  switch (dataPlane.kind) {
    case 'ws': {
      // Routed through `dataPlaneUrl` so the daemon's address is spelled in
      // one file. It cannot return null for a `ws` plane; the check is the
      // compiler's price for not re-deriving the URL here.
      const url = dataPlaneUrl(dataPlane);
      return url === null ? null : wsDial(url);
    }
    case 'relay': {
      const hostId = dataPlane.hostId;
      return relay === null
        ? null
        : relayDial(relay.origin, hostId, () => relay.mintTicket(hostId));
    }
    default: {
      // A third kind must not fall through to something that dials anyway —
      // this makes adding one a compile error here, which is where the
      // question belongs.
      //
      // The binding is the guard; the *return* is `null` and not the binding,
      // for the reason `data-plane-url.ts` spells out at length: a kind the
      // compiler never saw can still arrive at runtime from a newer sidecar,
      // and handing it back through a `Dial | null` signature would pass every
      // caller's `=== null` check and then be *called*.
      const unreachable: never = dataPlane;
      void unreachable;
      return null;
    }
  }
}

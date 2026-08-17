/**
 * Create a session over the data plane, one-shot.
 *
 * Session mutations deliberately do not ride the actors socket (ADR-005:
 * actors are the control plane, and create/close are ordering-sensitive
 * daemon operations). A short-lived data-plane connection creates the
 * session; the daemon's `sessions` push then updates the directory for
 * everyone, this tab included — the daemon reconciles, so the race between
 * "created" and "listed" resolves in the daemon's favour by construction.
 */

import type { ClientSigner } from '@zesterm/auth';
import { ConnectionClient, type Dial } from '@zesterm/client';
import type { SessionAddr } from '@zesterm/proto';

/**
 * A `Dial`, not a `ws://` URL.
 *
 * It took a URL and called `wsDial` itself, which meant it could **only** ever
 * create over `ws` — and the hosted client cannot use the LAN at all (mixed
 * content), so every create in the cloud world goes through the relay,
 * including one to the machine the browser is sitting on. `dialFor` already
 * answers "what dials this plane" for both kinds; this path predates it and
 * never learned. (#332)
 */
export function createSessionOverDataPlane(args: {
  dial: Dial;
  signer: ClientSigner;
  cols: number;
  rows: number;
}): Promise<SessionAddr> {
  const { dial, signer, cols, rows } = args;
  return new Promise((resolve, reject) => {
    // `let`, and never dereferenced without a check. The timer is armed
    // before the client exists, so a constructor that throws leaves this
    // closure holding a binding in its temporal dead zone — the throw rejects
    // the promise *and* leaves the timer running, and fifteen seconds later a
    // `ReferenceError` surfaces from a timer callback with nothing to catch
    // it. Rare, and very hard to read: the failure arrives long after its
    // cause, on a promise that settled ages ago.
    let client: ConnectionClient | null = null;
    const timeout = setTimeout(() => {
      client?.close();
      reject(new Error('the daemon did not answer the create in time'));
    }, 15_000);
    const settle = (): void => {
      clearTimeout(timeout);
      client?.close();
    };

    try {
      client = new ConnectionClient({
        dial,
        signer,
        label: 'zesterm-web',
        events: {
          onConnection: (state) => {
            if (state.phase === 'connected') {
              client?.createSession({ command: '', cwd: '', cols, rows });
            } else if (state.phase === 'failed') {
              settle();
              reject(new Error(state.message));
            }
          },
          onSessions: (sessions, created) => {
            if (created === null) return;
            const mine = sessions.find((s) => s.addr.session === created);
            settle();
            if (mine) resolve(mine.addr);
            else reject(new Error('the created session was not in the listing'));
          },
          onError: (message) => {
            settle();
            reject(new Error(message));
          },
        },
      });
    } catch (e: unknown) {
      // Disarm before rejecting, or the timer fires into a settled promise.
      clearTimeout(timeout);
      reject(e instanceof Error ? e : new Error(String(e)));
      return;
    }
    client.connect();
  });
}

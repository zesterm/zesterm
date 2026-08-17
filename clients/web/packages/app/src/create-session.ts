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
    const timeout = setTimeout(() => {
      client.close();
      reject(new Error('the daemon did not answer the create in time'));
    }, 15_000);

    const client: ConnectionClient = new ConnectionClient({
      dial,
      signer,
      label: 'zesterm-web',
      events: {
        onConnection: (state) => {
          if (state.phase === 'connected') {
            client.createSession({ command: '', cwd: '', cols, rows });
          } else if (state.phase === 'failed') {
            clearTimeout(timeout);
            client.close();
            reject(new Error(state.message));
          }
        },
        onSessions: (sessions, created) => {
          if (created === null) return;
          const mine = sessions.find((s) => s.addr.session === created);
          clearTimeout(timeout);
          client.close();
          if (mine) resolve(mine.addr);
          else reject(new Error('the created session was not in the listing'));
        },
        onError: (message) => {
          clearTimeout(timeout);
          client.close();
          reject(new Error(message));
        },
      },
    });
    client.connect();
  });
}

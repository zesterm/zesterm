/**
 * The one writer the directory actor has.
 *
 * A `ConnectionClient` on the daemon's loopback socket — full handshake with
 * an ephemeral identity (loopback authorizes by reaching the socket, same
 * posture as the desktop app), `watch_sessions: true`, and every push
 * projected into `SessionDirectory` via **in-process** `host.actor(...)`.
 * Nothing here touches the actors socket, and nothing here ever carries a
 * grid byte: M4's rule that the sidecar stays out of the PTY hot path is a
 * property of what this file imports.
 */

import type { Host } from '@sigx/actors/host';
import { generateIdentity, seedSigner } from '@zesterm/auth';
import { ConnectionClient, type Dial } from '@zesterm/client';
import { LOCAL_DIRECTORY_KEY, SessionDirectory, sessionEntryOf, type DataPlane } from '@zesterm/control';

export interface FeedOptions {
  readonly host: Host;
  readonly dial: Dial;
  /**
   * Advertised to browsers through the directory — the daemon's WS port.
   * Always `kind: 'ws'`: the sidecar *is* the local path, and a browser that
   * needs the relay never reached one of these.
   */
  readonly dataPlane: DataPlane;
  readonly label?: string;
  readonly onLog?: (line: string) => void;
}

/** Start feeding. Returns a stop function. */
export function startFeed(options: FeedOptions): () => void {
  const { host, dial, dataPlane } = options;
  const log = options.onLog ?? (() => {});
  const directory = host.actor(SessionDirectory, LOCAL_DIRECTORY_KEY);

  const client = new ConnectionClient({
    dial,
    // Ephemeral by design: the sidecar is not a device, and it should not
    // accumulate a pairing on every machine it runs on. Loopback's
    // AlwaysTrusted store welcomes it; the proof still runs.
    signer: seedSigner(generateIdentity()),
    label: options.label ?? 'zesterm-sidecar',
    events: {
      onSessions: (sessions, created) => {
        void directory.replaceAll(
          sessions.map(sessionEntryOf),
          created === null ? null : created.toString(),
        );
      },
      onConnection: (state) => {
        log(`daemon link: ${state.phase}`);
        if (state.phase === 'connected') {
          // Host identity arrives with the welcome; the ConnectionClient
          // does not surface it in v1, so the directory carries the data
          // plane and the connected bit — which is what the UI needs first.
          void directory.setLink(true, null, dataPlane);
        } else if (state.phase === 'reconnecting' || state.phase === 'failed') {
          void directory.setLink(false, null, null);
        }
      },
      onError: (message) => log(`daemon error: ${message}`),
    },
  });
  client.connect();
  return () => client.close();
}

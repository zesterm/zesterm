/**
 * The session list — the mockup's screen 1, minimum viable cut.
 *
 * One live actor read is the entire mechanism: the sidecar's feed writes the
 * directory, and `{ live: true }` re-runs this read after every turn that
 * mutated it, whoever caused it — a session created from the desktop shows
 * up here without this page doing anything.
 */

import { component, signal } from 'sigx';
import { useActorState } from '@sigx/actors/app';
import {
  LOCAL_DIRECTORY_KEY,
  SessionDirectory,
  type DirectoryView,
  type SessionEntry,
} from '@zesterm/control';

import { dataPlaneUrl } from '../data-plane-url.ts';
import { describeDeviceKey, type DeviceKeyKind } from '../device-key.ts';

export interface OpenTarget {
  readonly entry: SessionEntry;
  readonly dataPlaneUrl: string;
}

export const SessionList = component<{
  /**
   * Which kind of key this device signs with, said out loud.
   *
   * A browser that fell back to the seed is working, not secure, and the two
   * look identical from here — a screen that showed the same reassurance
   * either way would be lying on exactly the device where it matters.
   */
  deviceKind?: DeviceKeyKind;
  onOpen?: (target: OpenTarget) => void;
  onCreate?: (dataPlaneUrl: string) => void;
}>((ctx) => {
  const directory = useActorState(SessionDirectory, LOCAL_DIRECTORY_KEY, 'list', { live: true });
  const creating = signal({ busy: false });

  const urlOf = (view: DirectoryView): string | null => dataPlaneUrl(view.dataPlane);

  const create = (view: DirectoryView): void => {
    const url = urlOf(view);
    if (url === null || creating.busy) return;
    creating.busy = true;
    try {
      ctx.props.onCreate?.(url);
    } finally {
      creating.busy = false;
    }
  };

  return () => (
    <main class="session-list">
      <header>
        <h1>zesterm</h1>
        {directory.match({
          pending: () => <span class="link-state">reaching the sidecar…</span>,
          error: (e) => <span class="link-state degraded">{e.message}</span>,
          ready: (view: DirectoryView) =>
            view.connected ? (
              <span class="link-state ok">daemon connected</span>
            ) : (
              // The directory said so itself rather than going quiet — the
              // honest banner the mockup's degraded states call for.
              <span class="link-state degraded">daemon unreachable — reconnecting</span>
            ),
        })}
        {ctx.props.deviceKind === undefined ? null : (
          <span class="key-state">{describeDeviceKey(ctx.props.deviceKind)}</span>
        )}
      </header>

      {directory.match({
        pending: () => <p class="empty">loading…</p>,
        error: () => <p class="empty">the control plane is not answering</p>,
        ready: (view: DirectoryView) => (
          <>
            <ul class="sessions">
              {view.sessions.length === 0 ? (
                <li class="empty">
                  {view.connected ? 'no sessions — start one' : 'no sessions to show'}
                </li>
              ) : (
                view.sessions.map((s) => {
                  const url = urlOf(view);
                  return (
                    <li>
                      <button
                        class="session-row"
                        disabled={url === null}
                        onClick={() =>
                          url !== null && ctx.props.onOpen?.({ entry: s, dataPlaneUrl: url })
                        }
                      >
                        <span class={`state-dot ${s.altScreen ? 'alt' : 'shell'}`} />
                        <span class="title">{s.title === '' ? 'shell' : s.title}</span>
                        <span class="meta">
                          {s.cwd} · {s.cols}×{s.rows}
                          {s.attached ? ' · attached' : ''}
                        </span>
                      </button>
                    </li>
                  );
                })
              )}
            </ul>
            <footer>
              <button
                class="create"
                disabled={!view.connected || urlOf(view) === null}
                onClick={() => create(view)}
              >
                new session
              </button>
            </footer>
          </>
        ),
      })}
    </main>
  );
});

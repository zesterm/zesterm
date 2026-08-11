/**
 * The session list — the mockup's screen 1, minimum viable cut.
 *
 * The mechanism is a `DirectorySource`, created in this component's setup and
 * read in its render fn. On loopback that source is one live actor read: the
 * sidecar's feed writes the directory and the read re-runs after every turn
 * that mutated it, whoever caused it — a session created from the desktop
 * shows up here without this page doing anything. `directory-source.ts` says
 * why the source is a seam rather than that read written out here.
 */

import { component, signal } from 'sigx';
import { type DirectoryView, type SessionEntry } from '@zesterm/control';

import { dataPlaneUrl } from '../data-plane-url.ts';
import { describeDeviceKey, type DeviceKeyKind } from '../device-key.ts';
import type { DirectorySource, DirectoryStatus } from '../directory-source.ts';

export interface OpenTarget {
  readonly entry: SessionEntry;
  readonly dataPlaneUrl: string;
}

export const SessionList = component<{
  /** Where the list comes from; see `directory-source.ts`. */
  source: DirectorySource;
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
  const directory = ctx.props.source();
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

  const linkState = (status: DirectoryStatus) => {
    switch (status.kind) {
      case 'pending':
        return <span class="link-state">reaching the sidecar…</span>;
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
      case 'error':
        return <p class="empty">the control plane is not answering</p>;
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
        );
      }
    }
  };

  return () => {
    const status = directory();
    return (
      <main class="session-list">
        <header>
          <h1>zesterm</h1>
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

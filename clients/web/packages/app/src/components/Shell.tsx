/**
 * The app shell: which screen, and the client-side-only state the mockup
 * enumerates — nothing else lives here, because everything else is server
 * state arriving over one of the two planes.
 */

import { component, signal } from 'sigx';
import type { Theme } from '@zesterm/theme';

import { createSessionOverDataPlane } from '../create-session.ts';
import type { DeviceKey } from '../device-key.ts';
import { actorDirectorySource } from '../directory-source.ts';
import { SessionList, type OpenTarget } from './SessionList.tsx';
import { TerminalView } from './TerminalView.tsx';

type View = { kind: 'list' } | { kind: 'terminal'; target: OpenTarget };

export const Shell = component<{ device: DeviceKey; theme: Theme }>((ctx) => {
  const { device, theme } = ctx.props;
  const view = signal<{ current: View; error: string | null }>({
    current: { kind: 'list' },
    error: null,
  });

  const create = (dataPlaneUrl: string): void => {
    createSessionOverDataPlane({ url: dataPlaneUrl, signer: device.signer, cols: 120, rows: 32 })
      .then((addr) => {
        view.error = null;
        view.current = {
          kind: 'terminal',
          target: {
            dataPlaneUrl,
            entry: {
              host: addr.host,
              session: addr.session.toString(),
              title: '',
              cwd: '',
              cols: 120,
              rows: 32,
              altScreen: false,
              attached: false,
            },
          },
        };
      })
      .catch((e: unknown) => {
        view.error = e instanceof Error ? e.message : String(e);
      });
  };

  return () => (
    <div class="shell">
      {view.error !== null ? <div class="shell-error">{view.error}</div> : null}
      {view.current.kind === 'list' ? (
        <SessionList
          // The actor-backed source, because this shell is only ever reached
          // on the loopback path — the sidecar is what hosts the actor.
          source={actorDirectorySource}
          deviceKind={device.kind}
          onOpen={(target: OpenTarget) => (view.current = { kind: 'terminal', target })}
          onCreate={create}
        />
      ) : (
        <TerminalView
          entry={view.current.target.entry}
          dataPlaneUrl={view.current.target.dataPlaneUrl}
          signer={device.signer}
          theme={theme}
          onBack={() => (view.current = { kind: 'list' })}
        />
      )}
    </div>
  );
});

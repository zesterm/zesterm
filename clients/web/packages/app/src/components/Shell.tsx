/**
 * The app shell: which screen, and the client-side-only state the mockup
 * enumerates — nothing else lives here, because everything else is server
 * state arriving over one of the two planes.
 */

import { component, signal } from 'sigx';
import type { ClientIdentity } from '@zesterm/auth';
import type { Theme } from '@zesterm/theme';

import { createSessionOverDataPlane } from '../create-session.ts';
import { SessionList, type OpenTarget } from './SessionList.tsx';
import { TerminalView } from './TerminalView.tsx';

type View = { kind: 'list' } | { kind: 'terminal'; target: OpenTarget };

export const Shell = component<{ identity: ClientIdentity; theme: Theme }>((ctx) => {
  const { identity, theme } = ctx.props;
  const view = signal<{ current: View; error: string | null }>({
    current: { kind: 'list' },
    error: null,
  });

  const create = (dataPlaneUrl: string): void => {
    createSessionOverDataPlane({ url: dataPlaneUrl, identity, cols: 120, rows: 32 })
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
          onOpen={(target: OpenTarget) => (view.current = { kind: 'terminal', target })}
          onCreate={create}
        />
      ) : (
        <TerminalView
          entry={view.current.target.entry}
          dataPlaneUrl={view.current.target.dataPlaneUrl}
          identity={identity}
          theme={theme}
          onBack={() => (view.current = { kind: 'list' })}
        />
      )}
    </div>
  );
});

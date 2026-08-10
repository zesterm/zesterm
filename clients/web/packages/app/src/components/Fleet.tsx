/**
 * What you see once you are signed in.
 *
 * On the **local** path this is the real session list, unchanged: the sidecar
 * hosts the `SessionDirectory` actor and the daemon is a `ws://` away.
 *
 * On the **hosted** path it cannot be, and says so. Two independent reasons,
 * and naming both matters because fixing either alone changes nothing: there
 * is no sidecar at the edge to host the actor (ADR-005), and mixed content
 * forbids an https page from opening `ws://<lan-ip>:7718` at all — even to the
 * machine the browser is running on. Rendering the real list would spin on a
 * socket that cannot connect and read as a broken daemon.
 */

import { component } from 'sigx';
import type { Theme } from '@zesterm/theme';

import type { Bootstrap, User } from '../bootstrap.ts';
import type { DeviceKey } from '../device-key.ts';
import { AccountMenu } from './AccountMenu.tsx';
import { Shell } from './Shell.tsx';

export const Fleet = component<{
  bootstrap: Bootstrap;
  device: DeviceKey;
  theme: Theme;
}>((ctx) => () => {
  const { bootstrap, device, theme } = ctx.props;

  if (bootstrap.mode === 'local') {
    return <Shell device={device} theme={theme} />;
  }

  const user = bootstrap.user as User;
  return (
    <div class="shell">
      <header class="topbar">
        <span class="brand">zesterm</span>
        <AccountMenu user={user} />
      </header>
      <div class="centered grow">
        <div class="card">
          <h1>No machines yet</h1>
          <p class="muted">
            You are signed in, but this client cannot reach any of your machines — the relay that
            carries terminal traffic between them is not built.
          </p>
          <p class="fineprint">
            Running a daemon on this machine? The local client at <code>http://127.0.0.1:7350</code>{' '}
            talks to it directly. A browser on <code>https</code> cannot, and that is a rule of the
            web rather than a setting.
          </p>
        </div>
      </div>
    </div>
  );
});

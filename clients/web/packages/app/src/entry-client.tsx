/**
 * The browser entry. Two planes, wired once:
 *
 * - **Control**: `actorsPlugin` over `socketTransport` to the sidecar's
 *   `/_sigx/socket` — the session list, live. Same-origin in production; the
 *   vite dev server reaches it through the proxy in `vite.config.ts`.
 * - **Data**: the daemon's binary WebSocket, dialled per session by
 *   `TerminalView`, its address learned *from* the control plane.
 *
 * No SSR in v1: the sidecar serves static files, `useActorState` simply has
 * nothing to hydrate from and fetches on mount — one round trip on a
 * loopback socket.
 *
 * Both of those describe the **local** path, and this one build also serves the
 * edge, where neither is true: there is no sidecar to host the actors, and an
 * `https://` page cannot reach a `ws://` daemon on the LAN at all. So the first
 * thing it does is ask which world it is in — see `bootstrap.ts`.
 */

import { defineApp } from 'sigx';
import { actorsPlugin } from '@sigx/actors/app';
import { socketTransport } from '@sigx/actors-ws/client';
import { applyCssVars, obsidian, themeById } from '@zesterm/theme';

import { fetchBootstrap } from './bootstrap.ts';
import { deviceIdentity } from './identity.ts';
import { Shell } from './components/Shell.tsx';
import { Unavailable } from './components/Unavailable.tsx';
import './style.css';

// The mockup's client-side state list says themeId is the client's to keep.
const themeId = localStorage.getItem('zesterm.theme') ?? 'obsidian';
const theme = themeById(themeId) ?? obsidian;
applyCssVars(theme.ui, document.documentElement);

const socketUrl =
  (location.protocol === 'https:' ? 'wss://' : 'ws://') + location.host + '/_sigx/socket';

const mount = document.getElementById('app')!;

void fetchBootstrap().then((bootstrap) => {
  if (bootstrap.mode === 'cloud') {
    // Deliberately not the Shell. Reaching a shell from here needs the relay,
    // and until it exists every path the session list could take is broken:
    // there is no sidecar hosting `SessionDirectory`, and mixed content
    // forbids dialling a LAN daemon from an https page. Rendering the real UI
    // would spin on a socket that cannot connect and blame the daemon.
    defineApp(Unavailable({ theme })).mount(mount);
    return;
  }

  defineApp(Shell({ identity: deviceIdentity(), theme }))
    .use(actorsPlugin({ transport: socketTransport({ url: socketUrl }) }))
    .mount(mount);
});

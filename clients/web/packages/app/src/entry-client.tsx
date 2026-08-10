/**
 * The browser entry. Three things wired once:
 *
 * - **Which world**: `/api/bootstrap` decides whether the sidecar or the edge
 *   is answering, before anything mounts. One `vite build` serves both.
 * - **Control plane** (local only): `actorsPlugin` over `socketTransport` to
 *   the sidecar's `/_sigx/socket`. There is no sidecar at the edge, so the
 *   plugin is not installed there — an actors socket that cannot connect would
 *   spin forever and read as a broken daemon.
 * - **Routes**: `@sigx/router`, with the login gate as a `beforeEnter`.
 *
 * The data plane is unchanged: the daemon's binary WebSocket, dialled per
 * session by `TerminalView` at an address learned from the control plane.
 */

import { defineApp } from 'sigx';
import { actorsPlugin } from '@sigx/actors/app';
import { socketTransport } from '@sigx/actors-ws/client';
import { RouterView } from '@sigx/router';
import { component } from 'sigx';
import { applyCssVars, obsidian, themeById } from '@zesterm/theme';

import { fetchBootstrap } from './bootstrap.ts';
import { deviceKey } from './device-key.ts';
import { routerPlugin } from './routes.tsx';
import './style.css';

// The mockup's client-side state list says themeId is the client's to keep.
const themeId = localStorage.getItem('zesterm.theme') ?? 'obsidian';
const theme = themeById(themeId) ?? obsidian;
applyCssVars(theme.ui, document.documentElement);

// Cache the resolved background for the next load's first paint. index.html
// replays it before this bundle exists, so a light theme does not flash dark.
try {
  localStorage.setItem('zesterm.boot-bg', theme.ui.bg);
} catch {
  // Private modes throw on write; the inline fallback stands.
}

const socketUrl =
  (location.protocol === 'https:' ? 'wss://' : 'ws://') + location.host + '/_sigx/socket';

const Root = component(() => () => <RouterView />);

// The device key is awaited beside the bootstrap rather than after it: both
// are prerequisites for the first route, and IndexedDB plus a key generation
// are not free on a cold visit.
void Promise.all([fetchBootstrap(), deviceKey()]).then(([bootstrap, device]) => {
  const app = defineApp(Root({})).use(routerPlugin({ bootstrap, device, theme }));

  // Only where something hosts the actors. Installing it at the edge would
  // dial a socket that is not there.
  if (bootstrap.mode === 'local') {
    app.use(actorsPlugin({ transport: socketTransport({ url: socketUrl }) }));
  }

  app.mount(document.getElementById('app')!);
});

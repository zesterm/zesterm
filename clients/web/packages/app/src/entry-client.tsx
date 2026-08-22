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

import { fetchBootstrap } from './bootstrap.ts';
import { MONO_FAMILY } from './chrome-model.ts';
import { deviceKey } from './device-key.ts';
import { routerPlugin } from './routes.tsx';
import { setKeyboardUp } from './soft-keyboard.ts';
import { initThemeStore } from './state/theme.ts';
import { watchVisualViewport } from './visual-viewport.ts';
import './style.css';

// The grid's font constant, mirrored into a CSS variable so the chrome's mono
// text (cwd lines, key caps) is the SAME stack the canvas measures — the
// handoff forbids shipping a second mono for chrome. CSS cannot import a TS
// constant, so this property is the bridge.
document.documentElement.style.setProperty('--zt-mono', MONO_FAMILY);

// The theme store owns the whole choice lifecycle — the localStorage read,
// the fallback, the CSS vars (tokens plus derived chrome surfaces), and the
// boot-bg cache index.html replays before this bundle exists. Constructed
// here because this is the one place that may hand it the real DOM.
const store = initThemeStore(document.documentElement, window.localStorage);
const theme = store.theme;

// The soft keyboard, on the platforms that overlay it rather than resize for
// it (iOS Safari). Writes --zt-vv-height/--zt-vv-top, which .shell reads.
watchVisualViewport(window, document.documentElement, setKeyboardUp);

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

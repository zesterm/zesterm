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
 */

import { defineApp } from 'sigx';
import { actorsPlugin } from '@sigx/actors/app';
import { socketTransport } from '@sigx/actors-ws/client';
import { applyCssVars, obsidian, themeById } from '@zesterm/theme';

import { deviceIdentity } from './identity.ts';
import { Shell } from './components/Shell.tsx';
import './style.css';

// The mockup's client-side state list says themeId is the client's to keep.
const themeId = localStorage.getItem('zesterm.theme') ?? 'obsidian';
const theme = themeById(themeId) ?? obsidian;
applyCssVars(theme.ui, document.documentElement);

const socketUrl =
  (location.protocol === 'https:' ? 'wss://' : 'ws://') + location.host + '/_sigx/socket';

defineApp(Shell({ identity: deviceIdentity(), theme }))
  .use(actorsPlugin({ transport: socketTransport({ url: socketUrl }) }))
  .mount(document.getElementById('app')!);

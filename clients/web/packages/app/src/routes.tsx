/**
 * The route table, and the login gate.
 *
 * `@sigx/router` rather than a hand-rolled one. An earlier draft here was
 * ~70 lines over `pushState`, written after checking `sigx`'s own exports and
 * concluding the ecosystem shipped no router — it ships one, as a sibling
 * package, and checking one package's exports was not the same question.
 *
 * Note the guard property is **`beforeEnter`**, not the `guard:` the published
 * guide shows; the shipped `RouteRecordRaw` is what this follows.
 */

import {
  createWebHistory,
  createRouterPlugin,
  useQuery,
  type NavigationGuard,
} from '@sigx/router';
import { component } from 'sigx';
import type { Theme } from '@zesterm/theme';

import type { Bootstrap } from './bootstrap.ts';
import type { DeviceKey } from './device-key.ts';
import { Login } from './components/Login.tsx';
import { Fleet } from './components/Fleet.tsx';
import { NotFound } from './components/NotFound.tsx';

export interface AppContext {
  readonly bootstrap: Bootstrap;
  readonly device: DeviceKey;
  readonly theme: Theme;
}

/**
 * Signed out on the hosted path? Go to `/login`.
 *
 * **Redirect, never `false`.** The router's own guide is explicit about this
 * and it is the trap worth repeating: on a direct page load `from` is `null`,
 * and returning `false` merely blocks the navigation — leaving the protected
 * component on screen. A gate that is bypassed by pasting a URL is not a gate.
 *
 * There is nothing to gate on the local path. Reaching `127.0.0.1:7350` *is*
 * the authority, which is the same posture the sidecar's `allowAnonymous: true`
 * already states — the daemon still challenges the device key underneath.
 */
export function requireAccount(ctx: AppContext): NavigationGuard {
  return () => {
    if (ctx.bootstrap.mode === 'local') return true;
    return ctx.bootstrap.user === null ? '/login' : true;
  };
}

/** Signed in already? `/login` is not somewhere to be. */
function skipIfSignedIn(ctx: AppContext): NavigationGuard {
  return () => {
    if (ctx.bootstrap.mode === 'local') return '/hosts';
    return ctx.bootstrap.user === null ? true : '/hosts';
  };
}

export function routerPlugin(ctx: AppContext) {
  const gate = requireAccount(ctx);

  // Route components take no props -- the router supplies the route, not the
  // app's context -- so the screens are wrapped in closures over `ctx` here.
  // That also keeps `Fleet` and `Login` ordinary components a test can render
  // directly, rather than things that can only exist inside a router.
  // `useQuery()` returns the record itself rather than a signal, so it is read
  // inside the render rather than captured at setup -- otherwise arriving at
  // `/login?error=1` from the callback would show no error on a route that was
  // already mounted.
  const LoginRoute = component(() => () => (
    <Login failed={useQuery()['error'] !== undefined} />
  ));

  const FleetRoute = component(() => () => (
    <Fleet bootstrap={ctx.bootstrap} device={ctx.device} theme={ctx.theme} />
  ));

  return createRouterPlugin({
    history: createWebHistory(),
    routes: [
      { path: '/', redirect: '/hosts' },
      { path: '/login', component: LoginRoute, beforeEnter: skipIfSignedIn(ctx) },
      { path: '/hosts', component: FleetRoute, beforeEnter: gate },
      { path: '/h/:hostId', component: FleetRoute, beforeEnter: gate },
      { path: '/h/:hostId/s/:sessionId', component: FleetRoute, beforeEnter: gate },
      // Unknown paths must not quietly render the session list: the Worker's
      // asset fallback serves index.html for *any* path, so a typo arrives
      // here looking exactly like a real route.
      { path: '/:rest*', component: NotFound },
    ],
  });
}

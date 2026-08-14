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
import { LinkApprove } from './components/LinkApprove.tsx';
import { NotFound } from './components/NotFound.tsx';
import { ThemeGallery } from './components/ThemeGallery.tsx';
import { SHELL_CHILD_PATHS, SHELL_PATH, safeNextPath } from './route-table.ts';

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
  return (to) => {
    if (ctx.bootstrap.mode === 'local') return true;
    if (ctx.bootstrap.user !== null) return true;
    // The destination rides along, so it survives the whole OAuth round trip:
    // Login threads it into `/auth/login?next=…`, the Worker's `safeNext`
    // vets it again, and the callback lands back where the person was going.
    // Without this, `/link?grant=…` — a URL another device just opened — would
    // dead-end at the fleet screen with the grant lost.
    return `/login?next=${encodeURIComponent(to.fullPath)}`;
  };
}

/** Signed in already? `/login` is not somewhere to be — go where `next` says. */
function skipIfSignedIn(ctx: AppContext): NavigationGuard {
  return (to) => {
    const raw = to.query['next'];
    const next = safeNextPath(typeof raw === 'string' ? raw : undefined);
    if (ctx.bootstrap.mode === 'local') return next;
    return ctx.bootstrap.user === null ? true : next;
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
  const LoginRoute = component(() => () => {
    const query = useQuery();
    const rawNext = query['next'];
    return (
      <Login
        failed={query['error'] !== undefined}
        next={safeNextPath(typeof rawNext === 'string' ? rawNext : undefined)}
      />
    );
  });

  const LinkRoute = component(() => () => <LinkApprove bootstrap={ctx.bootstrap} />);

  const FleetRoute = component(() => () => (
    <Fleet bootstrap={ctx.bootstrap} device={ctx.device} theme={ctx.theme} />
  ));

  const ThemesRoute = component(() => () => <ThemeGallery theme={ctx.theme} />);

  return createRouterPlugin({
    history: createWebHistory(),
    routes: [
      { path: '/', redirect: '/hosts' },
      { path: '/login', component: LoginRoute, beforeEnter: skipIfSignedIn(ctx) },
      // ONE record for every URL the shell lives at, with the session paths
      // as absolute children (see route-table.ts): RouterView keys the routed
      // component by matched[0].path, so sibling records would remount the
      // Shell — and discard every open tab — on each crossing between /hosts
      // and a session URL. The children carry no component of their own; the
      // shell reads the params itself — off `useRoute()` at each use, never a
      // `useParams()` record captured at setup, which is frozen (#196) — and
      // activates —
      // or opens from the directory — the named tab. The reverse arrow is the
      // shell's: activating a tab navigates here, so the URL is an EFFECT of
      // activation and the route only ever feeds activation, never a
      // navigation of its own — one direction each way, no loop. `gate` runs
      // for the children too: the guard walks every matched record, and the
      // parent is always matched.
      {
        path: SHELL_PATH,
        component: FleetRoute,
        beforeEnter: gate,
        children: SHELL_CHILD_PATHS.map((path) => ({ path })),
      },
      // The theme gallery (design §8). Gated like the shell on the hosted
      // path; on the local path the gate is a pass-through, and the gallery
      // works identically there — the theme id never crosses the wire.
      { path: '/themes', component: ThemesRoute, beforeEnter: gate },
      // The device link approval page (#226). Behind the gate, which is what
      // makes the whole hand-off work signed out: the gate carries
      // `/link?grant=…` through OAuth as `next`, and the person lands back
      // here with their session — grant intact.
      { path: '/link', component: LinkRoute, beforeEnter: gate },
      // Unknown paths must not quietly render the session list: the Worker's
      // asset fallback serves index.html for *any* path, so a typo arrives
      // here looking exactly like a real route.
      { path: '/:rest*', component: NotFound },
    ],
  });
}

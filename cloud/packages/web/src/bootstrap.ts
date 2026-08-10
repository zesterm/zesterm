/**
 * What the app is told about the world it woke up in.
 *
 * The web client ships as **one** `vite build` that serves both the loopback
 * sidecar and the edge. It must not learn which from a `VITE_*` variable baked
 * in at build time, or the bundle you tested is not the bundle you shipped —
 * so it asks, once, at runtime.
 *
 * This type is duplicated in `clients/web/packages/app/src/bootstrap.ts`
 * rather than shared. The two projects have separate lockfiles on purpose (see
 * `cloud/README.md`), and a package dependency across that line would undo it
 * for one interface. It is small, it is checked by the app's own tests against
 * a real response from this Worker, and the day it grows past that it earns a
 * generated binding like the wire types already have.
 */

/** The sidecar: `ws://` is legal here, and this path survives Cloudflare being down. */
export interface LocalBootstrap {
  readonly mode: 'local';
}

/**
 * The edge. `user` is `null` until accounts land — the field exists now so the
 * app can render its signed-out state against the shape it will keep, rather
 * than against a shape that changes under it.
 */
export interface CloudBootstrap {
  readonly mode: 'cloud';
  readonly user: null;
}

export type Bootstrap = LocalBootstrap | CloudBootstrap;

export function cloudBootstrap(): CloudBootstrap {
  return { mode: 'cloud', user: null };
}

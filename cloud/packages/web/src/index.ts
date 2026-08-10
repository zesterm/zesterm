/**
 * The entrypoint, and nothing else.
 *
 * Everything that decides *what* to answer is in `router.ts`, as a pure
 * function over `Request` — so it is covered by `node --test` without workerd.
 * What is left here is the one thing that genuinely needs the runtime: handing
 * a request the router declined to the static asset binding.
 */

import { routeApi } from './router.ts';

export interface Env {
  /**
   * The built web client — `clients/web/packages/app/dist`, wired in
   * `wrangler.jsonc`. Serving the SPA is the binding's job, including the
   * `not_found_handling` fallback that makes client-side routes work.
   */
  readonly ASSETS: { fetch(request: Request): Promise<Response> };
  /** This deployment's own origin. A placeholder until the account exists. */
  readonly APP_ORIGIN: string;
}

export default {
  fetch(request: Request, env: Env): Response | Promise<Response> {
    // `run_worker_first` in wrangler.jsonc routes only `/api/*` and `/auth/*`
    // here; everything else never reaches this Worker at all. The check is
    // still made rather than assumed, because the two lists are in different
    // files and the failure if they drift — the app served as a 404 JSON body,
    // or an API path swallowed by index.html — is confusing out of proportion
    // to the cost of one `startsWith`.
    return routeApi(request) ?? env.ASSETS.fetch(request);
  },
};

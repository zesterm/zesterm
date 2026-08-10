/**
 * The entrypoint, and nothing else.
 *
 * Everything that decides *what* to answer is in `router.ts`, which is pure
 * enough to run under `node --test`. What is left here is the one thing that
 * genuinely needs the runtime: handing a request the router declined to the
 * static asset binding.
 */

import type { Env } from './env.ts';
import { routeApi } from './router.ts';

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    // `run_worker_first` in wrangler.jsonc routes only /api/* and /auth/* here,
    // so most requests never reach this Worker. The check is still made rather
    // than assumed, because the two lists live in different files and the
    // failure if they drift -- the app served as a 404 JSON body, or an API
    // path swallowed by index.html -- is confusing out of proportion to the
    // cost of one comparison.
    return (await routeApi(request, env)) ?? env.ASSETS.fetch(request);
  },
};

export type { Env };

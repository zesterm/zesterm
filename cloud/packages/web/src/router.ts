/**
 * The `/api` surface, as a pure function.
 *
 * Pure — `(Request) => Response | null` — so the whole routing table is
 * testable under `node --test` with no workerd, no miniflare and no bindings.
 * `null` means "not mine": the entrypoint then hands the request to the asset
 * binding, which is what serves the app.
 *
 * The split matters more than it looks. Everything that decides *what* to
 * answer lives here and is covered by ordinary tests; `index.ts` keeps only the
 * part that genuinely needs the runtime. When this grows an auth flow, the
 * cookie parsing and the state check belong on this side of that line too.
 */

import { cloudBootstrap } from './bootstrap.ts';

/** JSON, with caching refused. */
function json(body: unknown, status = 200): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: {
      'content-type': 'application/json; charset=utf-8',
      // `/api/*` answers are per-user the moment accounts land, and a response
      // cached at the edge before that is a response served to the wrong
      // person after it. Refusing from the start costs nothing and means the
      // day this matters is not the day someone remembers.
      'cache-control': 'no-store',
    },
  });
}

/**
 * Answer an `/api/*` request, or `null` to let the assets handle it.
 *
 * Deliberately not a framework. Five routes do not need one, and a Worker's
 * cold start is a real number that a router dependency spends for you.
 */
export function routeApi(request: Request): Response | null {
  const url = new URL(request.url);
  if (!url.pathname.startsWith('/api/')) return null;

  if (url.pathname === '/api/bootstrap') {
    if (request.method !== 'GET') return json({ error: 'method_not_allowed' }, 405);
    return json(cloudBootstrap());
  }

  // An unknown `/api/*` path is 404 JSON, never the SPA's index.html. A fetch
  // for a route that does not exist should fail as a fetch — falling through to
  // the app would hand JavaScript a 200 full of HTML, which surfaces as a JSON
  // parse error naming a line of markup and tells you nothing.
  return json({ error: 'not_found' }, 404);
}

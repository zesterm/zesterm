/**
 * Responses, and the CSRF rule — stated once so it cannot be half-applied.
 */

/** JSON, with caching refused. */
export function json(body: unknown, status = 200, headers: HeadersInit = {}): Response {
  return new Response(JSON.stringify(body), {
    status,
    headers: {
      'content-type': 'application/json; charset=utf-8',
      // Every /api/* answer is per-user, and a response cached at the edge is
      // one served to the wrong person afterwards.
      'cache-control': 'no-store',
      ...headers,
    },
  });
}

/**
 * Whether a state-changing request may proceed.
 *
 * Two conditions, and together with `SameSite=Lax` on the session cookie they
 * are the whole defence — no CSRF tokens, no double-submit cookie:
 *
 * 1. **`Origin` equals ours.** A cross-site form or `fetch` carries the
 *    attacker's origin; a same-site one carries ours.
 * 2. **`Content-Type: application/json`.** A form POST — the one cross-site
 *    request a browser will make *without* a preflight — cannot set it. A
 *    `fetch` that does set it triggers a preflight this Worker never answers.
 *
 * `/api/*` emits no CORS headers anywhere, which is what makes (2) hold.
 *
 * Note it fails **closed** on a missing `Origin`: some privacy tooling strips
 * it, and the cost is a rejected write rather than an accepted forgery.
 */
export function csrfOk(request: Request, appOrigin: string): boolean {
  if (request.method === 'GET' || request.method === 'HEAD') return true;
  if (request.headers.get('origin') !== appOrigin) return false;
  const ct = request.headers.get('content-type') ?? '';
  return ct.split(';')[0]?.trim() === 'application/json';
}

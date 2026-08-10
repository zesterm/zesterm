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
  if (isSafeMethod(request)) return true;
  if (request.headers.get('origin') !== appOrigin) return false;
  return isJsonBody(request);
}

/**
 * The same rule with condition (1) dropped, for a route that carries **no
 * ambient authority**.
 *
 * CSRF is a specific attack: a browser is made to issue a request that succeeds
 * because it silently attaches credentials the page did not have to know — the
 * session cookie. `Origin` is how that is caught. A route which never consults
 * the cookie has nothing of the kind to be forged, so the check protects
 * nothing there and only excludes the callers who cannot satisfy it: a daemon
 * on someone's Mac is not a browser and sends no `Origin` at all.
 *
 * Condition (2) stays, and it is not ceremony. Requiring
 * `Content-Type: application/json` still costs a daemon one header while
 * keeping the one cross-site request a browser makes without a preflight — a
 * form POST — off the route entirely. Whatever an attacker can reach here they
 * could equally reach with `curl`; what this stops is a *victim's* browser
 * being the one that reaches it.
 *
 * Using this is a deliberate act. The route that does must authenticate the
 * request some other way — for `/api/enroll/claim`, a code the account minted
 * and an Ed25519 signature over it — and must not read the session cookie at
 * all, because a route that does both is exactly the CSRF hole this dropped the
 * defence against.
 */
export function csrfOkWithoutOrigin(request: Request): boolean {
  if (isSafeMethod(request)) return true;
  return isJsonBody(request);
}

function isSafeMethod(request: Request): boolean {
  return request.method === 'GET' || request.method === 'HEAD';
}

function isJsonBody(request: Request): boolean {
  const ct = request.headers.get('content-type') ?? '';
  return ct.split(';')[0]?.trim() === 'application/json';
}

/**
 * The request body as a JSON object, or `null` for anything else.
 *
 * A malformed body, a JSON array, `null` and a bare string all collapse to the
 * same `null`, so every handler's first branch is one shape rather than four.
 */
export async function jsonObject(request: Request): Promise<Record<string, unknown> | null> {
  try {
    const body: unknown = await request.json();
    if (typeof body !== 'object' || body === null || Array.isArray(body)) return null;
    return body as Record<string, unknown>;
  } catch {
    return null;
  }
}

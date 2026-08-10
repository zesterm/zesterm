/**
 * Which world this page woke up in, asked at runtime rather than baked in.
 *
 * The app ships as **one** `vite build`, serving both the loopback sidecar and
 * the edge. Learning which from a `VITE_*` variable would mean the bundle you
 * tested is not the bundle you shipped, so it asks `/api/bootstrap` — which
 * both the sidecar (`packages/sidecar/src/server.ts`) and the Worker
 * (`cloud/packages/web/src/router.ts`) answer.
 *
 * The type is duplicated in the Worker rather than shared: the two projects
 * have separate lockfiles on purpose, and a package dependency across that line
 * would undo it for one interface. `bootstrap.test.ts` pins the two shapes
 * against each other, which is the part that actually matters.
 */

/** The sidecar. `ws://` is legal here, and this path survives Cloudflare being down. */
export interface LocalBootstrap {
  readonly mode: 'local';
}

/** The edge. `user` stays `null` until accounts land. */
export interface CloudBootstrap {
  readonly mode: 'cloud';
  readonly user: null;
}

export type Bootstrap = LocalBootstrap | CloudBootstrap;

/**
 * What the app assumes when `/api/bootstrap` cannot be reached or is not
 * understood.
 *
 * `local`, deliberately. An unreachable bootstrap is overwhelmingly a sidecar
 * that has not finished starting or a stale build being served by something
 * that is not the Worker, and the local path is the one that then works. The
 * opposite default — assume cloud — would show a signed-out screen to someone
 * running on loopback, where there is nothing to sign in to.
 */
export const FALLBACK: Bootstrap = { mode: 'local' };

/** Narrow an unknown JSON body, rather than trusting the server's shape. */
export function parseBootstrap(value: unknown): Bootstrap | null {
  if (typeof value !== 'object' || value === null) return null;
  const mode = (value as { mode?: unknown }).mode;
  if (mode === 'local') return { mode: 'local' };
  if (mode === 'cloud') return { mode: 'cloud', user: null };
  return null;
}

/**
 * Fetch it, or fall back.
 *
 * Never rejects. A boot path that can throw is a boot path that renders a blank
 * page, and "which server am I talking to" is not worth that — the fallback is
 * a working app on the more likely of the two answers.
 */
export async function fetchBootstrap(
  fetchImpl: typeof fetch = fetch,
  url = '/api/bootstrap',
): Promise<Bootstrap> {
  try {
    const res = await fetchImpl(url, { headers: { accept: 'application/json' } });
    if (!res.ok) return FALLBACK;
    return parseBootstrap(await res.json()) ?? FALLBACK;
  } catch {
    return FALLBACK;
  }
}

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

/** Who is signed in. Mirrors the Worker's `publicUser`, field for field. */
export interface User {
  readonly id: string;
  readonly displayName: string;
  readonly email: string | null;
  readonly avatarUrl: string | null;
}

/** The edge. `user` is `null` when signed out. */
export interface CloudBootstrap {
  readonly mode: 'cloud';
  readonly user: User | null;
  /**
   * Where the relay Worker lives, or `null` where there is no relay.
   *
   * On the envelope rather than baked into the bundle for the reason `mode` is,
   * and a *second* origin because ADR-009 makes the relay a second Worker.
   * `null` is an ordinary deployment, not a broken one: the fleet is still
   * reachable over `ws` on the LAN.
   */
  readonly relayOrigin: string | null;
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

/**
 * Narrow the user, or `null`.
 *
 * Field by field rather than a cast: this is the one place a server response
 * becomes something the UI renders, and `displayName` being `undefined`
 * further in reads as a blank account menu rather than as a bad response.
 */
export function parseUser(value: unknown): User | null {
  if (typeof value !== 'object' || value === null) return null;
  const u = value as Record<string, unknown>;
  if (typeof u['id'] !== 'string' || typeof u['displayName'] !== 'string') return null;
  return {
    id: u['id'],
    displayName: u['displayName'],
    email: typeof u['email'] === 'string' ? u['email'] : null,
    avatarUrl: typeof u['avatarUrl'] === 'string' ? u['avatarUrl'] : null,
  };
}

/**
 * An `http(s)` origin, or `null`.
 *
 * Checked here rather than at the dial, because the dial is where it would be
 * expensive: `relayDial` builds a `URL` from this string, and a malformed one
 * throws inside a promise chain whose only vocabulary is "the connection
 * closed" — so the fleet would show rows that fail to connect for ever with
 * nothing naming the cause. A value that is not a URL is no relay, which is a
 * state the app already renders honestly.
 *
 * `http(s)` only, though `relayDial` would happily open a `ws://` string:
 * one spelling for one deployment. Two would eventually be two settings that
 * disagree, and the scheme is the half `relayDial` derives anyway.
 */
function parseRelayOrigin(value: unknown): string | null {
  if (typeof value !== 'string' || !URL.canParse(value)) return null;
  const { protocol } = new URL(value);
  return protocol === 'https:' || protocol === 'http:' ? value : null;
}

/** Narrow an unknown JSON body, rather than trusting the server's shape. */
export function parseBootstrap(value: unknown): Bootstrap | null {
  if (typeof value !== 'object' || value === null) return null;
  const mode = (value as { mode?: unknown }).mode;
  if (mode === 'local') return { mode: 'local' };
  if (mode !== 'cloud') return null;
  return {
    mode: 'cloud',
    user: parseUser((value as { user?: unknown }).user),
    relayOrigin: parseRelayOrigin((value as { relayOrigin?: unknown }).relayOrigin),
  };
}

/**
 * How long boot will wait for an answer before assuming one.
 *
 * Nothing mounts until this resolves, so the number is a blank-page budget
 * rather than a network one. A stalled request — a captive portal that accepts
 * the connection and never answers is the usual way — would otherwise hang
 * boot forever, and "forever" is the one duration a splash cannot cover.
 * Loopback answers in single-digit milliseconds and the edge in tens, so three
 * seconds is far past either and still short of feeling broken.
 */
export const BOOTSTRAP_TIMEOUT_MS = 3_000;

/**
 * Fetch it, or fall back.
 *
 * Never rejects, and never hangs. A boot path that can throw *or* stall is a
 * boot path that renders a blank page, and "which server am I talking to" is
 * not worth that — the fallback is a working app on the more likely answer.
 */
export async function fetchBootstrap(
  fetchImpl: typeof fetch = fetch,
  url = '/api/bootstrap',
  timeoutMs = BOOTSTRAP_TIMEOUT_MS,
): Promise<Bootstrap> {
  try {
    const res = await fetchImpl(url, {
      headers: { accept: 'application/json' },
      // `AbortSignal.timeout` rather than a `Promise.race`: racing leaves the
      // request running and its rejection unhandled, which surfaces later as
      // an unrelated console error nobody can place.
      signal: AbortSignal.timeout(timeoutMs),
    });
    if (!res.ok) return FALLBACK;
    return parseBootstrap(await res.json()) ?? FALLBACK;
  } catch {
    // Abort included: a timeout is exactly the case the fallback exists for.
    return FALLBACK;
  }
}

/**
 * The `/link` approval page's data: reading a grant, and answering it.
 *
 * The page is the account-holder's half of the browser hand-off sign-in
 * (#226): the desktop app parked a grant and opened this URL; the person
 * compares the fingerprint the page shows against the one the app shows, and
 * clicks. Everything here is the cookie side — the app's signed halves never
 * pass through the browser.
 *
 * Shapes are parsed field by field, `registry.ts`'s reason: this is a server
 * response about to be rendered on a page whose whole job is to be read
 * carefully, and a card saying `undefined` teaches people to approve without
 * reading.
 */

import type { DeviceKind } from './registry.ts';

export interface LinkGrantDetails {
  readonly label: string;
  readonly kind: DeviceKind;
  readonly platform: string;
  /** First 8 hex of the device key — render through `fingerprintGroups`. */
  readonly fingerprint: string;
  /** Already approved: the page offers "return to the app" instead of a button. */
  readonly approved: boolean;
  readonly expiresAt: number;
}

const KINDS: readonly string[] = ['browser', 'phone', 'desktop'];

export function parseLinkGrant(value: unknown): LinkGrantDetails | null {
  if (typeof value !== 'object' || value === null) return null;
  const g = value as Record<string, unknown>;
  const label = typeof g['label'] === 'string' && g['label'].length > 0 ? g['label'] : null;
  const kind = typeof g['kind'] === 'string' && KINDS.includes(g['kind']) ? g['kind'] : null;
  const fingerprint =
    typeof g['fingerprint'] === 'string' && /^[0-9a-f]{8}$/.test(g['fingerprint'])
      ? g['fingerprint']
      : null;
  const expiresAt =
    typeof g['expiresAt'] === 'number' && Number.isFinite(g['expiresAt']) ? g['expiresAt'] : null;
  if (label === null || kind === null || fingerprint === null || expiresAt === null) return null;
  return {
    label,
    kind: kind as DeviceKind,
    platform: typeof g['platform'] === 'string' ? g['platform'] : '',
    fingerprint,
    // Absent reads as NOT approved — the cautious direction: the page then
    // shows a button whose press is idempotent, rather than telling someone
    // an unapproved device is already in.
    approved: g['approved'] === true,
    expiresAt,
  };
}

/**
 * `ab12cd34` → `ab12 cd34` — the spacing the person compares against the
 * app's own display, four characters at a time because eight unbroken hex
 * digits read as noise and the whole point of showing them is that someone
 * actually looks.
 */
export function fingerprintGroups(fingerprint: string): string {
  return fingerprint.replace(/(.{4})(?=.)/g, '$1 ');
}

/**
 * The grant id out of the page's query, or `null` for anything that is not
 * one. Shape-checked here so a mangled URL reads as "this link is not valid"
 * rather than as a 404 from a request that was never going to be right —
 * 43 base64url chars, the Worker's own `looksLikeLinkGrant`.
 */
export function grantFromQuery(value: string | string[] | undefined): string | null {
  if (typeof value !== 'string') return null;
  return /^[A-Za-z0-9_-]{43}$/.test(value) ? value : null;
}

/** What the page is showing. One closed union, so the render is a switch. */
export type LinkPhase =
  | { readonly phase: 'loading' }
  | { readonly phase: 'invalid' }
  | { readonly phase: 'ready'; readonly grant: LinkGrantDetails; readonly busy: boolean }
  | { readonly phase: 'approved'; readonly grant: LinkGrantDetails }
  | { readonly phase: 'denied' };

async function act(path: string, fetchImpl: typeof fetch): Promise<Response> {
  return fetchImpl(path, {
    method: 'POST',
    headers: { accept: 'application/json', 'content-type': 'application/json' },
    credentials: 'same-origin',
  });
}

/** The details read. Any refusal is one `null` — the page says "no longer valid". */
export async function fetchLinkGrant(
  id: string,
  fetchImpl: typeof fetch = fetch,
): Promise<LinkGrantDetails | null> {
  const res = await fetchImpl(`/api/link/${encodeURIComponent(id)}`, {
    headers: { accept: 'application/json' },
    credentials: 'same-origin',
  });
  if (!res.ok) return null;
  return parseLinkGrant(await res.json());
}

/**
 * Approve. Throws on refusal rather than folding to a boolean: the page has
 * just shown the person a working grant, so a failure here is a real error
 * worth naming, not an expected state.
 */
export async function approveLinkGrant(id: string, fetchImpl: typeof fetch = fetch): Promise<void> {
  const res = await act(`/api/link/${encodeURIComponent(id)}/approve`, fetchImpl);
  if (!res.ok) throw new Error(`link approve answered ${res.status}`);
}

export async function denyLinkGrant(id: string, fetchImpl: typeof fetch = fetch): Promise<void> {
  const res = await act(`/api/link/${encodeURIComponent(id)}/deny`, fetchImpl);
  if (!res.ok) throw new Error(`link deny answered ${res.status}`);
}

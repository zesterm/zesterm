/**
 * Turning what `/api/bootstrap` said into something `dialFor` can use.
 *
 * `relayDial` takes an injected `mintTicket` so it could land and be tested
 * before the endpoint existed. The endpoint exists now, and this is the one
 * place that calls it — everything above still receives the mint as a function,
 * which is what keeps the dial testable with no network.
 *
 * **The mint is a same-origin `POST`, and the relay is a different origin.**
 * That is the whole reason a ticket exists: the session cookie is scoped to the
 * app's origin and could never travel to the relay, so admission has to be
 * something the app fetches here and hands over there. ADR-009 names
 * authorizing the relay with the session cookie as rejected, and it is not
 * merely rejected — two origins make it impossible.
 */

import type { CloudBootstrap } from './bootstrap.ts';
import type { RelayAccess } from './dial-for.ts';

/**
 * `null` when this deployment has no relay, which is an ordinary state — the
 * fleet is still reachable over `ws` on the LAN.
 */
export function relayAccess(
  bootstrap: CloudBootstrap,
  fetchImpl: typeof fetch = fetch,
): RelayAccess | null {
  const origin = bootstrap.relayOrigin;
  if (origin === null) return null;
  return { origin, mintTicket: (hostId) => mintTicket(hostId, fetchImpl) };
}

/**
 * One ticket, for one attach.
 *
 * **Not cached, and the `expiresAt` the Worker also answers is ignored.** A
 * ticket lives thirty seconds and is spent by the socket that follows it; a
 * cache would hold a bearer credential in memory to save a round trip on a path
 * that already costs a WebSocket open, and would hand a *stale* one to the
 * redial that needed a fresh one most. `expiresAt` is only useful to that
 * cache, so reading it would be the first half of building one.
 *
 * Rejects rather than returning a sentinel, because `relayDial` treats a failed
 * mint as a dropped dial and its backoff ladder already retries exactly this —
 * a signed-out tab, a revoked host, a relay that is not configured.
 */
async function mintTicket(hostId: string, fetchImpl: typeof fetch): Promise<string> {
  const res = await fetchImpl('/api/relay/ticket', {
    method: 'POST',
    // Both halves of the CSRF rule the Worker applies: `credentials: 'same-origin'`
    // is the default and the cookie is what authenticates this, and the JSON
    // content type is what a cross-site form POST cannot set.
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify({ hostId }),
  });
  if (!res.ok) throw new Error(`the relay ticket was refused: ${res.status}`);

  const body: unknown = await res.json();
  const ticket = (body as { ticket?: unknown }).ticket;
  // Narrowed rather than cast: an empty string is a legal JSON value and would
  // reach `new WebSocket` as the subprotocol `ticket.`, which is a token the
  // relay refuses — one round trip later, with nothing naming the cause.
  if (typeof ticket !== 'string' || ticket.length === 0) {
    throw new Error('the relay ticket response carried no ticket');
  }
  return ticket;
}

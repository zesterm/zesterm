/**
 * "Who is asking", for the routes that answer machines as well as people.
 *
 * The one rule that makes the router's bearer exemption sound lives here, as
 * structure rather than as discipline: **a request that carries an
 * `Authorization` header is resolved by the token alone.** It never falls back
 * to the cookie — a bad token is `null`, not "try the session". So "this
 * request skipped the Origin check" and "this request was authorized by the
 * cookie" can never both be true, which is the invariant `ORIGINLESS` states
 * for its own routes and `BEARER` must keep for these.
 *
 * A cross-site page cannot attach `Authorization` without a CORS preflight (it
 * is not a safelisted header), `/api/*` answers no preflights, and a request
 * that somehow arrived anyway would be authorized as the *attacker's* machine
 * — their token, not the victim's anything — which is not a forgery.
 */

import type { Env } from '../env.ts';
import type { PublicUser } from '../db/types.ts';
import { resolveMachineToken, type MachinePrincipal } from '../db/machine-tokens.ts';
import { currentUser } from './session.ts';

export type Principal =
  | { readonly kind: 'user'; readonly user: PublicUser }
  | { readonly kind: 'host'; readonly id: string; readonly userId: string }
  | { readonly kind: 'device'; readonly id: string; readonly userId: string };

/**
 * Whether a request offers the one Authorization shape this Worker speaks.
 *
 * The router keys its Origin exemption on this — not on the mere presence of
 * an `Authorization` header — so a `Basic` or `Negotiate` header from some
 * proxy cannot widen the exemption to a request `requestPrincipal` would never
 * resolve by token. The two functions deliberately share this predicate's
 * spelling of "bearer" so they cannot drift apart.
 */
export function carriesBearer(request: Request): boolean {
  const header = request.headers.get('authorization');
  return header !== null && header.slice(0, 7).toLowerCase() === 'bearer ';
}

export async function requestPrincipal(
  request: Request,
  env: Env,
  now: number,
): Promise<Principal | null> {
  const header = request.headers.get('authorization');
  if (header !== null) {
    const token = bearerToken(header);
    if (token === null) return null;
    const machine = await resolveMachineToken(env.DB, token, now);
    return machine === null ? null : asPrincipal(machine);
  }

  const user = await currentUser(request, env, now);
  return user === null ? null : { kind: 'user', user };
}

function asPrincipal(machine: MachinePrincipal): Principal {
  return machine.kind === 'host'
    ? { kind: 'host', id: machine.id, userId: machine.userId }
    : { kind: 'device', id: machine.id, userId: machine.userId };
}

/**
 * `Bearer <token>`, per RFC 6750 — scheme case-insensitive, exactly one space,
 * anything else `null`. No other scheme is in use here, and answering a
 * `Basic` header by reading the cookie would be the fallback this module
 * exists to rule out.
 */
function bearerToken(header: string): string | null {
  const space = header.indexOf(' ');
  if (space === -1) return null;
  if (header.slice(0, space).toLowerCase() !== 'bearer') return null;
  const token = header.slice(space + 1);
  return token.length === 0 || token.includes(' ') ? null : token;
}

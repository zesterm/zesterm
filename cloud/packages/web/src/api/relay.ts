/**
 * `POST /api/relay/ticket` — a browser or the desktop app asks for admission
 * to one of the account's machines' rooms.
 *
 * The posture is `api/registry.ts`'s, deliberately and line for line: a
 * principal or 401, the id checked for shape before any round trip, and
 * ownership in the `WHERE` clause rather than in a branch after the read. A
 * host belonging to someone else is a 404 identical to one that does not
 * exist, because the ids are public keys and an endpoint that told the two
 * apart would answer "is this key enrolled with zesterm" for strangers'
 * machines.
 *
 * Two principals may mint: a person (cookie) and a **device** (bearer). A
 * *host's* token may not — a machine serving shells has no business minting
 * admission to its owner's other machines, and the 401 is the same one an
 * absent credential gets.
 *
 * On the cookie path it reads the session cookie, so it must never join
 * `ORIGINLESS`; it is on `BEARER` instead, which drops the Origin check only
 * for requests the cookie played no part in. See `router.ts` and
 * `api/principal.ts`.
 */

import { fromHex, readCookie, sessionIdOf, SESSION_COOKIE } from '@zesterm/cloud-shared';

import { ownsLiveHost } from '../db/registry.ts';
import type { Env } from '../env.ts';
import { json, jsonObject } from '../http.ts';
import { KEY_LEN } from '../enroll/preimage.ts';
import { mintAttachTicket, SIGNING_KEY_LEN } from '../relay/ticket.ts';
import { requestPrincipal } from './principal.ts';

export async function mintRelayTicket(request: Request, env: Env, now: number): Promise<Response> {
  const principal = await requestPrincipal(request, env, now);
  if (principal === null || principal.kind === 'host') {
    return json({ error: 'unauthorized' }, 401);
  }
  const userId = principal.kind === 'user' ? principal.user.id : principal.userId;

  const body = await jsonObject(request);
  const hostId = body?.['hostId'];

  // Shape first, so a mistyped id costs no round trip — and lowercase-only,
  // because `hosts.id` is stored as `hex()` writes it and an uppercase
  // spelling of a key is a different primary key rather than the same machine.
  //
  // 400 here where `revokeRegistryEntry` answers 404, and the difference is
  // deliberate: that route takes its id from the path, where a 400 and a 404
  // would together say which ids exist. This one takes it from a body it has
  // already declared malformed-or-not, and an id that could not be any host's
  // reveals nothing about anybody's account by being named a bad request.
  if (typeof hostId !== 'string' || fromHex(hostId, KEY_LEN) === null) {
    return json({ error: 'bad_request', detail: 'hostId must be 64 lowercase hex characters' }, 400);
  }

  if (!(await ownsLiveHost(env.DB, hostId, userId))) return json({ error: 'not_found' }, 404);

  // Checked *after* ownership, and that order is the point: a deployment with
  // no relay still answers a stranger's host with the same 404 everything else
  // does. Checked first, the 503 would arrive for hosts that do not exist and
  // the 404 only for hosts that do — an oracle for which machines are enrolled,
  // handed out by a route that has otherwise been careful not to be one.
  const signingKey = fromHex(env.TICKET_SIGNING_KEY ?? '', SIGNING_KEY_LEN);
  if (signingKey === null) return json({ error: 'relay_unavailable' }, 503);

  // Which device asked, when the credential can say; which *session*, when
  // only the cookie can. Both spellings are prefixed because both raw forms
  // are 64 hex and would otherwise be indistinguishable in relay logs. The
  // relay never authorizes on `dev` either way — it is attribution, and on the
  // cookie path nothing in the schema links a session to a device enrolment.
  // Re-derived from the cookie rather than threaded out of the resolver, which
  // answers who rather than which session — one more SHA-256 against a round
  // trip that already happened.
  const dev =
    principal.kind === 'device'
      ? `device:${principal.id}`
      : `session:${await sessionIdOf(readCookie(request.headers.get('cookie'), SESSION_COOKIE) ?? '')}`;

  const minted = await mintAttachTicket({ signingKey, host: hostId, user: userId, dev, now });
  return json(minted);
}

/**
 * `POST /api/relay/ticket` — the browser asks for admission to one of its own
 * machines' rooms.
 *
 * The posture is `api/registry.ts`'s, deliberately and line for line: a session
 * or 401, the id checked for shape before any round trip, and ownership in the
 * `WHERE` clause rather than in a branch after the read. A host belonging to
 * someone else is a 404 identical to one that does not exist, because the ids
 * are public keys and an endpoint that told the two apart would answer "is this
 * key enrolled with zesterm" for strangers' machines.
 *
 * **It reads the session cookie, so it must never join `ORIGINLESS`.** That set
 * is sound only because its one member consults no cookie at all; see
 * `router.ts` and `http.ts`'s `csrfOkWithoutOrigin`.
 */

import { fromHex, readCookie, sessionIdOf, SESSION_COOKIE } from '@zesterm/cloud-shared';

import { ownsLiveHost } from '../db/registry.ts';
import type { Env } from '../env.ts';
import { json, jsonObject } from '../http.ts';
import { KEY_LEN } from '../enroll/preimage.ts';
import { mintAttachTicket, SIGNING_KEY_LEN } from '../relay/ticket.ts';
import { currentUser } from './session.ts';

export async function mintRelayTicket(request: Request, env: Env, now: number): Promise<Response> {
  const user = await currentUser(request, env, now);
  if (user === null) return json({ error: 'unauthorized' }, 401);

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

  if (!(await ownsLiveHost(env.DB, hostId, user.id))) return json({ error: 'not_found' }, 404);

  // Checked *after* ownership, and that order is the point: a deployment with
  // no relay still answers a stranger's host with the same 404 everything else
  // does. Checked first, the 503 would arrive for hosts that do not exist and
  // the 404 only for hosts that do — an oracle for which machines are enrolled,
  // handed out by a route that has otherwise been careful not to be one.
  const signingKey = fromHex(env.TICKET_SIGNING_KEY ?? '', SIGNING_KEY_LEN);
  if (signingKey === null) return json({ error: 'relay_unavailable' }, 503);

  // The *session*, not the browser's device enrolment: nothing in the schema
  // links the two, and this is the closest thing the cookie path has to "which
  // browser asked". Re-derived from the cookie rather than threaded out of
  // `currentUser`, which answers who rather than which session — one more
  // SHA-256 against a round trip that already happened.
  const dev = await sessionIdOf(readCookie(request.headers.get('cookie'), SESSION_COOKIE) ?? '');

  const minted = await mintAttachTicket({ signingKey, host: hostId, user: user.id, dev, now });
  return json(minted);
}

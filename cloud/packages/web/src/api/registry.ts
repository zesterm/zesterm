/**
 * The devices screen's data: what this account owns, and taking it away again.
 *
 * Every route here is the caller's own or nothing. "Own" is never a check made
 * after the read — `user_id` is in the `WHERE` clause of each statement in
 * `db/registry.ts` — so there is no shape here in which forgetting one line
 * turns a listing into everybody's.
 */

import { fromHex } from '@zesterm/cloud-shared';

import { listDevices, listHosts, revokeDevice, revokeHost } from '../db/registry.ts';
import type { EnrollKind } from '../db/types.ts';
import type { Env } from '../env.ts';
import { json } from '../http.ts';
import { KEY_LEN } from '../enroll/preimage.ts';
import { currentUser } from './session.ts';

/** `GET /api/hosts` and `GET /api/devices`. Revoked rows are simply not there. */
export async function listRegistry(
  request: Request,
  env: Env,
  kind: EnrollKind,
  now: number,
): Promise<Response> {
  const user = await currentUser(request, env, now);
  if (user === null) return json({ error: 'unauthorized' }, 401);

  return kind === 'host'
    ? json({ hosts: await listHosts(env.DB, user.id) })
    : json({ devices: await listDevices(env.DB, user.id) });
}

/**
 * `POST /api/hosts/:id/revoke` and `POST /api/devices/:id/revoke`.
 *
 * Idempotent, and it answers with the revocation's *own* timestamp rather than
 * `now` — a second click on a slow page must not rewrite when this machine
 * stopped being trusted, because that instant is the thing anyone asking later
 * wants to know.
 *
 * A key that is not this account's is a 404, exactly like one that does not
 * exist. The two are the same answer on purpose: the ids are public keys, so an
 * endpoint that distinguished them would answer "is this key enrolled with
 * zesterm" for keys belonging to strangers.
 */
export async function revokeRegistryEntry(
  request: Request,
  env: Env,
  kind: EnrollKind,
  id: string,
  now: number,
): Promise<Response> {
  const user = await currentUser(request, env, now);
  if (user === null) return json({ error: 'unauthorized' }, 401);

  // Shape first, so a mistyped path costs no round trip — and lowercase-only,
  // because `hosts.id` is stored as `hex()` writes it and an uppercase spelling
  // of a key is a different primary key rather than the same machine.
  if (fromHex(id, KEY_LEN) === null) return json({ error: 'not_found' }, 404);

  const revokedAt =
    kind === 'host'
      ? await revokeHost(env.DB, id, user.id, now)
      : await revokeDevice(env.DB, id, user.id, now);

  return revokedAt === null ? json({ error: 'not_found' }, 404) : json({ id, revokedAt });
}

/**
 * The devices screen's data: what this account owns, and taking it away again.
 *
 * Every route here is the caller's own or nothing. "Own" is never a check made
 * after the read — `user_id` is in the `WHERE` clause of each statement in
 * `db/registry.ts` — so there is no shape here in which forgetting one line
 * turns a listing into everybody's.
 */

import { fromHex } from '@zesterm/cloud-shared';

import { revokeAttestationsFor } from '../db/attestations.ts';
import {
  listDevices,
  listHosts,
  restoreDevice,
  restoreHost,
  revokeDevice,
  revokeHost,
} from '../db/registry.ts';
import type { EnrollKind } from '../db/types.ts';
import type { Env } from '../env.ts';
import { json } from '../http.ts';
import { KEY_LEN } from '../enroll/preimage.ts';
import { currentUser } from './session.ts';
import { requestPrincipal } from './principal.ts';

/**
 * `GET /api/hosts` and `GET /api/devices`. Revoked rows are simply not there —
 * unless the *owner* asks with `?include=revoked`, which is the recovery view
 * the restore route exists for. Cookie principals only: a machine reads these
 * lists to reach the machines that exist, and which keys were cast out is
 * account history, not something a bearer credential needs.
 *
 * `/api/hosts` answers people and **devices** — the desktop app reads its
 * fleet with a bearer token — and carries `relayOrigin` beside the list,
 * because a bearer caller has no session to fetch `/api/bootstrap` with and
 * the hosts are unreachable without it. A *host's* token gets the 401 an
 * absent credential gets: a machine serving shells has no business
 * enumerating its owner's other machines.
 *
 * `/api/devices` answers people and **devices** too, since the desktop app
 * became an approver (#190): its fleet screen renders the account's devices
 * and vouches from pending rows, and it reads them with the only credential
 * it has. The widening this comment used to defer is exactly this decision,
 * and it is sound because the resolve join (`db/machine-tokens.ts`) admits
 * only *approved* devices — a pending key's token cannot enumerate the
 * account it is not yet trusted by. Hosts stay refused on both lists.
 */
export async function listRegistry(
  request: Request,
  env: Env,
  kind: EnrollKind,
  now: number,
): Promise<Response> {
  const principal = await requestPrincipal(request, env, now);
  if (principal === null) return json({ error: 'unauthorized' }, 401);

  // The owner's opt-in, and nobody else's: a bearer asking gets the ordinary
  // live view, silently — refusing outright would turn a copy-pasted URL into
  // an error for a caller the default answer serves fine.
  const includeRevoked =
    principal.kind === 'user' &&
    new URL(request.url).searchParams.get('include') === 'revoked';

  if (kind === 'host') {
    if (principal.kind === 'host') return json({ error: 'unauthorized' }, 401);
    const userId = principal.kind === 'user' ? principal.user.id : principal.userId;
    return json({
      hosts: await listHosts(env.DB, userId, now, includeRevoked),
      relayOrigin: env.RELAY_ORIGIN ?? null,
    });
  }

  // A host's token stays refused here as on `/api/hosts`, and for the same
  // reason with more teeth: this list is the approval surface, and a machine
  // serving shells has no business learning which keys are pending.
  if (principal.kind === 'host') return json({ error: 'unauthorized' }, 401);
  const userId = principal.kind === 'user' ? principal.user.id : principal.userId;
  return json({ devices: await listDevices(env.DB, userId, includeRevoked) });
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

  if (kind === 'device' && revokedAt !== null) {
    // Revoking a device takes its attestations with it — the ones vouching
    // FOR it and the ones it made as an approver. Here rather than inside
    // `revokeDevice`, because the attestation table's liveness is not a JOIN
    // against `devices` (a voucher names two devices, and joining on both
    // would make each read pay for the rule); the flag is written once, at
    // the only place a device revocation happens.
    await revokeAttestationsFor(env.DB, id, user.id, now);
  }

  return revokedAt === null ? json({ error: 'not_found' }, 404) : json({ id, revokedAt });
}

/**
 * `POST /api/hosts/:id/restore` and `POST /api/devices/:id/restore`.
 *
 * The act the enrolment comments promise — "un-revoking is an act by the
 * owner, in the browser" — so it is cookie-only on purpose: `currentUser`,
 * never `requestPrincipal`. A machine credential must not be able to un-revoke
 * its own principal; that a revoked key cannot bring itself back is the whole
 * point of the 409 it gets at enrolment, and a bearer here would be that key
 * bringing itself back with one extra request.
 *
 * Restoring clears the row's `revoked_at` and nothing else — see
 * `restoreHost` for why that alone puts the machine back on the account.
 * Idempotent like revoke; 404 collapses "not yours" and "not enrolled" for
 * revoke's reason.
 */
export async function restoreRegistryEntry(
  request: Request,
  env: Env,
  kind: EnrollKind,
  id: string,
  now: number,
): Promise<Response> {
  const user = await currentUser(request, env, now);
  if (user === null) return json({ error: 'unauthorized' }, 401);

  if (fromHex(id, KEY_LEN) === null) return json({ error: 'not_found' }, 404);

  const restored =
    kind === 'host'
      ? await restoreHost(env.DB, id, user.id)
      : await restoreDevice(env.DB, id, user.id);

  return restored ? json({ id }) : json({ error: 'not_found' }, 404);
}

/**
 * The browser hand-off sign-in (#226): a device asks, a person approves in
 * their browser, the device claims.
 *
 * Three authentications, none shared. `start` and `claim` are ORIGINLESS —
 * they read no cookie and are authenticated by Ed25519 signatures over the
 * `zesterm-link-v1` messages, the `/api/enroll/claim` invariant — while
 * `approve`, `deny` and the details read are ordinary cookie routes under the
 * full CSRF rule, because the approval IS the account-holder's act.
 *
 * The grant is **pre-account**: the row has no user until someone approves,
 * and whoever approves decides whose account the device joins. That is the
 * feature, not a hole — the person opened the link in the browser that holds
 * their session, the pairing-code discipline moved one step earlier. The
 * fingerprint on the approval page is the anti-phishing half: the app shows
 * the same eight hex characters while it waits, and a person can compare.
 */

import { fromHex, hex } from '@zesterm/cloud-shared';

import {
  approveLinkGrant,
  claimLinkGrant,
  denyLinkGrant,
  findLiveLinkGrant,
  looksLikeLinkGrant,
  newLinkGrantId,
  startLinkGrant,
  LINK_GRANT_TTL_MS,
} from '../db/link.ts';
import { enrolDevice, existingDevice } from '../db/registry.ts';
import { createMachineToken } from '../db/machine-tokens.ts';
import { findUser } from '../db/users.ts';
import type { DeviceKind } from '../db/types.ts';
import type { Env } from '../env.ts';
import { json, jsonObject } from '../http.ts';
import { KEY_LEN, SIGNATURE_LEN } from '../enroll/preimage.ts';
import { verifyLinkClaim, verifyLinkRequest } from '../enroll/link-preimage.ts';
import { DEVICE_KINDS, labelOk, platformOk } from './enroll.ts';
import { currentUser } from './session.ts';

/**
 * How much of the key the approval page shows: first 8 hex, rendered by the
 * UI as `ab12 cd34`. Eight characters because a person is *comparing*, not
 * looking up — the app displays the same eight while it waits, and the
 * comparison only has to defeat a phisher racing to show a different key,
 * who cannot grind a matching prefix inside a ten-minute grant.
 */
const FINGERPRINT_CHARS = 8;

/**
 * `POST /api/link/start` — the app asks for a grant, proving it holds the key.
 *
 * ORIGINLESS, and the invariant that makes that sound is the claim's: this
 * handler never reads the session cookie. What it costs us is one
 * unauthenticated insert — bounded honestly rather than waved away: the
 * signature means each row costs the asker a real Ed25519 keypair,
 * `UNIQUE(device_id)` means a key holds at most one live row however often
 * it asks (asking again rotates, the machine-token discipline), and the TTL
 * reaps the rest. No account-level bound exists because there is no account
 * yet — that is the point of the flow.
 */
export async function startLink(request: Request, env: Env, now: number): Promise<Response> {
  const body = await jsonObject(request);
  if (body === null) return json({ error: 'bad_request' }, 400);

  const { deviceId, label, kind, platform, sig } = body as {
    deviceId?: unknown;
    label?: unknown;
    kind?: unknown;
    platform?: unknown;
    sig?: unknown;
  };

  // Shape first, so junk costs no database round trip — the claim's order.
  const key = typeof deviceId === 'string' ? fromHex(deviceId, KEY_LEN) : null;
  if (key === null) return json({ error: 'bad_request', detail: 'deviceId' }, 400);
  if (typeof label !== 'string' || !labelOk(label)) {
    return json({ error: 'bad_request', detail: 'label' }, 400);
  }
  // `desktop` as the default: this flow exists for the app that can open a
  // browser beside itself, and a browser signs in with the session it has.
  if (kind !== undefined && !DEVICE_KINDS.includes(kind as DeviceKind)) {
    return json({ error: 'bad_request', detail: 'kind' }, 400);
  }
  if (platform !== undefined && (typeof platform !== 'string' || !platformOk(platform))) {
    return json({ error: 'bad_request', detail: 'platform' }, 400);
  }
  const signature = typeof sig === 'string' ? fromHex(sig, SIGNATURE_LEN) : null;
  if (signature === null) return json({ error: 'bad_request', detail: 'sig' }, 400);

  if (!(await verifyLinkRequest({ key, label, signature }))) {
    return json({ error: 'bad_signature' }, 400);
  }

  // Deliberately no look at the devices table: answering differently for a
  // revoked or foreign key would make this unauthenticated route an oracle
  // for "is this key enrolled with zesterm". The claim checks incumbents at
  // spend time, where the caller has at least proven the key twice.
  const id = newLinkGrantId();
  await startLinkGrant(env.DB, {
    id,
    deviceId: hex(key),
    label,
    kind: (kind as DeviceKind | undefined) ?? 'desktop',
    platform: typeof platform === 'string' ? platform : '',
    now,
  });
  return json({ grant: id, expiresAt: now + LINK_GRANT_TTL_MS });
}

/**
 * `GET /api/link/:id` — what the approval page renders.
 *
 * Cookie-only: the reader is the person about to approve, and an anonymous
 * reader would turn grant ids into an unauthenticated probe. Unknown, expired
 * and claimed are one 404 — the id is briefly a capability, and a liveness
 * oracle is a free search for one.
 */
export async function getLinkGrant(request: Request, env: Env, id: string, now: number): Promise<Response> {
  const user = await currentUser(request, env, now);
  if (user === null) return json({ error: 'unauthorized' }, 401);

  if (!looksLikeLinkGrant(id)) return json({ error: 'not_found' }, 404);
  const grant = await findLiveLinkGrant(env.DB, id, now);
  if (grant === null) return json({ error: 'not_found' }, 404);

  return json({
    label: grant.label,
    kind: grant.kind,
    platform: grant.platform,
    fingerprint: grant.device_id.slice(0, FINGERPRINT_CHARS),
    // So a re-opened page can say "already approved -- return to the app"
    // instead of offering a button that would be a no-op.
    approved: grant.approved_at !== null,
    expiresAt: grant.expires_at,
  });
}

/**
 * `POST /api/link/:id/approve` — the account-holder's act.
 *
 * Whoever approves decides the account, so this is a full-CSRF cookie route
 * and nothing else. Idempotent: re-approving answers the first approval's
 * timestamp and never moves the grant to a second account (`COALESCE` in the
 * store).
 */
export async function approveLink(request: Request, env: Env, id: string, now: number): Promise<Response> {
  const user = await currentUser(request, env, now);
  if (user === null) return json({ error: 'unauthorized' }, 401);

  if (!looksLikeLinkGrant(id)) return json({ error: 'not_found' }, 404);
  const approved = await approveLinkGrant(env.DB, id, user.id, now);
  if (approved === null) return json({ error: 'not_found' }, 404);
  return json({ approvedAt: approved.approved_at });
}

/**
 * `POST /api/link/:id/deny` — deletion, not a status.
 *
 * A denied grant has no history worth keeping (nothing was ever enrolled),
 * and deleting frees the key to ask again. The second deny is therefore a
 * 404, which is honest: there is no longer anything to deny.
 */
export async function denyLink(request: Request, env: Env, id: string, now: number): Promise<Response> {
  const user = await currentUser(request, env, now);
  if (user === null) return json({ error: 'unauthorized' }, 401);

  if (!looksLikeLinkGrant(id)) return json({ error: 'not_found' }, 404);
  const denied = await denyLinkGrant(env.DB, id, now);
  return denied ? json({ denied: true }) : json({ error: 'not_found' }, 404);
}

/**
 * `POST /api/link/claim` — the app polls, then spends.
 *
 * ORIGINLESS like `start`, and reading no cookie for the same reason. The
 * order is the code claim's, with one addition:
 *
 * 1. **Shape**, so junk costs no round trip.
 * 2. **The claim signature verifies** — before the grant is even read, so a
 *    caller without the key learns nothing about any grant id, not even that
 *    one exists.
 * 3. **The grant is live and names this key.** Unknown, expired, denied
 *    (deleted), already-claimed and someone-else's-grant are ONE collapsed
 *    refusal — `invalid_code`'s discipline; a grant id must not be a
 *    liveness oracle.
 * 4. **Not yet approved is `{status:'pending'}`, 200** — the app polls, and
 *    waiting is the expected state of this endpoint, not an error.
 * 5. **Incumbents**, before spending, so a refusal leaves the grant alive:
 *    another account's key or a revoked one is the same 409 the code claim
 *    answers.
 * 6. **CAS the claim, then enrol** — spending first makes a replay inert.
 *    The device is born **approved**: the person clicked Approve in their
 *    session minutes ago, the same explicit act a typed code is.
 *    `approved_by` stays NULL — that column names an attesting *device*, and
 *    this approval, like a code, came from a session.
 */
export async function claimLink(request: Request, env: Env, now: number): Promise<Response> {
  const body = await jsonObject(request);
  if (body === null) return json({ error: 'bad_request' }, 400);

  const { grant, deviceId, sig } = body as {
    grant?: unknown;
    deviceId?: unknown;
    sig?: unknown;
  };

  if (typeof grant !== 'string' || !looksLikeLinkGrant(grant)) {
    return json({ error: 'bad_request', detail: 'grant' }, 400);
  }
  const key = typeof deviceId === 'string' ? fromHex(deviceId, KEY_LEN) : null;
  if (key === null) return json({ error: 'bad_request', detail: 'deviceId' }, 400);
  const signature = typeof sig === 'string' ? fromHex(sig, SIGNATURE_LEN) : null;
  if (signature === null) return json({ error: 'bad_request', detail: 'sig' }, 400);

  // One answer for everything a claimant must not be able to tell apart.
  const refused = () => json({ error: 'invalid_grant' }, 400);

  if (!(await verifyLinkClaim({ grant, key, signature }))) return refused();

  const id = hex(key);
  const row = await findLiveLinkGrant(env.DB, grant, now);
  // The device check is part of the collapse, and it sits BEFORE the pending
  // answer: only the key that asked may even learn the grant is waiting.
  if (row === null || row.device_id !== id) return refused();

  if (row.approved_at === null) return json({ status: 'pending' });

  const incumbent = await existingDevice(env.DB, id);
  const owner = row.approved_by_user;
  if (owner === null) {
    // approved_at without approved_by_user cannot be written by any path
    // here; refusing beats enrolling into an account nobody named.
    return refused();
  }
  if (incumbent !== null && (incumbent.user_id !== owner || incumbent.revoked_at !== null)) {
    // The code claim's two-situations-one-answer, verbatim: another account's
    // key is not this caller's business, and a revoked key that could re-link
    // itself has un-revoked itself (the owner un-revokes with the restore
    // route, #365). Checked before the spend, so the refusal leaves the grant
    // for whoever debugs it rather than burning it.
    return json({ error: 'already_enrolled' }, 409);
  }

  if ((await claimLinkGrant(env.DB, grant, id, now)) === null) {
    // Approved a moment ago, gone now: another claim won the race, or the
    // grant expired between the read and the write. Same fact, same answer.
    return refused();
  }

  const enrolled = await enrolDevice(env.DB, {
    id,
    userId: owner,
    label: row.label,
    kind: row.kind,
    now,
  });
  if (enrolled === null) {
    // Revoked in the window between the incumbent check and the insert. The
    // grant is spent, which is the safe direction: a refusal that leaves it
    // alive is a refusal an attacker can retry.
    return json({ error: 'already_enrolled' }, 409);
  }

  // The credential, the account name to print, and the same envelope the code
  // claim answers — so the app has ONE success shape however it signed in.
  const minted = await createMachineToken(env.DB, {
    userId: owner,
    kind: 'device',
    principalId: id,
    now,
  });
  const account = await findUser(env.DB, owner);
  return json({
    device: enrolled,
    token: minted.token,
    account: account === null ? null : account.display_name || account.id,
  });
}

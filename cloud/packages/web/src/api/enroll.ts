/**
 * Enrolment: minting a one-shot code, and spending it.
 *
 * The two halves are deliberately asymmetric, and the asymmetry is the design.
 * Minting is an act by a **person** and needs their session; claiming is an act
 * by a **machine**, which has no cookie, no browser and no origin — it has a
 * key. So one route is cookie-authenticated and the other is authenticated by a
 * code the account minted plus a signature over it, and neither one accepts the
 * other's proof.
 *
 * Neither half of the claim is sufficient alone, which is worth restating
 * because it is what decides the order of the checks below. A code with no
 * signature enrols whoever read it over your shoulder; a signature over no code
 * enrols a key nobody approved.
 */

import { fromHex, hex } from '@zesterm/cloud-shared';

import {
  createEnrollCode,
  enrolDevice,
  enrolHost,
  existingDevice,
  existingHost,
  findLiveEnrollCode,
  spendEnrollCode,
} from '../db/registry.ts';
import type { DeviceKind, EnrollKind } from '../db/types.ts';
import type { Env } from '../env.ts';
import { json, jsonObject } from '../http.ts';
import { createMachineToken } from '../db/machine-tokens.ts';
import { findUser } from '../db/users.ts';
import { looksLikeEnrollCode } from '../enroll/codes.ts';
import { KEY_LEN, SIGNATURE_LEN, verifyEnrollment } from '../enroll/preimage.ts';
import { requestPrincipal } from './principal.ts';

/** Exported for `devices.ts`: registration renders into the same screen. */
export const DEVICE_KINDS: readonly DeviceKind[] = ['browser', 'phone', 'desktop'];

/**
 * A label is shown to a person and never parsed, so the only bounds that matter
 * are "fits on a row" and "cannot smuggle control characters into a log line".
 *
 * Counted in **code points**, not `length`: `label.length` is UTF-16 code
 * units, so one astral-plane emoji counts as two and a name that renders as ten
 * characters is refused at what looks like the wrong size.
 */
const LABEL_MAX = 64;
const PLATFORM_MAX = 64;

/** C0, DEL and C1 — everything that could move a cursor in a log or a terminal. */
const CONTROL = /[\u0000-\u001f\u007f-\u009f]/;

export function labelOk(label: string): boolean {
  const points = [...label];
  if (points.length === 0 || points.length > LABEL_MAX) return false;
  return !CONTROL.test(label);
}

/**
 * Same screening as a label, and for the same reason.
 *
 * `platform` is attacker-controlled — it is *not* covered by the enrolment
 * signature — and it lands in the same devices screen and the same log lines a
 * label does. Length-checking it while screening the label was an
 * inconsistency an attacker only has to notice once: an ESC here smuggles the
 * terminal control sequence the label guard exists to stop.
 *
 * Empty is allowed, unlike a label: a daemon that does not know its platform
 * omits the field, and that is not an error.
 */
export function platformOk(platform: string): boolean {
  return [...platform].length <= PLATFORM_MAX && !CONTROL.test(platform);
}

/**
 * `POST /api/enroll/code` — mint a code for the signed-in person to carry,
 * or for their desktop app to carry on their behalf (issue #227).
 *
 * The response is the *only* time the code exists outside the database in a
 * readable form, which is fine: the row is what makes it spendable, and a code
 * without its row is eight characters of nothing.
 *
 * **Who may mint what — the policy, in one place.** A signed-in person mints
 * either kind. An **approved device** bearer (the desktop app; approval is
 * `resolveMachineToken`'s floor) mints `host` codes only: adding machines is
 * the "Enroll this machine" button's whole point, but a leaked app token
 * must not be able to mint credentials for *further devices* — that would
 * turn one stolen token into a stream of them. A host token mints nothing:
 * machines do not add machines.
 */
export async function mintEnrollCode(request: Request, env: Env, now: number): Promise<Response> {
  const principal = await requestPrincipal(request, env, now);
  if (principal === null) return json({ error: 'unauthorized' }, 401);

  const body = await jsonObject(request);
  const kind = body?.['kind'];
  if (kind !== 'host' && kind !== 'device') {
    return json({ error: 'bad_request', detail: "kind must be 'host' or 'device'" }, 400);
  }

  if (principal.kind === 'host') {
    return json({ error: 'forbidden', detail: 'a host token cannot mint enroll codes' }, 403);
  }
  if (principal.kind === 'device' && kind !== 'host') {
    return json(
      { error: 'forbidden', detail: 'a device token can only mint host codes' },
      403,
    );
  }

  const userId = principal.kind === 'user' ? principal.user.id : principal.userId;
  const minted = await createEnrollCode(env.DB, userId, kind, now);
  return json(minted);
}

/**
 * `POST /api/enroll/claim` — a machine answers a code with a signature.
 *
 * **Unauthenticated in the session sense, and it must stay that way.** It reads
 * no cookie: the router exempts it from the `Origin` half of the CSRF rule
 * precisely because it has no ambient authority to forge, and a route that took
 * both a cookie and an exemption would be the hole that rule exists to close.
 *
 * The order of the checks is the point of the whole function:
 *
 * 1. **Shape**, so junk costs no database round trip.
 * 2. **The code is live** — read only. Unknown, expired and already-spent are
 *    one answer here; the database still tells them apart (`used_at` is set,
 *    the row is not deleted) for whoever debugs it later.
 * 3. **The signature verifies** — still no write. A wrong signature must not
 *    burn a legitimate code, or anyone who can reach this endpoint can make a
 *    person mint codes until they give up, without holding any key at all.
 * 4. **The key may enrol at all** — before spending, so a refusal leaves the
 *    code usable by the machine that was supposed to answer it.
 * 5. **Spend, then enrol.** Spending first is what makes a replay inert: the
 *    conditional `UPDATE` is atomic, so a second identical claim matches no row
 *    and stops. The reverse order would let two concurrent claims both insert
 *    and leave one of them to be undone.
 */
export async function claimEnrollCode(request: Request, env: Env, now: number): Promise<Response> {
  const body = await jsonObject(request);
  if (body === null) return json({ error: 'bad_request' }, 400);

  const { code, hostId, label, sig, platform, deviceKind } = body as {
    code?: unknown;
    hostId?: unknown;
    label?: unknown;
    sig?: unknown;
    platform?: unknown;
    deviceKind?: unknown;
  };

  if (typeof code !== 'string' || !looksLikeEnrollCode(code)) {
    return json({ error: 'bad_request', detail: 'code' }, 400);
  }
  if (typeof label !== 'string' || !labelOk(label)) {
    return json({ error: 'bad_request', detail: 'label' }, 400);
  }
  // `hostId` names the common case and carries a `ClientId` when the code's
  // kind is `device`; the field is the one the daemon sends, and renaming it
  // per kind would mean the daemon has to know which kind the code was minted
  // for before it can format the request.
  const key = typeof hostId === 'string' ? fromHex(hostId, KEY_LEN) : null;
  if (key === null) return json({ error: 'bad_request', detail: 'hostId' }, 400);

  const signature = typeof sig === 'string' ? fromHex(sig, SIGNATURE_LEN) : null;
  if (signature === null) return json({ error: 'bad_request', detail: 'sig' }, 400);

  // Neither is signed, so neither may ever decide anything: they are what the
  // devices screen renders, and an attacker in the path can change them without
  // invalidating the signature. Bounded so they cannot be used as storage.
  if (
    platform !== undefined &&
    (typeof platform !== 'string' || !platformOk(platform))
  ) {
    return json({ error: 'bad_request', detail: 'platform' }, 400);
  }
  if (deviceKind !== undefined && !DEVICE_KINDS.includes(deviceKind as DeviceKind)) {
    return json({ error: 'bad_request', detail: 'deviceKind' }, 400);
  }

  // One answer for a dead code and for a bad signature, and it is deliberate.
  //
  // Answering `invalid_code` here and `bad_signature` below told an
  // unauthenticated caller whether a code was live, for free and without
  // spending it. That matters more than it first looks: the signature proves
  // only that the claimant holds the key it names, not that they were meant to
  // have the code — so anyone holding a live code can enrol *their own*
  // machine into the account that minted it. The code is the bearer token, and
  // a free liveness oracle is a free search for one.
  //
  // A legitimate daemon loses nothing: both outcomes mean "this enrolment did
  // not take, get a fresh code", which is the same next step either way.
  const refused = () => json({ error: 'invalid_code' }, 400);

  const codeRow = await findLiveEnrollCode(env.DB, code, now);
  if (codeRow === null) return refused();

  // The role is taken from the code the *account* minted, never from the
  // request. It lives inside the signing prefix, so a signature made to enrol a
  // machine is not a signature that enrols a browser key — and letting the
  // claimant name the role would hand that separation straight back.
  const role = codeRow.kind === 'host' ? 'host' : 'client';
  if (!(await verifyEnrollment({ role, code, key, label, signature }))) {
    // One deliberate crack in the no-oracle rule (#228): a signature that
    // verifies under the *other* role proves the caller signed over this very
    // code — they typed a machine code into the app, or the reverse — and
    // "mint a fresh one" sends them straight back to mint another of the same
    // wrong kind. Naming the kind grants nothing new: a live code is a bearer
    // credential whoever holds it can already spend under its own kind (the
    // comment above `refused`), so the most an attacker gains here is
    // learning a code is live one request before spending it — a request they
    // would have spent anyway. A dead code still verifies under neither role
    // and lands in `refused`, exactly as before.
    const other = role === 'host' ? 'client' : 'host';
    if (await verifyEnrollment({ role: other, code, key, label, signature })) {
      return json({ error: 'wrong_kind', kind: codeRow.kind }, 400);
    }
    // Same answer as an unknown code -- see `refused` above.
    return refused();
  }

  const id = hex(key);
  const kind: EnrollKind = codeRow.kind;
  const incumbent =
    kind === 'host' ? await existingHost(env.DB, id) : await existingDevice(env.DB, id);
  if (
    incumbent !== null &&
    (incumbent.user_id !== codeRow.user_id || incumbent.revoked_at !== null)
  ) {
    // Two different situations, one answer. Another account's key is not this
    // caller's business, and a revoked one is refused because revocation is a
    // positive statement: a machine that could re-enrol itself with a fresh
    // code has un-revoked itself, which is the one thing the schema's comment
    // says must not happen. Un-revoking is an act by the owner, in the browser.
    return json({ error: 'already_enrolled' }, 409);
  }

  if ((await spendEnrollCode(env.DB, code, id, now)) === null) {
    // Live a moment ago, gone now: another claim won the race. Same answer as
    // an unknown code, because it is the same fact — this code is spent.
    return refused();
  }

  const enrolled =
    kind === 'host'
      ? await enrolHost(env.DB, {
          id,
          userId: codeRow.user_id,
          label,
          platform: typeof platform === 'string' ? platform : '',
          now,
        })
      : await enrolDevice(env.DB, {
          id,
          userId: codeRow.user_id,
          label,
          // A code typed into a machine is being typed into an app or a phone;
          // a browser enrols with the session it already has and never comes
          // through here. Recorded as the default rather than guessed at read
          // time, so the devices screen renders one fact rather than two.
          kind: (deviceKind as DeviceKind | undefined) ?? 'desktop',
          now,
        });

  if (enrolled === null) {
    // The guard in `enrolHost`/`enrolDevice` fired between the check above and
    // the insert — the owner revoked the key in that window. The code is spent,
    // which is the safe direction: a refusal that leaves the code alive is a
    // refusal an attacker can retry.
    return json({ error: 'already_enrolled' }, 409);
  }

  // The credential the machine keeps. Minted *after* the enrol so a failure
  // above leaves no orphaned token, and returned exactly once — the daemon
  // stores it in the OS credential store and this response is the only place
  // it ever exists in the clear.
  const minted = await createMachineToken(env.DB, {
    userId: codeRow.user_id,
    kind,
    principalId: id,
    now,
  });

  // For the machine to print — "enrolled with <account>" — never to decide
  // anything. The row is this account's by construction (the code named it).
  // `display_name` can legitimately be the empty string, and "enrolled with"
  // followed by nothing reads as a bug on the machine's console — the stable
  // user id is the fallback that is always printable.
  const owner = await findUser(env.DB, codeRow.user_id);
  const account = owner === null ? null : owner.display_name || owner.id;

  // `{ host }` or `{ device }`, the same shape the listings use, rather than a
  // spread beside a `kind` field. A `PublicDevice` has a `kind` of its own —
  // `'phone'` — so the spread quietly overwrote the envelope's, and a caller
  // switching on it read the wrong answer for every device.
  return kind === 'host'
    ? json({ host: enrolled, token: minted.token, account })
    : json({ device: enrolled, token: minted.token, account });
}

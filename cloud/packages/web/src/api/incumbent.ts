/**
 * The one incumbent check every enrolment path runs (#367).
 *
 * Three routes let a key join an account — the code claim, the browser
 * registration, the link claim — and each used to restate this rule inline,
 * six times counting the post-write races. One copy, because the copies were
 * already drifting toward it: the rule is subtle in exactly one place (the
 * ordering below) and a seventh restatement is where that gets lost.
 *
 * The refusal names its cause where the old answer collapsed two that need
 * opposite moves: `revoked` is fixed by the owner restoring the row (#365),
 * `other_account` by going to the account that holds it. Naming them is safe
 * on every path this serves, because each call site sits after a verified
 * signature over the key — the detail describes only a key the caller
 * provably holds, never one they merely typed in.
 *
 * `other_account` is checked FIRST, and the order is load-bearing: a
 * stranger's revoked row must answer `other_account`, because that the row
 * was revoked — or that any account ever held it — is a fact about somebody
 * else's fleet.
 */

import type { Incumbent } from '../db/registry.ts';
import { json } from '../http.ts';

export function incumbentRefusal(incumbent: Incumbent | null, ownerUserId: string): Response | null {
  if (incumbent === null) return null;
  if (incumbent.user_id !== ownerUserId) {
    return json({ error: 'already_enrolled', detail: 'other_account' }, 409);
  }
  if (incumbent.revoked_at !== null) {
    return json({ error: 'already_enrolled', detail: 'revoked' }, 409);
  }
  return null;
}

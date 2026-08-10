/**
 * "Who is asking", for the routes that require an answer.
 *
 * One function, so that every signed-in route resolves the session the same
 * way. `/api/bootstrap` and `/api/me` deliberately answer `null` rather than
 * 401 — being signed out is not an error there — so they keep calling
 * `resolveSession` directly.
 */

import { readCookie, SESSION_COOKIE } from '@zesterm/cloud-shared';

import { resolveSession } from '../db/sessions.ts';
import type { Env } from '../env.ts';
import type { PublicUser } from '../db/types.ts';

export async function currentUser(
  request: Request,
  env: Env,
  now: number,
): Promise<PublicUser | null> {
  return resolveSession(env.DB, readCookie(request.headers.get('cookie'), SESSION_COOKIE), now);
}

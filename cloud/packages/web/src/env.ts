/**
 * The Worker's bindings, in one place.
 *
 * Secrets are `wrangler secret put`, never `vars` in `wrangler.jsonc` — that
 * file is committed.
 */

import type { Db } from './db/types.ts';

export interface Env {
  /** The built web client. Serving the SPA, including its 404 fallback, is its job. */
  readonly ASSETS: { fetch(request: Request): Promise<Response> };
  /** D1 `zesterm`. */
  readonly DB: Db;

  /** This deployment's own origin, and the CSRF check's only reference point. */
  readonly APP_ORIGIN: string;

  readonly GITHUB_CLIENT_ID: string;
  // --- secrets ---
  readonly GITHUB_CLIENT_SECRET: string;
  /** HMAC key for the signed OAuth state cookie. */
  readonly COOKIE_MAC_KEY: string;
}

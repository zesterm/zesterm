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

  /**
   * The relay Worker's origin, or absent where there is no relay.
   *
   * Optional because "no relay" is a real deployment rather than a broken one:
   * `/api/bootstrap` reports `relayOrigin: null` and the app still reaches
   * every machine it can see over `ws`. Public, so it lives in `vars`.
   */
  readonly RELAY_ORIGIN?: string;

  readonly GITHUB_CLIENT_ID: string;
  // --- secrets ---
  readonly GITHUB_CLIENT_SECRET: string;
  /** HMAC key for the signed OAuth state cookie. */
  readonly COOKIE_MAC_KEY: string;
  /**
   * The Ed25519 seed attach tickets are signed with, 64 hex characters. Its
   * public half is what the relay Worker verifies against, and the two are
   * deployed to different Workers on purpose: nothing that can serve a
   * stylesheet can also mint admission to a room.
   *
   * Optional for the same reason `RELAY_ORIGIN` is, and in exactly the same
   * deployments: with no relay there is nothing to mint admission to, and
   * `POST /api/relay/ticket` answers 503 while every other route works. A
   * mandatory declaration would make that state a lie the type tells rather
   * than one the route handles.
   */
  readonly TICKET_SIGNING_KEY?: string;
}

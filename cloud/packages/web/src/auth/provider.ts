/**
 * What a provider has to supply, and nothing more.
 *
 * The shape exists so that adding Google is one file, one registry entry, one
 * secret and one redirect URI — rather than a second copy of the state cookie,
 * the callback route and the session creation, which is where the bugs would
 * be. Everything security-shaped is provider-blind and lives in `routes.ts`.
 */

import type { Profile } from '../db/users.ts';

export interface TokenResponse {
  readonly access_token?: string;
  /** OIDC providers return the identity here instead of at a userinfo endpoint. */
  readonly id_token?: string;
  readonly error?: string;
  readonly error_description?: string;
}

export interface ProviderEnv {
  readonly GITHUB_CLIENT_ID: string;
  readonly GITHUB_CLIENT_SECRET: string;
}

export interface OAuthProvider {
  readonly id: string;
  readonly authorizeUrl: string;
  readonly tokenUrl: string;
  readonly scopes: readonly string[];
  /**
   * GitHub ignores PKCE; Google wants it. Carried per provider so the shared
   * route always *generates* a verifier and only sends it where it means
   * something — a flow that sometimes has the field and sometimes does not is
   * how one gets forgotten.
   */
  readonly usePkce: boolean;
  clientId(env: ProviderEnv): string;
  clientSecret(env: ProviderEnv): string;
  /** Exchange complete; turn the tokens into an identity. */
  profile(tokens: TokenResponse, fetchImpl: typeof fetch): Promise<Profile>;
}

export class OAuthError extends Error {
  /** Safe to show a person. The message itself may name internals. */
  readonly userMessage: string;

  // Declared and assigned rather than a parameter property: `erasableSyntaxOnly`
  // forbids those, because they are the one bit of TypeScript that emits code
  // and this workspace runs its tests through Node's type stripping.
  constructor(message: string, userMessage = 'Sign-in failed. Please try again.') {
    super(message);
    this.name = 'OAuthError';
    this.userMessage = userMessage;
  }
}

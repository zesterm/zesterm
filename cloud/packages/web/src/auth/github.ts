/**
 * GitHub, and the three things it does that cost an afternoon each.
 */

import type { Profile } from '../db/users.ts';
import { OAuthError, type OAuthProvider, type ProviderEnv, type TokenResponse } from './provider.ts';

interface GitHubUser {
  readonly id: number;
  readonly login: string;
  readonly name: string | null;
  readonly email: string | null;
  readonly avatar_url: string | null;
}

interface GitHubEmail {
  readonly email: string;
  readonly primary: boolean;
  readonly verified: boolean;
}

/**
 * Required, and its absence does not look like its cause.
 *
 * GitHub's API rejects a request with no `User-Agent` with a 403, and the body
 * reads as an authorization failure — so the obvious next move is to go and
 * check the token, which is fine.
 */
const UA = 'zesterm';

export const github: OAuthProvider = {
  id: 'github',
  authorizeUrl: 'https://github.com/login/oauth/authorize',
  tokenUrl: 'https://github.com/login/oauth/access_token',
  // `read:user` for the profile, `user:email` because the primary verified
  // address is not on /user reliably -- see below.
  scopes: ['read:user', 'user:email'],
  usePkce: false,
  clientId: (env: ProviderEnv) => env.GITHUB_CLIENT_ID,
  clientSecret: (env: ProviderEnv) => env.GITHUB_CLIENT_SECRET,

  async profile(tokens: TokenResponse, fetchImpl: typeof fetch): Promise<Profile> {
    const token = tokens.access_token;
    if (!token) throw new OAuthError('github returned no access_token');

    const headers = {
      authorization: `Bearer ${token}`,
      accept: 'application/vnd.github+json',
      'user-agent': UA,
    };

    const userRes = await fetchImpl('https://api.github.com/user', { headers });
    if (!userRes.ok) throw new OAuthError(`github /user failed: ${userRes.status}`);
    const user = (await userRes.json()) as GitHubUser;

    if (typeof user.id !== 'number') throw new OAuthError('github /user returned no numeric id');

    // `/user.email` is null whenever the address is private, and it is not
    // marked verified either way -- so the primary verified address comes from
    // /user/emails or not at all. `email_verified` is what the account-linking
    // rule turns on, so guessing here would be guessing about account takeover.
    let email: string | null = null;
    let emailVerified = false;

    const emailRes = await fetchImpl('https://api.github.com/user/emails', { headers });
    if (emailRes.ok) {
      const emails = (await emailRes.json()) as GitHubEmail[];
      const primary = emails.find((e) => e.primary && e.verified) ?? emails.find((e) => e.verified);
      if (primary) {
        email = primary.email;
        emailVerified = true;
      }
    }

    return {
      // The **numeric id**, as a string. Never `login`: logins are renameable
      // and reusable, so an account keyed on one is inherited by whoever claims
      // the name next.
      subject: String(user.id),
      email,
      emailVerified,
      displayName: user.name ?? user.login,
      avatarUrl: user.avatar_url,
    };
  },
};

/**
 * Parse a token response, treating GitHub's 200-with-an-error as the error it is.
 *
 * GitHub answers a failed exchange with **HTTP 200** and
 * `{"error":"bad_verification_code"}`. Checking `res.ok` alone therefore
 * reports success, and the failure surfaces much later as "no access_token" or
 * as a 401 from the API — a long way from the expired code that caused it.
 */
export function parseTokenResponse(status: number, body: unknown): TokenResponse {
  if (typeof body !== 'object' || body === null) {
    throw new OAuthError(`token endpoint returned a non-object (status ${status})`);
  }
  const tokens = body as TokenResponse;
  if (tokens.error) {
    throw new OAuthError(
      `token exchange rejected: ${tokens.error}${
        tokens.error_description ? ` (${tokens.error_description})` : ''
      }`,
    );
  }
  return tokens;
}

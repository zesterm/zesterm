/**
 * The D1 surface this Worker uses, declared rather than imported.
 *
 * `@cloudflare/workers-types` has `D1Database`, but naming a structural subset
 * here buys the thing that matters: the store's tests run against a plain
 * object under `node --test`, with no workerd and no miniflare. Security code
 * that can only be exercised by deploying is security code that is exercised
 * rarely.
 *
 * It is deliberately the *narrow* subset. If a future query needs `batch` or
 * `exec`, adding it here is a visible decision rather than a discovery.
 */

export interface D1Result<T> {
  readonly results: T[];
}

export interface D1PreparedStatement {
  bind(...values: unknown[]): D1PreparedStatement;
  first<T>(): Promise<T | null>;
  run(): Promise<unknown>;
  all<T>(): Promise<D1Result<T>>;
}

export interface Db {
  prepare(query: string): D1PreparedStatement;
}

// --- rows ------------------------------------------------------------------

export interface UserRow {
  readonly id: string;
  readonly email: string | null;
  readonly display_name: string;
  readonly avatar_url: string | null;
  readonly created_at: number;
  readonly updated_at: number;
  readonly disabled_at: number | null;
}

export interface SessionRow {
  readonly id: string;
  readonly user_id: string;
  readonly created_at: number;
  readonly last_seen_at: number;
  readonly expires_at: number;
  readonly revoked_at: number | null;
}

/** What the app is told about the signed-in person. Never the row verbatim. */
export interface PublicUser {
  readonly id: string;
  readonly displayName: string;
  readonly email: string | null;
  readonly avatarUrl: string | null;
}

export function publicUser(row: UserRow): PublicUser {
  // Field by field rather than a spread: `users` will grow columns, and a
  // spread would ship each new one to the browser by default. The safe
  // direction for that mistake is "a field is missing", not "a field leaked".
  return {
    id: row.id,
    displayName: row.display_name,
    email: row.email,
    avatarUrl: row.avatar_url,
  };
}

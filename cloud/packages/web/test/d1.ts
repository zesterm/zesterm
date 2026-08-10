/**
 * A `Db` backed by real SQLite, running the real migration.
 *
 * Node 24 ships `node:sqlite`, and D1 *is* SQLite — so the alternative (a
 * hand-written fake that answers the queries it was told about) would only
 * ever prove the store calls the methods it calls. This proves the SQL: the
 * foreign keys, the primary keys, the `email_verified = 1` filter that the
 * account-linking rule turns on, and the migration file itself.
 *
 * Not a substitute for deploying — D1's dialect, limits and consistency are
 * its own — but every failure this catches is one that would otherwise have
 * been caught by a person signing in.
 */

import { DatabaseSync } from 'node:sqlite';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

import type { D1PreparedStatement, Db, D1Result } from '../src/db/types.ts';

// `.href`, not the URL object: node's own `URL` and the global one are
// structurally different under these lib settings, so passing the object is
// rejected by both `readFileSync` and `fileURLToPath`. The string form is
// what they agree on.
const MIGRATION = fileURLToPath(new URL('../../../migrations/0001_init.sql', import.meta.url).href);

class Statement implements D1PreparedStatement {
  #db: DatabaseSync;
  #sql: string;
  #args: unknown[] = [];

  constructor(db: DatabaseSync, sql: string) {
    this.#db = db;
    this.#sql = sql;
  }

  bind(...values: unknown[]): D1PreparedStatement {
    // D1 returns a new statement; matching that keeps a caller from depending
    // on mutation this fake would allow and the real thing would not.
    const next = new Statement(this.#db, this.#sql);
    next.#args = values;
    return next;
  }

  async first<T>(): Promise<T | null> {
    const row = this.#db.prepare(this.#sql).get(...(this.#args as never[]));
    return (row as T | undefined) ?? null;
  }

  async run(): Promise<unknown> {
    return this.#db.prepare(this.#sql).run(...(this.#args as never[]));
  }

  async all<T>(): Promise<D1Result<T>> {
    return { results: this.#db.prepare(this.#sql).all(...(this.#args as never[])) as T[] };
  }
}

export interface TestDb extends Db {
  /** Straight to SQLite, for arranging state a test needs but does not assert. */
  raw: DatabaseSync;
  close(): void;
}

export function testDb(): TestDb {
  const sqlite = new DatabaseSync(':memory:');
  // The migration turns this on too, but `node:sqlite` scopes the pragma to
  // the connection -- so without it here the ON DELETE CASCADE and the
  // REFERENCES checks would silently do nothing and the tests would pass while
  // proving less than they claim.
  sqlite.exec('PRAGMA foreign_keys = ON');
  sqlite.exec(readFileSync(MIGRATION, 'utf8'));

  return {
    prepare: (sql: string) => new Statement(sqlite, sql),
    raw: sqlite,
    close: () => sqlite.close(),
  };
}

/** A user to hang sessions off, without going through the OAuth path. */
export function seedUser(db: TestDb, id: string, now = 1_000): void {
  db.raw
    .prepare(
      `INSERT INTO users (id, email, display_name, avatar_url, created_at, updated_at)
       VALUES (?, ?, ?, ?, ?, ?)`,
    )
    .run(id, `${id}@example.com`, id, null, now, now);
}

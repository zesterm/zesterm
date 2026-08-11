/**
 * Claims `wrangler.jsonc` makes that nothing else checks.
 *
 * `wrangler deploy --dry-run` is the gate for this file, and it is narrower
 * than it looks: it bundles the Worker and resolves *bindings* and the
 * *entrypoint*, because that is what a deploy needs. Anything the config says
 * about other commands — `migrations_dir` being the first example — it never
 * reads, and anything it says by *omission* it cannot read at all.
 *
 * Omission is most of this file. A `compatibility_flags` that grew an entry, an
 * `assets` block copied over from the web Worker, a `name` that drifted onto
 * the web Worker's — none of those is an error to wrangler, and the last of
 * them drops every terminal in the fleet.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync, existsSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const CONFIG_PATH = fileURLToPath(new URL('../wrangler.jsonc', import.meta.url).href);
const WEB_CONFIG_PATH = fileURLToPath(
  new URL('../../web/wrangler.jsonc', import.meta.url).href,
);

/**
 * jsonc, minus the comments.
 *
 * Hand-stripped rather than pulling in a parser, as in packages/web: this
 * package has no runtime dependencies and one regex is a smaller thing to own
 * than a dependency. It only has to cope with the comments *these* files use,
 * and a mistake shows up immediately as a JSON parse error in the tests.
 */
function readJsonc(path: string): Record<string, unknown> {
  const text = readFileSync(path, 'utf8')
    .replace(/^\s*\/\/.*$/gm, '')
    .replace(/\/\*[\s\S]*?\*\//g, '');
  return JSON.parse(text) as Record<string, unknown>;
}

const config = readJsonc(CONFIG_PATH);
const webConfig = readJsonc(WEB_CONFIG_PATH);

test('the relay is a different Worker from the web app', () => {
  // The one that costs the most to get wrong, and costs nothing to check.
  //
  // Deploying a Worker that owns a Durable Object class evicts every live
  // instance of that class. Two Workers with one name is one Worker, so the web
  // app's deploy would take the relay's objects with it -- dropping every
  // terminal in the fleet every time anyone changed a stylesheet. Nothing in
  // wrangler objects to it; the symptom is sessions dying at deploy time, in
  // production, for other people. -> ADR-009.
  assert.notEqual(
    config['name'],
    webConfig['name'],
    'sharing a name with the web Worker means a CSS change drops every terminal in the fleet',
  );

  // The other half: the split is only real while this Worker owns the class.
  // If a durable_objects binding here ever grows a `script_name`, the object
  // lives in *that* script and the deploy cadences are joined again.
  const bindings = durableObjectBindings();
  for (const b of bindings) {
    assert.equal(
      b['script_name'],
      undefined,
      'a script_name points the binding at another Worker, which puts the eviction back on its deploys',
    );
  }
});

test('the two Workers agree on the runtime', () => {
  // They share a database and will share wire types. A compatibility date that
  // differs between them is a difference nobody would think to look for.
  assert.equal(config['compatibility_date'], webConfig['compatibility_date']);
});

test('there are no compatibility flags', () => {
  // ADR-005: session actors run on the user's machine and never at the edge.
  // `nodejs_als` is what `@sigx/actors/host` needs to run, so an ALS flag here
  // is that decision undone by a config line -- which ADR-009 names as a
  // rejected option in so many words, because "the relay needs a coordination
  // point" is exactly the argument that reaches for it.
  assert.equal(
    config['compatibility_flags'],
    undefined,
    'no flags is a decision, not an omission — read ADR-005 before adding one',
  );
});

test('the relay serves no assets', () => {
  // The built web client is the other Worker's job. An `assets` block here is
  // the first step of one Worker doing both, which is the arrangement the
  // separate `name` above exists to prevent.
  assert.equal(config['assets'], undefined);
});

test('the migration creates the class the entrypoint exports, with SQLite storage', async () => {
  const migrations = (config['migrations'] ?? []) as Array<Record<string, unknown>>;
  assert.ok(migrations.length > 0, 'a durable object class with no migration cannot deploy');

  // `new_sqlite_classes`, not `new_classes`. The key-value backend is the
  // legacy one and cannot be chosen for a new namespace, so a class declared
  // with `new_classes` deploys and then fails on its first storage call -- at
  // the edge, on the first real attach.
  const created = new Set<string>();
  for (const m of migrations) {
    assert.equal(
      m['new_classes'],
      undefined,
      'new_classes is the key-value backend; new namespaces must use new_sqlite_classes',
    );
    for (const name of (m['new_sqlite_classes'] ?? []) as string[]) created.add(name);
  }

  // Every binding's class must actually be created by a migration, and the
  // class must actually be exported from `main`. Wrangler checks the first at
  // deploy time and the second not at all -- a renamed export is a Worker that
  // deploys and 500s.
  const entry = (await import('../src/index.ts')) as Record<string, unknown>;
  for (const b of durableObjectBindings()) {
    const className = b['class_name'] as string;
    assert.ok(created.has(className), `no migration creates ${className}`);
    assert.equal(
      typeof entry[className],
      'function',
      `${className} is bound and migrated but not exported from src/index.ts`,
    );
  }
});

test('migrations_dir points at a directory that exists and has migrations in it', () => {
  // Read from the **database binding**, which is where wrangler reads it. At
  // the top level it is only an "Unexpected fields found" *warning*, so the run
  // continues and then fails with "No migrations present" pointing at the
  // default path instead. packages/web paid for this one; the assertion is
  // repeated rather than shared because the mistake is per-config.
  assert.equal(
    config['migrations_dir'],
    undefined,
    'migrations_dir at the top level is ignored by wrangler — it belongs on the d1_databases entry',
  );

  const dbs = (config['d1_databases'] ?? []) as Array<Record<string, unknown>>;
  const db = dbs.find((d) => d['binding'] === 'DB');
  assert.ok(db, 'no d1_databases entry bound as DB');

  // The relay binds the *same* database as the web Worker, which is why the
  // migrations live at cloud/migrations rather than under either package. A
  // second database id here would mean two schemas drifting apart.
  const webDb = ((webConfig['d1_databases'] ?? []) as Array<Record<string, unknown>>).find(
    (d) => d['binding'] === 'DB',
  );
  assert.equal(db['database_id'], webDb?.['database_id']);

  const rel = db['migrations_dir'];
  assert.equal(typeof rel, 'string', 'migrations_dir must be set — the default is wrong here');
  const dir = resolve(dirname(CONFIG_PATH), rel as string);
  assert.ok(existsSync(dir), `migrations_dir resolves to ${dir}, which does not exist`);
  assert.ok(
    readdirSync(dir).some((f) => f.endsWith('.sql')),
    `no .sql files in ${dir}`,
  );
});

function durableObjectBindings(): Array<Record<string, unknown>> {
  const dos = config['durable_objects'] as { bindings?: Array<Record<string, unknown>> };
  const bindings = dos?.bindings ?? [];
  assert.ok(bindings.length > 0, 'the relay is its Durable Object; a config without one is inert');
  return bindings;
}

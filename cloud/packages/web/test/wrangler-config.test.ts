/**
 * Claims `wrangler.jsonc` makes that nothing else checks.
 *
 * `wrangler deploy --dry-run` is the gate for this file, and it is narrower
 * than it looks: it bundles the Worker and resolves *bindings* and the
 * *entrypoint*, because that is what a deploy needs. Anything the config says
 * about commands other than `deploy` — `migrations_dir` being the first
 * example — it never reads.
 *
 * So `migrations_dir` pointed at a directory that did not exist, and the way
 * that surfaced was `wrangler d1 migrations apply` failing during the first
 * real deployment. These assertions are cheap and turn that into a red test.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync, existsSync } from 'node:fs';
import { dirname, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const CONFIG_PATH = fileURLToPath(new URL('../wrangler.jsonc', import.meta.url).href);

/**
 * jsonc, minus the comments.
 *
 * Hand-stripped rather than pulling in a parser: this package has no runtime
 * dependencies and one regex is a smaller thing to own than a dependency. It
 * only has to cope with the comments *this* file uses, and a mistake here
 * shows up immediately as a JSON parse error in the tests.
 */
function readJsonc(path: string): Record<string, unknown> {
  const text = readFileSync(path, 'utf8')
    .replace(/^\s*\/\/.*$/gm, '')
    .replace(/\/\*[\s\S]*?\*\//g, '');
  return JSON.parse(text) as Record<string, unknown>;
}

const config = readJsonc(CONFIG_PATH);

test('migrations_dir points at a directory that exists and has migrations in it', () => {
  // Read from the **database binding**, which is where wrangler reads it.
  //
  // The first version of this test looked at the top level, because that is
  // where the first version of the config put it -- so it passed while
  // `wrangler d1 migrations apply` still failed. A guard that reads a value
  // from where the author assumed rather than where the tool looks is not a
  // guard. Wrangler's own signal is a *warning* ("Unexpected fields found in
  // top-level field"), so the run continues and fails later pointing at the
  // default path, which is why the mistake survives a casual read.
  assert.equal(
    config['migrations_dir'],
    undefined,
    'migrations_dir at the top level is ignored by wrangler — it belongs on the d1_databases entry',
  );

  const dbs = config['d1_databases'] as Array<Record<string, unknown>> | undefined;
  assert.ok(dbs && dbs.length > 0, 'no d1_databases binding');
  const rel = dbs[0]?.['migrations_dir'];
  assert.equal(typeof rel, 'string', 'migrations_dir must be set — the default is wrong here');

  // Resolved against the config file, which is the whole point: wrangler does
  // the same, and assuming it resolves against the repo root is the mistake
  // this test exists to catch.
  const dir = resolve(dirname(CONFIG_PATH), rel as string);
  assert.ok(existsSync(dir), `migrations_dir resolves to ${dir}, which does not exist`);

  const sql = readdirSync(dir).filter((f) => f.endsWith('.sql'));
  assert.ok(sql.length > 0, `no .sql files in ${dir}`);

  // Wrangler applies them in lexical order and records what it has run, so a
  // file that sorts before one already applied is silently skipped in
  // production while working perfectly on a fresh local database.
  for (const name of sql) {
    assert.match(name, /^\d{4}_/, `${name} must start with a four-digit sequence`);
  }
  assert.deepEqual([...sql].sort(), sql.slice().sort(), 'listing is stable');
});

test('the asset directory points at the app the Worker is meant to serve', () => {
  // The other path in this file that is resolved relative to the config, and
  // the other one that would only fail at deploy time.
  const assets = config['assets'] as { directory?: string } | undefined;
  assert.equal(typeof assets?.directory, 'string');
  const dir = resolve(dirname(CONFIG_PATH), assets!.directory!);
  assert.match(dir, /clients[/\\]web[/\\]packages[/\\]app[/\\]dist$/);
});

test('the origin and the worker name agree', () => {
  // workers.dev URLs are `<worker-name>.<account-subdomain>.workers.dev`, so a
  // renamed Worker moves the origin -- and every `__Host-` cookie is pinned to
  // the origin, which means the mismatch signs everybody out rather than
  // erroring anywhere a log would show it.
  const name = config['name'] as string;
  const origin = (config['vars'] as { APP_ORIGIN?: string }).APP_ORIGIN ?? '';
  assert.ok(origin.startsWith('https://'), 'the origin must be https for __Host- cookies');
  assert.ok(
    new URL(origin).hostname.startsWith(`${name}.`),
    `APP_ORIGIN ${origin} does not begin with the worker name "${name}"`,
  );
});

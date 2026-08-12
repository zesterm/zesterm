/**
 * The entrypoint may export handlers and Durable Object classes, and nothing
 * else.
 *
 * **This is not a style rule and the failure is total.** workerd reads every
 * named export of the module named by `main` as an entrypoint — a handler, a
 * Durable Object class, a `WorkerEntrypoint` — so one plain `export const`
 * beside them is a type error in that map and the Worker refuses to start:
 *
 * ```
 * Uncaught TypeError: Incorrect type for map entry 'ATTACH_PATH':
 *   the provided value is not of type 'function or ExportedHandler'.
 * ```
 *
 * Not one route failing. No Worker at all, on deploy.
 *
 * **`wrangler deploy --dry-run` does not catch it**, which is why this file
 * exists: `--dry-run` bundles and validates the config without ever starting a
 * runtime, so `cloud workers` was green across nineteen merged PRs while this
 * module could not boot. It was found by pointing `tools/fake-host.mjs` at a
 * real `wrangler dev` — the only thing in this repo that had ever started one.
 *
 * A constant a reader wants gathered stays in the module that declares it and
 * is imported from there. `CLOSE_TICKET_REFUSED` lives in `ticket.ts` for
 * exactly this reason.
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import * as entrypoint from '../src/index.ts';

/**
 * A function, or an object with a `fetch` — the two the runtime names.
 *
 * Both, not just the first: the error quoted above says "function or
 * ExportedHandler", and a guard stricter than the rule it enforces refuses
 * something that would deploy. A class is a function, so Durable Object exports
 * pass on the first arm.
 */
function mountable(value: unknown): boolean {
  if (typeof value === 'function') return true;
  return typeof (value as { fetch?: unknown } | null)?.fetch === 'function';
}

test('every named export of the entrypoint is something workerd can mount', () => {
  for (const [name, value] of Object.entries(entrypoint)) {
    if (name === 'default') continue;
    assert.ok(
      mountable(value),
      `\`${name}\` is a ${typeof value}, and workerd mounts every named export here as an ` +
        `entrypoint — anything that is not a function or an ExportedHandler is "not of type ` +
        `'function or ExportedHandler'" and the whole Worker refuses to start. Import it from ` +
        `the module that declares it instead.`,
    );
  }
});

test('the default export is a handler with a fetch', () => {
  const handler = (entrypoint as { default?: { fetch?: unknown } }).default;
  assert.equal(
    typeof handler?.fetch,
    'function',
    'the default export is what serves every request; a module that lost it deploys and 500s',
  );
});

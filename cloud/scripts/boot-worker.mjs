#!/usr/bin/env node
/**
 * Start this package's Worker under a real workerd, ask it one question, and
 * fail if the runtime refuses to come up.
 *
 * This exists because `wrangler deploy --dry-run` — the `dry-run` script beside
 * this one — **bundles and validates the config without ever starting a
 * runtime**. So `cloud workers` was green across nineteen merged PRs while
 * `packages/relay/src/index.ts` could not boot at all: workerd reads every
 * named export of the entrypoint module as an entrypoint, and one plain
 * `export const` beside the handler is a type error in that map:
 *
 * ```
 * Uncaught TypeError: Incorrect type for map entry 'ATTACH_PATH':
 *   the provided value is not of type 'function or ExportedHandler'.
 * ```
 *
 * Not one route failing — no Worker at all, on deploy. It was found by hand, by
 * pointing `packages/relay/tools/fake-host.mjs` at a real `wrangler dev`. → #113.
 *
 * # What this checks, and what it does not
 *
 * That the Worker **boots** and that **its own code answers**. The probe asserts
 * a status and a substring of the body, so an answer the Worker's module did not
 * produce fails it too. It is deliberately not a functional test: what a route
 * decides is settled by `node --test` against a pure `Request`, which is faster
 * and needs no runtime. Nothing here verifies a ticket, a signature or a session.
 *
 * Each package declares its own probe in its `boot` script. `--body-contains`
 * takes a single unquoted word on purpose: a package script is handed to
 * whatever shell pnpm found, and the one on Windows does not understand `'…'`.
 *
 * # What it costs, because that is what decides whether it survives
 *
 * Measured on an M-series Mac: ~6.5s for one Worker, and 5.7-8.0s for
 * `pnpm -r boot`, which is both of them — they run concurrently, so the second
 * is very nearly free. **~4.6s of each is importing wrangler's CLI bundle**;
 * workerd itself starts in ~250ms and answers in ~120ms. So the runtime is
 * nearly free and the module load is the whole bill. Booting both from one node
 * process would save one copy of it, at the cost of a script that has to know
 * about every package rather than each package declaring its own probe — worth
 * revisiting only if a third Worker appears.
 *
 * # Why `unstable_startWorker`, and what was rejected
 *
 * Surveyed against wrangler 4.120.0, which is what this project installs.
 *
 * - **`getPlatformProxy()`** hands Node the *bindings* without ever mounting the
 *   entrypoint. Run against the broken module above it resolves happily and
 *   reports `env` — it cannot catch this class of bug at all. (It prints a
 *   warning that no `RelayRoom` class is exported, which is *not* a check that
 *   found anything: it prints exactly the same warning against the healthy
 *   module, because it never loads the module to look.)
 * - **`unstable_dev()`** is the older wrapper over the same machinery and does
 *   start workerd — but against the broken module its `fetch` never settles,
 *   and its options carry no `structuredLogsHandler`, so the only failure it
 *   can produce is a timeout that reports "timed out" instead of the runtime's
 *   own message. Under `unstable_startWorker` that message arrives about half a
 *   second after the runtime is asked to start, and names the export.
 * - **`vitest-pool-workers`** would run the Worker under workerd too, at the
 *   cost of putting vitest into a project whose every other suite is
 *   `node --test`. Two test runners for one assertion is the wrong trade.
 * - **`wrangler dev` as a subprocess** is how the bug was found and it works,
 *   but the port has to be scraped out of its stdout and the process reaped on
 *   every exit path. The API returns the URL and disposes.
 *
 * # The failure mode is a hang, which is why there are two detectors
 *
 * A Worker that workerd refuses to start does not make `fetch` reject: the
 * request is queued against a runtime that never arrives, and `worker.ready`
 * resolves anyway. So the error-level runtime log is the primary detector — it
 * arrives in well under a second and carries the message a person needs — and
 * the timeout is the backstop for a failure that produces no such log.
 */

import { writeSync } from 'node:fs';
import { createRequire } from 'node:module';
import { pathToFileURL } from 'node:url';

const usage = `boot-worker.mjs --path <path> --status <n> [--body-contains <text>]
                        [--var NAME=value]... [--timeout <ms>]

Run from a Worker package directory; the wrangler config is read from there.`;

/** @returns {never} */
function die(message) {
  // `writeSync`, not `console.error`: every failure path here ends in
  // `process.exit` — workerd is a live child process and node would otherwise
  // never exit — and a write to a *pipe*, which is what CI gives us, is
  // asynchronous and would be dropped by it.
  writeSync(2, `\nboot-worker: ${message}\n`);
  process.exit(1);
}

function parseArgs(argv) {
  const options = { path: null, status: null, bodyContains: null, vars: {}, timeoutMs: 60_000 };
  for (let i = 0; i < argv.length; i++) {
    const flag = argv[i];
    const value = argv[i + 1];
    if (value === undefined) die(`${flag} needs a value\n\n${usage}`);
    i++;
    switch (flag) {
      case '--path':
        options.path = value;
        break;
      case '--status':
        options.status = Number(value);
        break;
      case '--body-contains':
        options.bodyContains = value;
        break;
      case '--timeout':
        options.timeoutMs = Number(value);
        break;
      case '--var': {
        const eq = value.indexOf('=');
        if (eq < 1) die(`--var wants NAME=value, got ${JSON.stringify(value)}`);
        options.vars[value.slice(0, eq)] = value.slice(eq + 1);
        break;
      }
      default:
        die(`unknown flag ${flag}\n\n${usage}`);
    }
  }
  if (options.path === null || options.status === null) die(`--path and --status are required\n\n${usage}`);
  return options;
}

const options = parseArgs(process.argv.slice(2));

// Resolved from the *package* rather than from this file: `wrangler` is a
// devDependency of each Worker package and this script lives in `cloud/scripts`,
// where pnpm's non-hoisted layout gives it no `node_modules` of its own.
const require = createRequire(pathToFileURL(`${process.cwd()}/package.json`));
let unstable_startWorker;
try {
  ({ unstable_startWorker } = await import(pathToFileURL(require.resolve('wrangler')).href));
} catch (error) {
  die(`could not load wrangler from ${process.cwd()} — run this from a Worker package (${error})`);
}

/** The first error-level log workerd produced, if any. */
let reportRuntimeError;
const runtimeError = new Promise((resolve) => {
  reportRuntimeError = resolve;
});

const bindings = Object.fromEntries(
  Object.entries(options.vars).map(([name, value]) => [name, { type: 'plain_text', value }]),
);

let worker;
const outcome = await Promise.race([
  (async () => {
    // `port: 0` so two packages can boot at once under `pnpm -r` without
    // agreeing on ports; `inspector: false` because nothing here attaches a
    // debugger; `persist: false` so the check reads no `.wrangler/state` left
    // behind by an earlier `wrangler dev` — a gate whose answer depends on a
    // local D1 somebody once seeded is not a gate.
    worker = await unstable_startWorker({
      config: 'wrangler.jsonc',
      // Overrides anything of the same name in an uncommitted `.dev.vars`,
      // which is the point: these are throwaway values and the check must
      // behave the same on a developer's machine as on a bare CI runner.
      // Spread rather than passed unconditionally: a Worker with no throwaway
      // values to supply must not hand wrangler an empty binding set.
      ...(Object.keys(bindings).length > 0 ? { bindings } : {}),
      dev: {
        server: { port: 0 },
        inspector: false,
        persist: false,
        structuredLogsHandler(log) {
          if (log.level === 'error') reportRuntimeError(log.message);
        },
      },
    });
    const response = await worker.fetch(new URL(options.path, await worker.url));
    return { kind: 'response', response };
  })(),
  runtimeError.then((message) => ({ kind: 'runtime-error', message })),
  new Promise((resolve) => {
    setTimeout(() => resolve({ kind: 'timeout' }), options.timeoutMs);
  }),
]).catch((error) => ({ kind: 'threw', error }));

/**
 * Take workerd down before exiting, and give up if it will not go.
 *
 * `process.exit` on its own orphans the runtime — it is a child process, not a
 * handle node will drain — and the failure paths all end in one, since a
 * half-started dev server is precisely what will not let node exit by itself.
 * The bound is there because a runtime that never came up is exactly the case
 * where `dispose` might not return, and a check that hangs is worse than a
 * stray process a CI runner is about to delete anyway.
 */
async function stopWorker() {
  if (worker === undefined) return;
  await Promise.race([
    worker.dispose().catch(() => {}),
    new Promise((resolve) => {
      setTimeout(resolve, 2_000);
    }),
  ]);
}

if (outcome.kind === 'runtime-error') {
  await stopWorker();
  die(
    `the Worker did not start:\n\n  ${outcome.message}\n\n` +
      `workerd mounts every named export of the entrypoint as an entrypoint, so a plain ` +
      `\`export const\` beside the handler is a type error in that map and nothing serves at ` +
      `all. \`wrangler deploy --dry-run\` cannot see this — it never starts a runtime.`,
  );
}
if (outcome.kind === 'timeout') {
  await stopWorker();
  die(
    `no response in ${options.timeoutMs}ms. The Worker never came up, and it did so without ` +
      `logging an error — look above for whatever workerd did say.`,
  );
}
if (outcome.kind === 'threw') {
  await stopWorker();
  // The `cause` is not decoration. `startWorker` rejects with a flat "An error
  // occurred when starting the server" and puts the sentence that names the
  // actual problem — a missing `clients/web/packages/app/dist`, a malformed
  // field — underneath it, so printing only the outer message turns a
  // one-line fix into a hunt.
  const cause = outcome.error?.cause;
  die(`starting the Worker failed: ${outcome.error}` + (cause === undefined ? '' : `\n\n  ${cause}`));
}

const body = await outcome.response.text();
const problems = [];
if (outcome.response.status !== options.status) {
  problems.push(`expected status ${options.status}, got ${outcome.response.status}`);
}
if (options.bodyContains !== null && !body.includes(options.bodyContains)) {
  problems.push(`expected the body to contain ${JSON.stringify(options.bodyContains)}`);
}
if (problems.length > 0) {
  await stopWorker();
  die(
    `${problems.join('; ')}.\nGET ${options.path} answered:\n\n${body.slice(0, 500)}\n\n` +
      `The runtime came up, so this is the Worker's own answer — either the route moved or ` +
      `something upstream of it (an asset binding, a redirect) took the request first.`,
  );
}

writeSync(1, `boot-worker: GET ${options.path} -> ${outcome.response.status}, under a real workerd.\n`);
// Stopped and then exited rather than returned from: even after `dispose`,
// letting node drain what is left adds about three seconds per Worker to a
// check whose whole argument is being cheap enough to run on every PR.
await stopWorker();
process.exit(0);

/**
 * Session context on the listing (#416): what the daemon says a session is
 * standing in — branch, kube context, pins — and the `busy` bit beside it.
 *
 * The property under test is the `HostOffer` one: both additions are
 * `#[serde(default)]` on the host, so a listing from an older daemon omits
 * them and the parser must produce a decoded value rather than throw — and,
 * the other way, a context that *is* sent must arrive with every field a chip
 * renders, `source` labels included, because a trust label that gets lost in
 * decode is a trust boundary that exists only in documentation (ADR-015).
 */

import { test } from 'node:test';
import assert from 'node:assert/strict';

import { parseSessionInfo } from '../src/wire.ts';

const base = {
  addr: { host: '2e'.repeat(32), session: 1 },
  title: 'zsh',
  cwd: '/home/andy/dev',
  cols: 120,
  rows: 30,
  alt_screen: false,
  attached: true,
};

test('a listing from a daemon that predates context parses to "did not say"', () => {
  const info = parseSessionInfo(base);
  assert.equal(info.context, null, 'absent context is "the daemon did not say", not a throw');
  assert.equal(info.busy, false, 'absent busy is not-busy, the only safe default');
});

test('a full context arrives with its trust labels intact', () => {
  const info = parseSessionInfo({
    ...base,
    busy: true,
    context: {
      git: { branch: 'feature/x', detached: false },
      facts: [
        { key: 'kube', value: 'prod-eu', source: 'daemon_probe' },
        { key: 'venv', value: 'ml', source: 'shell_report' },
      ],
      revision: 7,
    },
  });
  assert.equal(info.busy, true);
  const ctx = info.context;
  assert.ok(ctx, 'a sent context must decode');
  assert.equal(ctx.git?.branch, 'feature/x');
  assert.equal(ctx.git?.detached, false);
  assert.equal(ctx.git?.dirty, null, 'dirty unsent is unknown, never clean');
  assert.equal(ctx.revision, 7);
  assert.deepEqual(
    ctx.facts.map((f) => [f.key, f.value, f.source]),
    [
      ['kube', 'prod-eu', 'daemon_probe'],
      ['venv', 'ml', 'shell_report'],
    ],
    'each fact keeps who said it — the label is the payload, not the docs',
  );
});

test('an unknown source degrades to shell_report, never to a dead listing', () => {
  // A newer daemon minting a third provenance must not cost an older client
  // its session list — and the degrade direction matters: unknown provenance
  // is read with the *least* trust, not the most.
  const info = parseSessionInfo({
    ...base,
    context: {
      git: null,
      facts: [{ key: 'aws_profile', value: 'staging', source: 'attested_probe' }],
      revision: 1,
    },
  });
  assert.equal(info.context?.facts[0]?.source, 'shell_report');
});

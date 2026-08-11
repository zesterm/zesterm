import { test } from 'node:test';
import assert from 'node:assert/strict';

import type { DirectoryView } from '@zesterm/control';

import { directoryStatusOf, type MatchableAsync } from '../src/directory-source.ts';

const VIEW: DirectoryView = {
  connected: true,
  host: { id: 'ab'.repeat(32), label: 'mac' },
  sessions: [],
  dataPlane: { kind: 'ws', host: '127.0.0.1', port: 7718 },
  lastCreated: null,
};

/**
 * A state that fires exactly one arm.
 *
 * The mapping is the only part of the seam a component-less test can reach —
 * `actorDirectorySource` needs an instance and a mounted actors plugin, and
 * this package has no DOM to mount into. What this does pin is that each arm
 * still reaches the status the list renders, which is what a hand-edit of the
 * dispatch above would silently break.
 */
function fires(arm: 'pending' | 'ready', error?: Error): MatchableAsync<DirectoryView> {
  return {
    match: (arms) => {
      if (error !== undefined) return arms.error?.(error);
      return arm === 'pending' ? arms.pending?.() : arms.ready(VIEW);
    },
  };
}

test('a pending read is a pending status, and carries no view to render', () => {
  const status = directoryStatusOf(fires('pending'));
  assert.equal(
    status.kind,
    'pending',
    'the list shows “reaching the sidecar…” off this arm; anything else claims a link that is not up',
  );
});

test('a failed read carries the error message, not a generic one', () => {
  const status = directoryStatusOf(fires('pending', new Error('socket closed')));
  assert.equal(status.kind, 'error', 'a failed control-plane read must not read as merely slow');
  assert.equal(
    status.kind === 'error' ? status.message : null,
    'socket closed',
    'the header shows this string — a swallowed message leaves a degraded banner saying nothing',
  );
});

test('a settled read hands the view through unchanged', () => {
  const status = directoryStatusOf(fires('ready'));
  assert.equal(status.kind, 'ready', 'the sessions and the create button hang off this arm');
  assert.equal(
    status.kind === 'ready' ? status.view : null,
    VIEW,
    'the same object, not a copy: the list reads dataPlane and connected off it',
  );
});

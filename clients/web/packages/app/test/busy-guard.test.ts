import assert from 'node:assert/strict';
import test from 'node:test';

import { runExclusive, type BusyState } from '../src/busy-guard.ts';

/** A promise plus the handles to settle it, so a test can hold work open. */
function deferred(): {
  promise: Promise<void>;
  resolve: () => void;
  reject: (e: Error) => void;
} {
  let resolve!: () => void;
  let reject!: (e: Error) => void;
  const promise = new Promise<void>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

test('a second run is refused while the first is still in flight', async () => {
  // The regression this file exists for. A flag cleared in a `finally` on the
  // synchronous call passes every test that only checks one click, and lets a
  // double-click start two shells on the user's machine.
  const state: BusyState = { busy: false };
  const first = deferred();
  let runs = 0;

  runExclusive(state, () => {
    runs += 1;
    return first.promise;
  });
  runExclusive(state, () => {
    runs += 1;
    return first.promise;
  });

  assert.equal(runs, 1, 'the second click ran while the first create was still in flight');

  first.resolve();
  await first.promise;
  runExclusive(state, () => {
    runs += 1;
  });
  assert.equal(runs, 2, 'the guard never released after the work finished');
});

test('a failure releases the guard, so the user can retry at once', async () => {
  const state: BusyState = { busy: false };
  const first = deferred();
  let runs = 0;

  runExclusive(state, () => {
    runs += 1;
    return first.promise;
  });
  first.reject(new Error('the daemon refused'));
  await first.promise.catch(() => {});
  // One turn for the `finally` to run.
  await Promise.resolve();

  runExclusive(state, () => {
    runs += 1;
  });
  assert.equal(runs, 2, 'a lock held by a failure is indistinguishable from a hung UI');
});

test('work that returns nothing releases immediately', () => {
  // Nothing to wait for; holding the flag would leave the control dead for
  // ever rather than merely for the length of the work.
  const state: BusyState = { busy: false };
  let runs = 0;
  runExclusive(state, () => {
    runs += 1;
  });
  assert.equal(state.busy, false, 'a synchronous run left the guard held');
  runExclusive(state, () => {
    runs += 1;
  });
  assert.equal(runs, 2);
});

/**
 * "Run this, and refuse to run it again until it has finished."
 *
 * A file of its own for the reason every other model in this package is one:
 * components here have no test harness, so behaviour that can be wrong lives
 * where `node --test` can reach it. This one can be wrong in a way that costs
 * the user something real — a duplicate shell on their machine — and it was
 * wrong in review, which is the argument for extracting it rather than the
 * argument against.
 *
 * **The bug this shape exists to prevent.** The obvious spelling is a flag set
 * before the call and cleared in a `finally`:
 *
 * ```ts
 * busy = true;
 * try { start(); } finally { busy = false; }   // guards nothing
 * ```
 *
 * Starting a session is asynchronous in both worlds — a round trip to the
 * daemon on loopback, a ticket and a pipe over the relay — so `start()` returns
 * long before the work does, and the flag is already back to `false` when the
 * second click arrives. It reads exactly like mutual exclusion and provides
 * none.
 */

/** Somewhere to keep the flag. A signal satisfies this structurally. */
export interface BusyState {
  busy: boolean;
}

/**
 * Run `work` unless it is already running.
 *
 * A `work` that returns nothing is taken at its word and releases at once:
 * there is nothing to wait for, and holding the flag would leave the button
 * dead for ever. Rejection releases too — `finally`, not `then` — because a
 * create that failed is one the user should be able to retry immediately, and
 * a lock held by a failure is indistinguishable from a hung UI.
 */
export function runExclusive(state: BusyState, work: () => void | Promise<unknown>): void {
  if (state.busy) return;
  state.busy = true;
  const settled = work();
  if (settled === undefined) {
    state.busy = false;
    return;
  }
  // `then(f, f)` rather than `finally(f)`: `finally` returns a *new* promise
  // that inherits the rejection, and nothing is holding it, so a create that
  // failed would release the guard correctly and then raise an
  // `unhandledrejection` on the way out — a console error per failed create,
  // originating in a file that is not reporting the error and does not own it.
  // The caller already has the failure; this only needs the release.
  const release = (): void => {
    state.busy = false;
  };
  void settled.then(release, release);
}

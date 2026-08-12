/**
 * Where the session list gets its sessions.
 *
 * One seam, because the two worlds keep the same list in structurally
 * different places. On loopback the sidecar hosts the `SessionDirectory`
 * actor and a live actor read is the whole mechanism. **The deployed client
 * has no sidecar**, so at the edge there is nowhere legitimate to host that
 * actor at all, and the list has to live in the tab — written by one
 * connection per host rather than read from a control plane.
 *
 * `SessionList` reads one shape either way and cannot tell which world it is
 * in. That is the point: a component that switches on `bootstrap.mode` is a
 * component that has to be re-tested in both, and the loopback path is the one
 * that must keep working when Cloudflare does not (ADR-005).
 *
 * **Two ways to give the hosted client an actor, both rejected — written down
 * so they are not rediscovered:**
 *
 * - **Run `createHost` in the browser.** It pulls turns, placement, storage,
 *   reminders and metrics into a terminal client's bundle in order to hold one
 *   array. A single-tab host is not a control plane, it is a variable.
 * - **`nodejs_als`/`nodejs_compat`, to run the actors host at the edge
 *   instead.** That is ADR-005 undone by a compatibility flag.
 *
 * **A source is created in a component's *setup* and read in its *render
 * fn*, and the split is load-bearing rather than stylistic.**
 * `useActorState` registers its read with the instance that calls it, so a
 * source created by a parent and handed down as an already-made reader would
 * move the subscription's lifetime to that parent — on loopback the live
 * directory read would then outlive the list that wanted it, staying
 * subscribed for as long as the shell is mounted. Passing a source as a
 * *function* is what keeps that lifetime where it already was.
 *
 * **Both halves exist now.** The hosted one is `live-directory.ts` — one
 * `ConnectionClient({ watchSessions: true })` per machine in the account,
 * dialled through `dialFor`, writing into a store keyed by `HostId`.
 *
 * It is *not* the shape this file predicted, and the difference is worth
 * recording because the prediction reads plausibly. The note here said the
 * hosted half "widens the `ready` arm from one view to a keyed set". It does
 * not: a source is **per host**, the keying lives inside the store that hands
 * them out, and `DirectoryStatus` still describes exactly one machine. Hoisting
 * the key into the status would have moved every per-host state — connecting,
 * pairing, asleep, refused — inside the `ready` arm, so "the list is ready" and
 * "this machine is ready" would have become the same word; and it would have
 * rewritten `SessionList`'s rendering for the benefit of the path that does not
 * need it. What did change is two new arms below, which the loopback path can
 * never produce.
 */

import { useActorState } from '@sigx/actors/app';
import { LOCAL_DIRECTORY_KEY, SessionDirectory, type DirectoryView } from '@zesterm/control';

/**
 * What the list can be told at any moment.
 *
 * The first three are sigx's five, collapsed: `idle` and `refreshing` fold
 * into `pending` and `ready` by the dispatch `directoryStatusOf` maps through,
 * and a source that is a live socket has no honest meaning for either.
 *
 * The last two only ever come from the hosted path, and both exist because
 * `error` would be a lie:
 *
 * - **`offline`** — the machine did not answer. Over the relay this is the
 *   *common* case, not a fault: most machines are asleep most of the time, and
 *   a fleet screen that paints half its rows red is one nobody reads. Nothing
 *   here claims to know *why* — the browser cannot: `ByteLinkHandlers` has no
 *   error channel, so a relay that closed `4404` (nobody home) and one that
 *   closed `4401` (the ticket was refused) arrive identically. See
 *   `relay-dial.ts`, which names that as a deliberate v1 cut.
 * - **`pairing`** — the daemon is waiting for someone to approve this browser
 *   at the machine, and the code has to be shown or the wait is unexplained.
 *   Enrolling a browser with the *account* does not make its device key
 *   trusted by a *daemon*; the first attach to each machine stops here.
 */
export type DirectoryStatus =
  | { readonly kind: 'pending' }
  | { readonly kind: 'pairing'; readonly code: string }
  | { readonly kind: 'offline' }
  | { readonly kind: 'error'; readonly message: string }
  | { readonly kind: 'ready'; readonly view: DirectoryView };

/** Called inside a render fn; reactive there like any signal read. */
export type DirectoryReader = () => DirectoryStatus;

/** Called during setup — see the module doc on why it is a function. */
export type DirectorySource = () => DirectoryReader;

/**
 * The part of sigx's `AsyncState` this file consumes.
 *
 * Structural rather than the real type, for two reasons that point the same
 * way: `AsyncState` lives in `@sigx/runtime-core`, which this package does not
 * depend on and should not start depending on for one import; and naming only
 * `match` is what lets the mapping below be tested without a component
 * instance, which nothing else in this package can be.
 */
export interface MatchableAsync<T> {
  match<R>(arms: {
    pending?: () => R;
    error?: (e: Error) => R;
    ready: (value: T) => R;
  }): R | undefined;
}

const PENDING: DirectoryStatus = { kind: 'pending' };

/**
 * An actor read, as a `DirectoryStatus`.
 *
 * Dispatched through `match` rather than by switching on the state name: sigx
 * folds `idle` into the `pending` arm and `refreshing` into the `ready` one,
 * and a switch here would be a second copy of that table, free to disagree
 * with the first after any release.
 */
export function directoryStatusOf(state: MatchableAsync<DirectoryView>): DirectoryStatus {
  return (
    state.match<DirectoryStatus>({
      pending: () => PENDING,
      error: (e) => ({ kind: 'error', message: e.message }),
      ready: (view) => ({ kind: 'ready', view }),
    }) ??
    // `match` is typed `R | undefined` for the caller who omits an arm; all
    // three are given, so this is unreachable. Pending is the arm that shows
    // nothing rather than the one that claims something.
    PENDING
  );
}

/**
 * The loopback path: the sidecar's actor, read live.
 *
 * `{ live: true }` is the entire mechanism — the read re-runs after every turn
 * that mutated the directory, whoever caused it, so a session created from the
 * desktop appears here without this page doing anything.
 */
// Typed as the seam rather than as its return value. Both compile — a function
// returning a `DirectoryReader` *is* a `DirectorySource` — but naming the seam
// here means a change to it fails at this declaration, where the contract is,
// instead of at whichever component happened to call it.
export const actorDirectorySource: DirectorySource = () => {
  const state = useActorState(SessionDirectory, LOCAL_DIRECTORY_KEY, 'list', { live: true });
  return () => directoryStatusOf(state);
};

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
 * **Only the actor-backed half exists today.** The hosted half — one
 * `ConnectionClient({ watchSessions: true })` per host, dialled through
 * `dialFor`, writing into a store keyed by `HostId` — is blocked on the relay
 * ticket endpoint (`relay-dial.ts` says why it is injected) and on there being
 * any notion of which hosts are online. It widens the `ready` arm below from
 * one view to a keyed set; nothing else here moves.
 */

import { useActorState } from '@sigx/actors/app';
import { LOCAL_DIRECTORY_KEY, SessionDirectory, type DirectoryView } from '@zesterm/control';

/**
 * What the list can be told at any moment.
 *
 * Three arms rather than sigx's five: `idle` and `refreshing` are collapsed by
 * the dispatch this maps through, and a source that is a live socket has no
 * honest meaning for either.
 */
export type DirectoryStatus =
  | { readonly kind: 'pending' }
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
export function actorDirectorySource(): DirectoryReader {
  const state = useActorState(SessionDirectory, LOCAL_DIRECTORY_KEY, 'list', { live: true });
  return () => directoryStatusOf(state);
}

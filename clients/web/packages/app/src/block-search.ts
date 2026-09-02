/**
 * The palette's fleet-wide block search, as the browser holds it (#530).
 *
 * The native app's shape, in a signal: one question in flight, the hosts it
 * reached, and every answer parked per host. The daemon's echoed `query` is
 * the only correlation there is — a reply for a query the palette has typed
 * past is dropped by comparing it, so a slow host answering `ca` after the
 * person typed `cargo` never puts the broader answer where the narrower one
 * belongs. Asked again on every keystroke, undebounced: a frame is a few
 * hundred bytes, the relay round trips are paid in parallel, and a stale
 * answer costs nothing but the compare.
 *
 * `hostsAsked` counts frames that actually went out — a machine still in its
 * handshake, or asleep, was not asked — and `hostsAnswered` counts echoes
 * that matched, refusals included: a host that said "history not searched"
 * did answer. The query row prints the two so a slow relay reads as pending
 * rather than as a fleet that is smaller than it is.
 *
 * The wire shape stops here. `BlockHit` is the projection every palette row
 * is built from, and `@zesterm/control` is not involved: routing a
 * per-keystroke answer through the session-directory actor would be a
 * second mechanism for the very message the hosted path receives directly.
 */

import type { BlockMatch, BlockMatchesMessage, BlockState } from '@zesterm/proto';
import { signal } from 'sigx';

/** Most rows one host returns per query; the palette caps again after the merge. */
export const BLOCK_SEARCH_LIMIT = 40;

/** One matched block, camel-cased and keyed the way the palette keys things. */
export interface BlockHit {
  readonly hostId: string;
  /** `null` for a block only the host's store remembers: nothing to open. */
  readonly session: string | null;
  readonly block: number;
  readonly command: string;
  /** The host cut a stored command: read it, never re-run it as if whole. */
  readonly commandTruncated: boolean;
  readonly cwd: string;
  readonly state: BlockState;
  readonly startedMs: number | null;
  readonly endedMs: number | null;
  readonly branch: string;
  readonly author: string | null;
}

export function blockHitOf(hostId: string, m: BlockMatch): BlockHit {
  return {
    hostId,
    session: m.session === null ? null : m.session.toString(),
    block: m.block,
    command: m.command,
    commandTruncated: m.command_truncated,
    cwd: m.cwd,
    state: m.state,
    startedMs: m.started_ms,
    endedMs: m.ended_ms,
    branch: m.context?.branch ?? '',
    author: m.author,
  };
}

/** The search as it stands, for the palette's render. */
export interface BlockSearchView {
  readonly query: string;
  /** Every host's answers, concatenated; reference-stable until an answer lands. */
  readonly hits: readonly BlockHit[];
  readonly hostsAsked: number;
  readonly hostsAnswered: number;
}

export interface BlockSearchStore {
  /**
   * Ask a new question. `fanout` sends it and returns how many hosts it
   * actually reached; every earlier answer is dropped, so a fast host's rows
   * for the old query never show under the new one.
   */
  ask(query: string, fanout: (query: string, limit: number) => number): void;
  /** Park one host's answer — unless its echo is for a question no longer asked. */
  answer(hostId: string, reply: BlockMatchesMessage): void;
  /** A reactive read: a render that calls it re-runs when an answer lands. */
  view(): BlockSearchView;
}

const NO_HITS: readonly BlockHit[] = Object.freeze([]);

export function blockSearchStore(): BlockSearchStore {
  // Top-level property assignment on a signal is the one reactivity every
  // component in this app already relies on; the record is replaced whole,
  // the `live-directory.ts` convention.
  const state = signal<{
    query: string;
    asked: number;
    answers: Readonly<Record<string, readonly BlockHit[]>>;
  }>({ query: '', asked: 0, answers: {} });

  // `hits` must be reference-stable between answers: the palette reads it on
  // every keystroke, and a watch that compares by reference would otherwise
  // see a list that is never equal to itself.
  let flattenedOf: Readonly<Record<string, readonly BlockHit[]>> | null = null;
  let flattened: readonly BlockHit[] = NO_HITS;

  return {
    ask(query, fanout) {
      state.query = query;
      state.answers = {};
      state.asked = fanout(query, BLOCK_SEARCH_LIMIT);
    },
    answer(hostId, reply) {
      if (reply.query !== state.query) return;
      state.answers = {
        ...state.answers,
        [hostId]: reply.matches.map((m) => blockHitOf(hostId, m)),
      };
    },
    view() {
      const answers = state.answers;
      if (answers !== flattenedOf) {
        flattenedOf = answers;
        const all = Object.values(answers).flat();
        flattened = all.length === 0 ? NO_HITS : all;
      }
      return {
        query: state.query,
        hits: flattened,
        hostsAsked: state.asked,
        hostsAnswered: Object.keys(answers).length,
      };
    },
  };
}

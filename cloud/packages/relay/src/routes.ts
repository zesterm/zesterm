/**
 * The three paths the relay answers, and the name a room is addressed by.
 *
 * Their own file because both ends need them: the Worker routes on them and
 * the Durable Object, which is handed the request verbatim, decides from them
 * which half of the pipe it is being asked for. Declaring them in `index.ts`
 * would make `room.ts` import the module that re-exports it, which is a cycle
 * for three string constants.
 *
 * Versioned, because a second shape would be.
 */

/** A browser, holding an attach ticket. */
export const ATTACH_PATH = '/v1/attach';

/** A daemon, parking its long-lived control link. */
export const CONTROL_PATH = '/v1/control';

/**
 * A daemon dialling back for one pipe it was just told to open.
 *
 * Separate from `CONTROL_PATH` because it is a separate connection and that is
 * the whole of ADR-009's dial-back: one socket per session, so `serve()` is
 * reused unchanged and a busy `cat` cannot stall every other session on the
 * host.
 */
export const PIPE_PATH = '/v1/pipe';

/**
 * The query parameter the Worker hands the room a verified ticket's `jti` in.
 *
 * A constant for the same reason `pipeTag`'s prefix is one: it is written in
 * `index.ts` and read back in `room.ts`, two halves of one fact. Spelt
 * differently in the two places, every attach would arrive with an empty id and
 * be refused — a total outage whose cause is a string literal.
 *
 * Unlike `?host=`, which is the caller's own parameter passed through, this one
 * is written by the Worker onto a request only it can address the object with.
 */
export const ATTACH_JTI_PARAM = 'jti';

/**
 * The `jti` off an attach the Worker has already rewritten, or `''`.
 *
 * The reading half of the pair, here rather than inline in `room.ts` so that the
 * writing half in `index.ts` has something to be tested *against*. `fetch` on
 * either side is the one function neither suite can drive — `WebSocketPair` is a
 * workerd global — so without this the two ends could name different parameters
 * and every test in the project would still pass, while every attach in the
 * fleet was refused for an id that never arrived.
 *
 * Empty rather than `null`, because an absent id fails closed at the claim and
 * this should not offer a second way to spell that.
 */
export function attachJti(url: URL): string {
  return url.searchParams.get(ATTACH_JTI_PARAM) ?? '';
}

/**
 * `idFromName` for one host's room. **One object per host** — not per user, a
 * five-host user would funnel every stream through one colo, and not per
 * session, which is the control link per session that dial-back avoids.
 * → ADR-009.
 *
 * The prefix is not decoration: `idFromName` names a flat space that this
 * Worker may one day address something else in, and an unprefixed 64-hex name
 * is exactly the kind of thing a later feature keys by accident.
 */
export function roomName(hostId: string): string {
  return `host:${hostId}`;
}

/**
 * The two paths the relay answers, and the name a room is addressed by.
 *
 * Their own file because both ends need them: the Worker routes on them and
 * the Durable Object, which is handed the request verbatim, decides from them
 * which half of the pipe it is being asked for. Declaring them in `index.ts`
 * would make `room.ts` import the module that re-exports it, which is a cycle
 * for two string constants.
 *
 * Versioned, because a second shape would be.
 */

/** A browser, holding an attach ticket. */
export const ATTACH_PATH = '/v1/attach';

/** A daemon, parking its long-lived control link. */
export const CONTROL_PATH = '/v1/control';

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

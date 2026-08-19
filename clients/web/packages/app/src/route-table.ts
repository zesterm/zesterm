/**
 * The shell's route family, as pure data the route table and its test share.
 *
 * RouterView keys the routed component by the matched record's path pattern —
 * and for the top-level view that key is `matched[0].path`. As sibling
 * records, `/hosts` and `/h/:hostId/s/:sessionId` carry different keys, so
 * crossing between them (the sidebar footer, closing the last tab) remounted
 * the Shell and discarded every open tab. As children of the `/hosts` record
 * they all share its key, and the Shell survives the crossing with its tabs.
 */
export const SHELL_PATH = '/hosts';

/**
 * The profiles screen (design §12, `⌘⇧,`).
 *
 * A child of the shell record like the session URLs, and for the same reason:
 * as a sibling it would carry a different RouterView key, so opening profiles
 * would remount the Shell and discard every open tab — the exact bug the note
 * above is about, one screen later.
 */
export const PROFILES_PATH = '/profiles';

/**
 * Absolute on purpose: the matcher takes a '/'-prefixed child path as-is
 * instead of joining it under the parent, which is what lets the `/h/…` URLs
 * live under the `/hosts` record without sharing its prefix.
 */
export const SHELL_CHILD_PATHS: readonly string[] = [
  '/h/:hostId',
  '/h/:hostId/s/:sessionId',
  PROFILES_PATH,
];

/**
 * Where to land after signing in — the client mirror of the Worker's
 * `safeNext`, and the same rules for the same reason: `next` rides through a
 * login URL, and one that is allowed to be absolute turns the login into an
 * open redirect wearing this origin's credibility.
 *
 * `//evil.example` is the case a naive `startsWith('/')` misses (a
 * protocol-relative URL), and `/\evil.example` is the same trick after some
 * browsers normalise the backslash. `/login` itself is refused too — a `next`
 * pointing back at the gate would bounce a signed-in visitor in a loop the
 * server never sees.
 *
 * Duplicated rather than imported because the Worker cannot share code with
 * this workspace; `routes.test.ts` pins the two rule sets against the same
 * cases the Worker's own tests use.
 */
export function safeNextPath(raw: string | undefined): string {
  if (raw === undefined || raw === '') return SHELL_PATH;
  if (!raw.startsWith('/')) return SHELL_PATH;
  if (raw.startsWith('//')) return SHELL_PATH;
  if (raw.startsWith('/\\')) return SHELL_PATH;
  if (raw === '/login' || raw.startsWith('/login?')) return SHELL_PATH;
  return raw;
}

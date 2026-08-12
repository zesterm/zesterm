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
 * Absolute on purpose: the matcher takes a '/'-prefixed child path as-is
 * instead of joining it under the parent, which is what lets the `/h/…` URLs
 * live under the `/hosts` record without sharing its prefix.
 */
export const SHELL_CHILD_PATHS: readonly string[] = ['/h/:hostId', '/h/:hostId/s/:sessionId'];

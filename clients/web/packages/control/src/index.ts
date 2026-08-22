/**
 * Control-plane actor definitions, imported by both sides: the sidecar hosts
 * them, the app calls them. One module so the two can never disagree about a
 * method's shape.
 */

export {
  SessionDirectory,
  sessionEntryOf,
  hostFactsOf,
  type SessionEntry,
  type EntryContext,
  type EntryGit,
  type EntryFact,
  type LaunchTarget,
  type HostFacts,
  type HostInfo,
  type DataPlane,
  type DirectoryState,
  type DirectoryView,
} from './session-directory.actor.ts';

/** The one directory in v1. Becomes the HostId when the fleet arrives. */
export const LOCAL_DIRECTORY_KEY = 'local';

/**
 * The pieces `main.ts` wires together, exported so tests — and any future
 * embedding, like the M4 daemon-spawned build — drive the same code.
 */

export { defaultSocketPath, socketDial } from './net-link.ts';
export { startFeed, entryOf, type FeedOptions } from './feed.ts';
export { startServer, type SidecarServerOptions } from './server.ts';

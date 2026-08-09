/**
 * The sidecar process.
 *
 * ```
 * node src/main.ts [--socket <daemon socket>] [--port 7350]
 *                  [--ws-host 127.0.0.1] [--ws-port 7718]
 *                  [--static <dir>] [--allow-origin <origin>]...
 * ```
 *
 * Standalone Node in v1. The roadmap's M4 shape — a Bun single file spawned
 * as a child of the daemon over stdio — is a packaging change over exactly
 * this code, recorded there rather than half-built here.
 */

import { createHost } from '@sigx/actors/host';
import { memoryStorage } from '@sigx/actors/host';

import { SessionDirectory } from '@zesterm/control';
import { startFeed } from './feed.ts';
import { defaultSocketPath, socketDial } from './net-link.ts';
import { startServer } from './server.ts';

async function main(): Promise<void> {
  const args = process.argv.slice(2);
  const opt = (name: string): string | undefined => {
    const i = args.indexOf(name);
    return i >= 0 ? args[i + 1] : undefined;
  };
  const all = (name: string): string[] =>
    args.flatMap((a, i) => (a === name && args[i + 1] !== undefined ? [args[i + 1] as string] : []));

  if (args.includes('--help')) {
    console.log(
      'zesterm sidecar -- the control plane, on your machine\n\n' +
        '  --socket <path>        the daemon loopback socket (default: per-user)\n' +
        '  --port <port>          serve the app + actors socket here (default 7350)\n' +
        '  --bind <addr>          interface to bind (default 127.0.0.1)\n' +
        '  --ws-host <host>       where browsers reach the data plane (default 127.0.0.1)\n' +
        '  --ws-port <port>       the daemon’s --listen-ws port (default 7718)\n' +
        '  --static <dir>         serve a built app from here\n' +
        '  --allow-origin <o>     extra allowed origin for the actors socket (repeatable;\n' +
        '                         the vite dev server, in practice)\n',
    );
    return;
  }

  const port = Number(opt('--port') ?? 7350);
  const bind = opt('--bind') ?? '127.0.0.1';
  const socketPath = opt('--socket') ?? defaultSocketPath();
  const dataPlane = {
    host: opt('--ws-host') ?? '127.0.0.1',
    port: Number(opt('--ws-port') ?? 7718),
  };

  // The directory is feed-primed on every start, so memory storage is the
  // honest choice: nothing here is worth surviving a restart, and a session
  // list read from disk is a list of ghosts.
  const host = createHost({ actors: [SessionDirectory], storage: memoryStorage() });
  await host.start();

  const extraOrigins = all('--allow-origin');
  const staticDir = opt('--static');
  const server = await startServer({
    host,
    port,
    bind,
    ...(staticDir === undefined ? {} : { staticDir }),
    // An explicit allowlist replaces same-origin wholesale, so the page's
    // own origins ride along with the exceptions.
    allowOrigins:
      extraOrigins.length === 0
        ? []
        : [...extraOrigins, `http://127.0.0.1:${port}`, `http://localhost:${port}`],
  });

  const stopFeed = startFeed({
    host,
    dial: socketDial(socketPath),
    dataPlane,
    onLog: (line) => console.log(`[sidecar] ${line}`),
  });

  console.log(`[sidecar] serving http://${bind}:${port} (daemon: ${socketPath}, data plane: ws://${dataPlane.host}:${dataPlane.port})`);

  const shutdown = (): void => {
    stopFeed();
    server.close();
    void host.stop().finally(() => process.exit(0));
  };
  process.on('SIGINT', shutdown);
  process.on('SIGTERM', shutdown);
}

void main();

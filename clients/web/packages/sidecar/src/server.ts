/**
 * The HTTP face: the built app's static files and the actors socket, one
 * origin. Same-origin is what lets the socket session's default posture hold
 * with no configuration — the page and `/_sigx/socket` share
 * `127.0.0.1:<port>`, and nothing else passes the check.
 *
 * Hand-rolled static serving because the app is a handful of build artifacts
 * and a dependency-shaped file server would be the sidecar's biggest dep.
 */

import { createServer, type Server } from 'node:http';
import { readFile } from 'node:fs/promises';
import { extname, join, normalize } from 'node:path';

import type { Host } from '@sigx/actors/host';
import { attachActorSocket } from '@sigx/actors-ws/node';

const TYPES: Record<string, string> = {
  '.html': 'text/html; charset=utf-8',
  '.js': 'text/javascript; charset=utf-8',
  '.mjs': 'text/javascript; charset=utf-8',
  '.css': 'text/css; charset=utf-8',
  '.json': 'application/json',
  '.svg': 'image/svg+xml',
  '.png': 'image/png',
  '.ico': 'image/x-icon',
  '.woff2': 'font/woff2',
  '.map': 'application/json',
};

export interface SidecarServerOptions {
  readonly host: Host;
  readonly port: number;
  readonly bind?: string;
  /** The app's `vite build` output. Omit to serve only the socket (dev mode). */
  readonly staticDir?: string;
  /**
   * Extra allowed origins beside same-origin — the vite dev server's, in
   * practice. Empty in production, where same-origin is the whole point.
   */
  readonly allowOrigins?: readonly string[];
}

export async function startServer(options: SidecarServerOptions): Promise<Server> {
  const { host, port, staticDir } = options;
  const bind = options.bind ?? '127.0.0.1';

  const server = createServer((req, res) => {
    void (async () => {
      if (staticDir === undefined) {
        res.writeHead(404).end('sidecar: no static dir; dev mode serves the app from vite');
        return;
      }
      const url = new URL(req.url ?? '/', 'http://localhost');
      // Normalize, then refuse anything that still climbs: the classic
      // traversal guard, kept boring.
      const rel = normalize(url.pathname).replace(/^([/\\])+/, '');
      if (rel.startsWith('..')) {
        res.writeHead(400).end();
        return;
      }
      const path = join(staticDir, rel === '' ? 'index.html' : rel);
      try {
        const body = await readFile(path);
        res
          .writeHead(200, { 'content-type': TYPES[extname(path)] ?? 'application/octet-stream' })
          .end(body);
      } catch {
        // A client-side route, not a missing file: the SPA fallback.
        try {
          const index = await readFile(join(staticDir, 'index.html'));
          res.writeHead(200, { 'content-type': TYPES['.html'] as string }).end(index);
        } catch {
          res.writeHead(404).end();
        }
      }
    })();
  });

  const origins = options.allowOrigins ?? [];
  attachActorSocket(server, {
    host,
    // `same-origin` alone in production; the dev server's origin is an
    // explicit, visible exception rather than a loosened default.
    origin: origins.length === 0 ? 'same-origin' : [...origins],
  });

  await new Promise<void>((resolve, reject) => {
    server.once('error', reject);
    server.listen(port, bind, resolve);
  });
  return server;
}

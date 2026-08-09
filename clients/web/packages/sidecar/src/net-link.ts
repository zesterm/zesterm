/**
 * The `Dial` seam over a Node socket — the sidecar's leg of the data-plane
 * seam family. Unix socket or Windows named pipe; `net.connect` speaks both
 * from one path argument, which is why this file has no platform branch.
 */

import { connect } from 'node:net';

import type { ByteLinkHandlers, Dial } from '@zesterm/client';

/** Where this user's daemon listens — `zest-daemon --socket-path`'s answer, mirrored. */
export function defaultSocketPath(): string {
  if (process.platform === 'win32') {
    const user = sanitize(process.env['USERNAME'] ?? 'default');
    return `\\\\.\\pipe\\zesterm-${user}`;
  }
  const dir = process.env['XDG_RUNTIME_DIR'];
  if (dir !== undefined && dir !== '') return `${dir}/zesterm.sock`;
  const user = sanitize(process.env['USER'] ?? 'default');
  return `/tmp/zesterm-${user}.sock`;
}

function sanitize(name: string): string {
  return name.replace(/[^A-Za-z0-9_-]/g, '_');
}

/** Dial the daemon's loopback socket. Each call is one fresh connection. */
export function socketDial(path: string): Dial {
  return (handlers: ByteLinkHandlers) => {
    const socket = connect(path);
    // One terminal callback however the socket dies: `close` always follows
    // `error`, so routing only `close` keeps the client's redial from firing
    // twice per outage.
    socket.on('connect', () => handlers.onOpen());
    socket.on('data', (chunk) => handlers.onMessage(new Uint8Array(chunk)));
    socket.on('error', () => {});
    socket.on('close', () => handlers.onClose());
    return {
      send: (bytes: Uint8Array) => {
        socket.write(bytes);
      },
      close: () => {
        socket.destroy();
      },
    };
  };
}

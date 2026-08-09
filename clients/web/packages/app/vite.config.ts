/**
 * Client-only in v1: the sidecar serves static files, and the actors host
 * lives in the sidecar — NOT in a vite dev host, which is why this config
 * does not use `sigxActors()`. Running the directory actor in two hosts
 * would give the feed one and the browser the other.
 *
 * Dev traffic reaches the sidecar through the proxy, so there is exactly one
 * actors code path in dev and prod alike. Start the sidecar first:
 *   pnpm --filter @zesterm/sidecar start
 * The actors socket is same-origin in production; in dev the sidecar must be
 * told about vite's origin: --allow-origin http://localhost:5173
 */
import { defineConfig } from 'vite';
import sigxPlugin from '@sigx/vite';

export default defineConfig({
  // JSX compiles to sigx's runtime; tsconfig's jsxImportSource only informs
  // the type checker, this is what informs the transform.
  oxc: { jsx: { runtime: 'automatic', importSource: 'sigx' } },
  server: {
    proxy: {
      '/_sigx/socket': { target: 'ws://127.0.0.1:7350', ws: true },
    },
  },
  plugins: [sigxPlugin()],
});

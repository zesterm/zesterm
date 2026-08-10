# `cloud/` — zesterm on Cloudflare

The hosted web client, and later the accounts API and the relay. **A third
project**, beside the Cargo workspace and `clients/web`: its own
`pnpm-workspace.yaml` and its own lockfile.

Not folded into `clients/web` on purpose. That workspace has no build tooling
except vite, and its dependency policy — `proto`, `theme`, `input` and
`settings` stay dependency-free, crypto only in `auth` — is a documented
feature. `wrangler` and `@cloudflare/workers-types` in that lockfile would
pollute a tree whose smallness is the point. The two projects share exactly one
thing, the built app, and that is a **directory path** rather than a package
dependency.

```
packages/web/   the Worker that serves the built app and /api/*
```

## One build, two worlds

The app must not learn its environment from a `VITE_*` build variable, or the
bundle you tested is not the bundle you shipped. It asks at runtime:

```
GET /api/bootstrap  ->  { "mode": "local" }                  served by the sidecar
                    ->  { "mode": "cloud", "user": null }    served by this Worker
```

So `pnpm --filter @zesterm/app build` produces one artifact that works on
loopback and at the edge.

## The thing that will cost you a day if nobody says it

**An `https://` page cannot open `ws://192.168.1.5:7718`.** Mixed content, and
there is no workaround short of a real certificate on every daemon.

The consequence is structural, not a defect: **the deployed web client can never
use the LAN data plane.** All of its terminal traffic goes through the relay —
including to the machine you are sitting at. `ws://localhost` from an https page
is allowed in Chrome and blocked in Firefox; do not design around it.

That is what the browser *is* here: the away client. The desktop app is the
local one, and the sidecar (`http://127.0.0.1:7350`, where `ws://` is legal)
stays as the local browser path. **The sidecar path is not a subset of the cloud
path — it is the path that survives Cloudflare being unreachable**, which
ADR-005 requires of a local terminal.

## Deploying

Nothing here deploys yet: there is no Cloudflare account wired up and
`APP_ORIGIN` is a placeholder. When there is, the order matters —

```sh
pnpm -C clients/web --filter @zesterm/app build   # the Worker serves this directory
pnpm -C cloud --filter @zesterm/web run deploy
```

## Gates

```sh
pnpm -C cloud install
pnpm -C cloud -r typecheck
pnpm -C cloud -r test        # node --test; no workerd needed, the routing is pure
pnpm -C cloud -r dry-run     # wrangler bundles and validates the config
```

`dry-run` is the one that earns its keep. It validates `wrangler.jsonc` with no
credentials and no network — a binding with no matching migration, a missing
entrypoint, a renamed class. Without it a wrangler config is only ever wrong at
deploy time, which is the worst possible moment to find out. It needs the app's
`dist/`, so build that first; CI does, which also proves the two trees agree.

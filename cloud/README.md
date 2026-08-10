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
packages/shared/  cookie signing, opaque tokens, constant-time compare —
                  zero deps, and no runtime globals beyond what Node and
                  workerd both have, so it is testable under `node --test`
packages/web/     the Worker: the built app, /api/*, /auth/*, D1
```

## Accounts

GitHub OAuth, hand-rolled. The session cookie is `__Host-zt_session`, an opaque
48-byte token; what the database stores is `sha256(token)`, so a dump of
`sessions` is a list of hashes rather than a set of usable cookies.

The tests run the **real** migration against `node:sqlite` — D1 is SQLite, and
Node 24 ships one — so the foreign keys, the primary keys and the
`email_verified = 1` filter behind account linking are exercised rather than
mocked. The whole OAuth round trip runs against a stubbed GitHub, including the
failures nobody clicks on purpose: a mismatched state, an expired one, and
GitHub's 200-with-an-error-body.

Migrations are applied by hand for now:

```sh
wrangler d1 migrations apply zesterm --remote     # or --local for wrangler dev
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

## Running it locally

```sh
cp packages/web/.dev.vars.example packages/web/.dev.vars   # then fill it in
pnpm exec wrangler d1 migrations apply zesterm --local
pnpm --filter @zesterm/web-worker dev                      # workerd, port 8787
```

**`APP_ORIGIN` in `.dev.vars` is the reason that file exists.** The CSRF rule
compares each mutating request's `Origin` against it, so with the production
value in place every write from localhost is correctly refused 403 — sign-out
included. That refusal is the rule working, and it makes the local loop useless
without the override.

Sign-in locally also needs the **second** OAuth app: a GitHub OAuth app accepts
exactly one callback URL, so production cannot also serve `localhost`.

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

`test` covers the security-shaped code — cookies, sessions, the OAuth flow —
without deploying anything, which is the point: security code that can only be
exercised by a person signing in is security code that is exercised rarely.

`dry-run` is the one that earns its keep. It validates `wrangler.jsonc` with no
credentials and no network — a binding with no matching migration, a missing
entrypoint, a renamed class. Without it a wrangler config is only ever wrong at
deploy time, which is the worst possible moment to find out. It needs the app's
`dist/`, so build that first; CI does, which also proves the two trees agree.

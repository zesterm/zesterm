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
packages/shared/  cookie signing, opaque tokens, constant-time compare, and
                  the `zesterm-sig-v1` preimage the web Worker verifies
                  against today, and the relay will when it verifies its own
                  attach challenge —
                  zero deps, and no runtime globals beyond what Node and
                  workerd both have, so it is testable under `node --test`
packages/web/     the Worker: the built app, /api/*, /auth/*, D1
packages/relay/   the *second* Worker: the dial-back pipe and the Durable
                  Object a daemon and a browser meet inside
```

## Two Workers, and the deploy order that matters

`packages/web` and `packages/relay` are separate Workers with separate
`wrangler.jsonc` files and separate deploy cadences. That is not tidiness.

**Deploying a Worker that owns a Durable Object class evicts every live
instance of that class.** One Worker serving both jobs would therefore drop
every terminal in the fleet every time anyone changed a stylesheet. So the web
app is deployed freely and the relay deliberately. → ADR-009.

The split is only real while this stays true, and two things can quietly undo
it: giving both Workers the same `name` (two names for one Worker), or pointing
the relay's `durable_objects` binding at another script with `script_name`
(the object then lives on that script's deploys). `packages/relay/test/wrangler-config.test.ts`
asserts against both, because wrangler is happy with either.

What they *do* share is the D1 database, and therefore `cloud/migrations/` —
which is why migrations sit at the project root rather than under a package,
and why `migrations_dir` on each D1 binding points back up at it.

The order, once there is an account to deploy to:

```sh
pnpm -C clients/web --filter @zesterm/app build     # the web Worker serves this directory
# Inside a package, because `wrangler` is a devDependency of the Workers and
# `d1 migrations apply` reads the D1 binding and `migrations_dir` from a config.
pnpm -C cloud --filter @zesterm/web-worker exec wrangler d1 migrations apply zesterm --remote
pnpm -C cloud --filter @zesterm/web-worker run deploy
pnpm -C cloud --filter @zesterm/relay-worker run deploy   # last, and least often
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

## The device registry

What an account owns is public keys: `hosts.id` and `devices.id` **are** the
64-hex Ed25519 keys (ADR-006), so enrolment is a signature rather than a claim.

```
POST /api/enroll/code    signed in    { kind }        -> { code, expiresAt }
POST /api/enroll/claim   a daemon     { code, hostId, label, sig }
GET  /api/hosts          signed in                    -> { hosts: [...] }
GET  /api/devices        signed in                    -> { devices: [...] }
POST /api/hosts/:id/revoke      signed in, own only
POST /api/devices/:id/revoke    signed in, own only
```

The code is eight characters of an alphabet with no `0`/`O` and no `1`/`I`/`L`,
because a person reads it off one screen and types it into another. It lives ten
minutes: for the whole of that window it is a bearer token, and whoever reads it
over your shoulder can enrol *their* machine instead — their signature over it is
perfectly valid.

Two things about `/api/enroll/claim` are worth knowing before changing it.

**It verifies the signature before it spends the code.** Reversed, anybody who
can reach the endpoint burns codes without holding any key at all, and the
person minting them never manages to enrol a machine. Spending is then a
compare-and-set inside one `UPDATE … WHERE used_at IS NULL … RETURNING` — D1
offers no transaction across two `prepare` calls — so a replayed claim matches
no row instead of enrolling twice.

**It is the one route exempt from the `Origin` half of the CSRF rule**, listed
by name in `router.ts`. A daemon is not a browser and sends no `Origin` at all.
The exemption is sound only because the route consults no session cookie: CSRF
is the forgery of a request that succeeds on ambient credentials, and there are
none here. It still requires `content-type: application/json`, which keeps the
one cross-site request a browser makes without a preflight — a form POST — off
it entirely. Whatever an attacker can send with `curl` is theirs to send; what a
victim's browser can be made to send is ours to prevent.

The enrolment preimage in `src/enroll/preimage.ts` is a byte-for-byte port of
`crates/zest-mesh/src/enroll.rs`. The two share no code, so `test/enroll-preimage.test.ts`
pins them with the Rust's own golden hex **and** a signature the Rust
actually produced; without those a drift shows up at bring-up as a signature
mismatch that names neither side. Verification passes `zip215: false` to match
dalek's `verify_strict` — noble's default accepts small-order public keys, and
those verify almost anything.

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
`APP_ORIGIN` is a placeholder. When there is, the order matters — see "Two
Workers, and the deploy order that matters" above.

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

The relay needs no workerd either, for a different reason: its room is written
against the narrow interfaces in `packages/relay/src/room/state.ts`, so
`packages/relay/test/fake-platform.ts` can stand in for the Durable Object
runtime — including its limits, which it enforces, and its eviction, which it
simulates by handing out a fresh state over the same durable data.

`dry-run` is the one that earns its keep. It validates `wrangler.jsonc` with no
credentials and no network — a binding with no matching migration, a missing
entrypoint, a renamed class. Without it a wrangler config is only ever wrong at
deploy time, which is the worst possible moment to find out. It needs the app's
`dist/`, so build that first; CI does, which also proves the two trees agree.

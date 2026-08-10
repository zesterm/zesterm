# zesterm — shared agent guide

> ⚠️ **BRANCH FIRST — never work on `main`.** Before touching ANY file, create a
> worktree (`pnpm wt new <N-short-slug>`) and do everything from
> `<repo>/branches/<N-short-slug>`. This applies to every change, however small —
> editing or committing in the primary checkout (`<repo>/main`) causes conflicts
> for parallel sessions. Check yourself before every commit:
> `git branch --show-current` must print your worktree's branch name — if it
> prints `main` or nothing (detached HEAD), stop.
> Already edited files in `main` by mistake? Move the work, don't commit it:
> `git stash -u` → `pnpm wt new <N-short-slug>` →
> `cd <repo>/branches/<N-short-slug>` → `git stash pop`.

Canonical guidance for **any** AI agent working in this repo (Claude Code, GitHub
Copilot CLI, work agents, …). Tool-specific notes live in `CLAUDE.md`; it defers
here for everything shared — when it conflicts with this file, the tool-specific
file wins for that tool only.

This is the sigx standard agent setup — this file, `scripts/worktree.mjs`,
`scripts/apply-branch-protection.mjs` and a thin tool-specific file — as it
originates in [`signalxjs/repo-template`](https://github.com/signalxjs/repo-template).
Where zesterm diverges from the template it is because zesterm is a Rust
workspace rather than a pnpm monorepo; those points are marked.

## What this is

A GPU-accelerated, themable terminal, and a **fleet**: every machine runs a
daemon and can be reached from every device. The Mac's shell in a window on
Windows; a Linux build watched from a phone.

Rust workspace under `crates/`, plus `xtask/` for the gates, `clients/web/`
(a pnpm workspace, Node 24, `node --test`) for the browser client, and `cloud/`
(a *second*, separate pnpm workspace) for the Cloudflare Workers that host it.
Three projects, three lockfiles — `cloud/README.md` says why the last two are
not one. The repo is `zesterm/zesterm`, base branch `main`.

### Read these first

1. **[docs/ROADMAP.md](docs/ROADMAP.md)** — the plan, current state, and what is
   being built next. Source of truth; issue #1 mirrors it.
2. **[docs/CONTRACTS.md](docs/CONTRACTS.md)** — the seams that hold the clients,
   the daemon and the core together. **Read this before touching a shared type.**
3. **[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)** — decisions that were
   expensive to reach and are cheap to accidentally undo. Argue with the
   reasoning there before changing any of them.

## Development workflow (issue → PR → Copilot review → merge)

**This is mandatory for EVERY agent-driven change — including one-line fixes.
Never commit straight to `main`.**

1. **Issue first.** If no GitHub issue already tracks the work, create one *before*
   writing code and put the plan in it:
   ```sh
   gh issue create --title "<concise title>" --body "<what & why, plus the plan/checklist>"
   ```
   If you worked in plan mode, the approved plan **is** the issue body. Note the
   number it returns (`#N`). Issue #1 is the roadmap mirror and is never the
   issue for a change — open a new one and link it.

2. **Worktree, always.** Never work on `main`. Use the worktree flow (below):
   `pnpm wt new <N-short-slug>` gives an isolated checkout on branch
   `<N-short-slug>`. Don't substitute `git switch -c` in the primary checkout —
   it occupies `<repo>/main`, which parallel sessions share.

3. **Implement & verify.** For a **bug fix, write a failing test that reproduces
   the bug *first*** (red), then make the fix so that test passes (green) — see
   "Test-first bug fixes" under Conventions. Either way, prove the change: the
   seven gates below, plus the TypeScript suite if you touched `clients/web/` or
   any type on the wire. Stage specific files (`git add <path>`), never
   `git add -A`. No co-author trailers.

4. **Open a PR, then request Copilot over GraphQL.** Two steps, in this order;
   the middle line only captures the PR's node id for the second. Reference the
   issue so it auto-closes on merge:
   ```sh
   gh pr create --base main --title "<title>" \
     --body "Closes #N. <short summary of the change>"

   pr_id=$(gh pr view <pr> --repo zesterm/zesterm --json id -q .id)
   gh api graphql -f query='mutation($pr:ID!,$b:ID!){
     requestReviews(input:{pullRequestId:$pr, botIds:[$b], union:true}) {
       pullRequest { reviewRequests(first:5){ nodes {
         requestedReviewer { ... on Bot { login } } } } } } }' \
     -f pr="$pr_id" -f b="BOT_kgDOCnlnWA" \
     --jq '.data.requestReviews.pullRequest.reviewRequests.nodes[].requestedReviewer.login'
   ```
   `BOT_kgDOCnlnWA` is `copilot-pull-request-reviewer`'s node id. The `--jq` is
   not decoration: **read the response back**, because the one thing that goes
   wrong here goes wrong silently. It should print
   `copilot-pull-request-reviewer`; anything else, including nothing, means no
   review was requested and the PR will sit there waiting for one forever. The
   bot posts within a minute or two.

   The PR description becomes the squash commit **body** verbatim, and the PR
   title (with ` (#<pr>)` appended) becomes its subject — see step 6. Write the
   description as the commit body you want on `main`.

   **Do not reach for `--reviewer @copilot` or the REST route.** Both are in the
   sigx template and neither works on this box, in the two different ways that
   are hardest to diagnose:
   - `gh` 2.87.2 cannot resolve `@copilot` and fails
     `gh pr create --reviewer @copilot` with `could not request reviewer:
     '@copilot' not found` — *aborting before the PR is created*, so the error
     reads like a reviewer problem while the actual damage is that you have no
     PR. Same for `gh pr edit <pr> --add-reviewer @copilot`.
   - `POST /pulls/<pr>/requested_reviewers` with
     `reviewers[]=copilot-pull-request-reviewer[bot]` returns **200** and a PR
     object whose `requested_reviewers` is still `[]`. No error, no reviewer,
     nothing in the timeline. This is the one that costs an afternoon.

   (The REST route takes the `[bot]`-suffixed slug; the review author login in
   `.reviews[].author.login` appears *without* the suffix. Neither spelling
   makes it work.)

5. **Wait for Copilot's review, then fix.** Do not merge before it has reviewed. Poll
   until a review by the bot appears, then read it:
   ```sh
   gh pr view <pr> --json reviews -q '.reviews[].author.login'   # wait for "copilot-pull-request-reviewer"
   gh pr view <pr> --json reviews,comments
   ```
   Address every actionable comment with follow-up commits and push. If the review
   doesn't re-trigger on its own, re-request it by re-running the GraphQL mutation
   from step 4 — `union:true` makes it idempotent, and `--add-reviewer @copilot`
   fails here for the reason given there.
   Repeat until Copilot has no remaining actionable feedback.

   **Then resolve the threads.** The ruleset sets
   `required_review_thread_resolution` (check with
   `gh api repos/zesterm/zesterm/rules/branches/main`), so a PR carrying an
   unresolved **inline** comment cannot merge however green it is — with a
   merge queue it silently never enqueues, and `gh pr checks` shows nothing
   wrong. Pushing the fix does not resolve a thread, and neither does replying
   at PR level. There is no `gh pr` porcelain — reply on each thread and
   resolve it over GraphQL:
   ```sh
   # list the open threads
   gh api graphql -f query='query { repository(owner:"zesterm", name:"zesterm") {
     pullRequest(number:<pr>) { reviewThreads(first:100) { nodes {
       id isResolved comments(first:1){nodes{body}} } } } } }' \
     -q '.data.repository.pullRequest.reviewThreads.nodes[]
         | select(.isResolved==false) | "\(.id) \(.comments.nodes[0].body[0:60])"'

   # reply (say which commit fixed it), then resolve — pass the body as a
   # GraphQL variable, not string-interpolated: quotes and backslashes in a
   # review reply otherwise break the query
   gh api graphql -f query='mutation($t:ID!,$b:String!){
     addPullRequestReviewThreadReply(input:{pullRequestReviewThreadId:$t, body:$b}){ comment { id } } }' \
     -f t="<thread-id>" -f b="Fixed in <sha>. <what changed>"
   gh api graphql -f query='mutation($t:ID!){
     resolveReviewThread(input:{threadId:$t}){ thread { isResolved } } }' -f t="<thread-id>"
   ```

6. **Merge it yourself.** Once Copilot's feedback is resolved and CI is green,
   merge (squash — repo rules block merge commits) and clean up:
   ```sh
   pr=123                                     # your PR number (digits only)
   gh pr checks "$pr"                         # must be all green first
   gh pr merge "$pr" --squash --delete-branch \
     --subject "$(gh pr view "$pr" --json title -q .title) (#$pr)" \
     --body "$(gh pr view "$pr" --json body -q .body)"
   ```
   Pass `--subject`/`--body` explicitly, exactly as above — GitHub appends
   `Co-authored-by:` trailers to every message it generates itself (in **all**
   squash-message modes, even PR_TITLE/PR_BODY) whenever a branch-commit author
   differs from the merging account; an explicit message is used verbatim, so
   no trailers. Then remove the worktree: `pnpm wt rm <name>`.

### `main` protection

The ruleset **"sigx-standard: protect main"** (id `20627800`) is active on
`zesterm/zesterm`: no direct pushes, no force-push, no deletion, squash-only
merges, review threads must resolve, zero approving reviews required so the
owner may self-merge once Copilot has reviewed. Every one of this repo's
check-runs must be green and the branch must be up to date:

```
test (windows-latest)  test (macos-latest)  test (ubuntu-latest)
invariants  web client  cloud workers
```

**Read the live state, never this paragraph** — a disabled ruleset that the
guide calls active is the worse of the two failures, and this file has already
been wrong in both directions:

```sh
gh api repos/zesterm/zesterm/rules/branches/main   # [] means nothing is enforced
```

Reconcile drift, or restore it after a deliberate pause, with the whole command
— the script is idempotent:

```sh
pnpm branch-protection zesterm/zesterm --approvals 0 \
  --checks "test (windows-latest); test (macos-latest); test (ubuntu-latest); \
            invariants; web client; cloud workers"
```

Those names are real and confirmed reporting — the first five on PR #25,
`cloud workers` on #32. Requiring a name that never reports blocks every merge
forever, which is why the script makes checks opt-in rather than guessing — so
if CI ever grows or renames a job, this list and `.github/workflows/ci.yml`
move together or merges stop.

**Both directions.** A new job has to report on a real PR *before* it is added
here, so adding it is a follow-up commit rather than part of the PR that
introduced it. And because the command above replaces the ruleset wholesale, a
name missing from this list is silently *removed* from enforcement the next
time anyone runs it to reconcile drift — a restore command that quietly reduces
protection, which is worse than one that is merely out of date.

To pause enforcement without losing the configuration (what "turn protection
off while the in-flight work lands" means), set `enforcement` to `disabled`
rather than deleting the ruleset — then re-run the command above to restore it:

```sh
gh api -X PUT repos/zesterm/zesterm/rulesets/20627800 --input - <<'EOF'
{ "name": "sigx-standard: protect main", "target": "branch", "enforcement": "disabled",
  "conditions": { "ref_name": { "include": ["refs/heads/main"], "exclude": [] } },
  "rules": [
    { "type": "deletion" },
    { "type": "non_fast_forward" },
    { "type": "pull_request", "parameters": {
        "required_approving_review_count": 0, "dismiss_stale_reviews_on_push": true,
        "require_code_owner_review": false, "require_last_push_approval": false,
        "required_review_thread_resolution": true } }
  ],
  "bypass_actors": [] }
EOF
```

The `rules` array is repeated because a `PUT` replaces the ruleset wholesale —
omit it and you have silently emptied the thing you meant to pause. It matters
less than it looks, since the restore command rebuilds the whole ruleset from
the script, but a half-erased ruleset that still reports as existing is exactly
the state that makes people trust the name over the contents.

While it is off the workflow above is **not** suspended — it is merely held up
by discipline instead of by GitHub, which is the weaker of the two and the
reason a pause should be short. Branch first anyway.

## The gates

All seven must pass before you call something done:

```
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo xtask check-deps
cargo xtask check-schema
cargo xtask check-bindings
cargo xtask check-fixtures
cargo xtask check-export-web
```

`check-export-web` is in this list, unlike the TypeScript suite below, for the
reason that keeps them apart: a **Rust-only** change to `zest-config` or
`zest-theme` breaks it. The settings schema, the walked UI fields and the
built-in themes are generated into `clients/web/`, so editing a theme's hex or
adding a setting leaves them stale with nothing else to notice.

And, **if you touched `clients/web/` or any type on the wire**, the TypeScript
suite too. It is not in the list above on purpose: a Rust-only change that passes
`check-fixtures` cannot break it, and a gate people learn to skip is worse than
no gate.

```
pnpm -C clients/web install
pnpm -C clients/web -r typecheck
pnpm -C clients/web -r test
```

And if you touched `cloud/`, that project too. `dry-run` is the one worth
knowing about: it bundles and validates `wrangler.jsonc` with no credentials
and no network, which is what catches a binding whose class is never exported
or a renamed entrypoint — otherwise only ever wrong at deploy time.

```
pnpm -C cloud install
pnpm -C cloud -r typecheck
pnpm -C cloud -r test
pnpm -C cloud -r dry-run        # needs clients/web/packages/app/dist built first
```

CI runs all of it on Windows, macOS and Linux plus the wasm32 build — see
`.github/workflows/ci.yml`.

## The one invariant

`zest-core` must never depend on `wgpu`, `winit`, `windows`, or `tokio`, and must
build for `wasm32-unknown-unknown`. This is what lets the native app, the future
daemon, and the browser/mobile clients share one terminal implementation instead
of three that quietly diverge.

```
cargo xtask check-deps
```

If a dependency genuinely belongs, move the *code* up a layer rather than
relaxing the rule.

## Parallel work with git worktrees

To work two things at once — each with its own checkout and its own agent
session — use a worktree instead of switching branches in place:

```sh
pnpm wt new <name> [--from <branch>]   # worktree at <repo>/branches/<name>: own branch
pnpm wt list                           # show all worktrees
pnpm wt rm <name> [--force]            # remove a worktree
```

Layout convention (all sigx repos): the primary checkout lives at `<repo>/main`
and every worktree at `<repo>/branches/<name>`. `pnpm wt new` creates the
checkout there on a new branch `<name>`. Launch a **separate agent session from
the worktree directory**; sessions stay independent per directory. Names:
letters, digits, `.`, `_`, `-` only.

**Rust-specific, and the reason this costs more here than in a JS repo:** a
worktree has its own `target/`, so the first `cargo build` in it is a full cold
build of the workspace — minutes, not seconds. That is the price of isolation
and it is usually worth paying; if it isn't, `CARGO_TARGET_DIR` pointed at one
shared directory removes the rebuild at the cost of cargo serializing concurrent
builds on a lock. Don't share it silently — a build that appears to hang for two
minutes is another worktree holding the lock.

`pnpm wt new` installs the web client's dependencies only when it finds
`clients/web/pnpm-lock.yaml`; there is nothing to install for the Rust side.
(This is zesterm's one divergence from the template's `worktree.mjs`, which runs
a plain `pnpm install` at the root.)

## Documentation is part of the change

zesterm has no docs site; the docs are in the repo, and they ship in the same
commit as the work — not as a follow-up.

| When you… | Update… |
|---|---|
| land any roadmap-visible work | `docs/ROADMAP.md` **in the same commit**, then refresh tracking issue [#1](https://github.com/zesterm/zesterm/issues/1). A roadmap that lags is one nobody trusts |
| touch a shared type on a seam | `docs/CONTRACTS.md` — land **every** consumer in the same PR, update the table, say so on #1. A frozen contract with a half-updated consumer is worse than either shape. Adding a new type *beside* one is free |
| undo or revise an expensive decision | `docs/ARCHITECTURE.md` — argue with the reasoning there first |
| change a command, gate or script | this file, and `README.md` if it names it |
| change the workflow / process itself | this file — and, since it is the shared sigx standard, upstream the same change to [`signalxjs/repo-template`](https://github.com/signalxjs/repo-template) |
| pay for a new trap | "Traps already paid for" below, plus a comment where it bites |

**Never edit the root `Cargo.toml` or `Cargo.lock` by hand.** Every crate the
project will have is registered already, including the skeletons. Adding a
*dependency* to your own crate's manifest is fine.

## Commands

```
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
cargo xtask check-deps
cargo build -p zest-core --no-default-features --target wasm32-unknown-unknown

cargo xtask schema                             # regenerate the settings JSON Schema
cargo test -p zest-proto --features ts         # regenerate the TypeScript bindings
cargo xtask fixtures                           # regenerate the conformance fixtures
cargo xtask export-web                         # regenerate the web client's schema, UI fields and themes
cargo run -p zest-proto --example fixture_dump -- --only vim-macos --print 7
                                               # one fixture frame, decoded, to stdout

zesterm-dev                                    # build both binaries and open a window
zesterm-dev --no-build --attach-probe          # probe flags stay in the foreground
.\scripts\zesterm-dev.ps1                      # the same thing, in PowerShell on Windows
cargo run --profile fast -p zest-app           # the terminal, quick rebuild
./target/fast/zesterm --startup-probe          # time to first paint; fails over 100ms
cargo build --release && ./target/release/zesterm   # the shipping build
cargo run -p zest-app  --example headless      # a terminal with no window
cargo run -p zest-font --example font_dump     # font sample sheet as a PNG
cargo run -p zest-pty  --example pty_dump      # raw VT stream / corpus recorder
cargo run -p zest-render-wgpu --example alpha_probe   # transparency capability

zest-daemon --socket-path                      # where this user's daemon listens
zest-daemon --socket <path>                    # serve this machine's terminals
zest-daemon --listen-lan                       # serve other machines too (off by default)
zest-daemon --listen-ws                        # serve browsers over WebSocket (off by default, port 7718)
zest-daemon --identity                         # this host's public key
zest-daemon --trusted                          # which devices are paired
zest-daemon --ephemeral                        # throwaway key, for the edit-run loop
zest-daemon --enroll <code>                    # join this machine to an account (no transport yet — see enroll.rs)
zest-daemon --account                          # what this machine has stored; never the token itself
zest-daemon --logout                           # forget this machine's copy of the token
cargo run -p zest-daemon --example attach      # drive a daemon session, no GUI
cargo run -p zest-mesh   --example mesh_probe  # advertise and browse the fleet
cargo run -p zest-daemon --example pair -- --addr <host:port>   # pair with a host

pnpm -C cloud -r dry-run                       # validate the Worker configs, no credentials needed
pnpm -C cloud --filter @zesterm/web dev        # the hosted client under workerd, port 8787

pnpm wt new <N-slug>                           # a worktree for issue #N
pnpm wt list
pnpm wt rm <N-slug>
pnpm branch-protection zesterm/zesterm         # reconcile main's ruleset
```

Each `--example` above answers "which layer is wrong" without the ones above it.
`attach` is the daemon's `headless`: when a session renders wrongly in the app it
says whether the daemon or the renderer is at fault, with no window, GPU or font
involved. `mesh_probe` is the two-machine check no unit test can perform — it
reports **self-visible** separately from **peers**, so "my multicast is not
leaving this box" and "nothing else is advertising" are distinguishable.

## Conventions & working principles

- **Plan first for non-trivial work.** Both Claude Code and Copilot CLI have a
  built-in plan mode; use it and let the CLI manage the plan file. The approved
  plan is the issue body.
- **Verify before declaring done.** Run the gates; show evidence the change
  works. "It should work" is not evidence.
- **Test-first bug fixes.** Reproduce the bug with a *failing* test first (red),
  then make the fix so it goes green — the failing test proves both that the bug
  exists and that the fix addresses it, and it stays behind as a regression test.
  Never fix a bug without a test that would have caught it. While you're in the
  area, if you find behaviour that should be covered but isn't, add the missing
  tests in the same PR.
- **Tests assert behaviour with a reason.** `assert!(x, "why this matters")`.
  Several existing tests exist purely to catch silent regressions (cell size,
  allocation-free scrolling, 0%-idle damage) — those are load-bearing, not
  ceremony.
- **Comments explain *why*, never *what*.** The non-obvious constraint, the
  rejected alternative, the bug this shape prevents. If a line is self-evident,
  it gets no comment.
- **Minimal, surgical edits.** Don't refactor unrelated code. Don't add
  backward-compat shims for things that never shipped.
- **Find bugs at the cheapest layer.** The font PNG dump and the headless
  terminal both exist because diagnosing through a renderer means first guessing
  which layer is wrong.
- **Cross-platform paths**: Windows is the primary platform and CI runs all
  three — use the path separator and shell syntax of the environment you're in,
  and prefer Node or Rust over shell one-liners for anything committed.
- **Git hygiene**: stage specific files (`git add <path>`), never `git add -A` /
  `git add .`. Do **not** add co-author trailers to commits (e.g.
  `Co-Authored-By: Claude …` / `Co-authored-by: Copilot …`).

## Traps already paid for

Each of these cost real time and is documented where it bites:

- **ConPTY needs `STARTF_USESTDHANDLES` with null handles**, or the child writes
  to the parent's stdout instead of the pty whenever ours is redirected — which,
  for a terminal, is always. Every API call still reports success.
  (`zest-pty/src/windows.rs`, gotcha 5.)
- **`ClosePseudoConsole` deadlocks** unless the reader is still draining, which
  dictates the whole shutdown protocol. The reader also cannot observe child
  exit at all.
- **Windows serializes I/O per handle on a *synchronous* named pipe**, so a
  reader thread sitting in `ReadFile` — which is exactly what a server does
  while a client is quiet — holds off a writer thread on that same handle. The
  writes return success and simply never arrive, and the peer sees a connection
  that is established, greeted, and then silent. `DuplicateHandle` does not help;
  it names the same file object. The fix is `FILE_FLAG_OVERLAPPED` on both ends
  with a per-operation `OVERLAPPED` and event — and `ConnectNamedPipe` must then
  be overlapped too, or it returns without waiting and the server serves a
  connection nobody made. (`zest-daemon/src/local.rs`.)
- **On macOS, `TIOCSWINSZ` on the pty master fails with `ENOTTY` until the slave
  has been opened once.** Setting the initial size right after `unlockpt` — the
  obvious place, and what Linux accepts — therefore fails with an error saying
  the fd is not a terminal, which it plainly is. Set it on the slave.
  (`zest-pty/src/unix.rs`, gotcha 3.)
- **Closing a unix pty master cannot hang up a pty whose reader is parked in
  `read`.** The hangup fires when the *last* duplicate of the master fd closes,
  and the blocked reader holds one; it cannot let go until the read returns, and
  the read will not return until the hangup. Every call involved succeeds and
  the shell simply lives on. A short-lived owner never sees this — the process
  exits and takes every fd with it — so it survived until a daemon started
  closing one session out of many. `PtyTransport::hangup` signals the session's
  process group instead. (`zest-pty/src/unix.rs`, gotcha 5.)
- **A unix pty master reports EOF as `EIO`, not as a zero-length read.** Treat it
  as EOF or every clean shell exit logs an I/O error and looks like a crash.
  (`zest-pty/src/unix.rs`, gotcha 2.)
- **macOS's `/bin/sh` does not pass `SIGINT` on when non-interactive**, so a
  `sh -c 'sleep 30'` test child survives a `^C` that a working pty delivered
  correctly. It makes a correct implementation look broken; spawn the binary
  directly in tests. Verified against a C reference before believing it.
- **macOS delivers filesystem events under the resolved path** — `/var` and
  `/tmp` are symlinks into `/private` — so comparing a watched path literally
  against `notify`'s event paths silently never matches, and the config simply
  stops reloading. (`zest-config/src/watch.rs`.)
- **A DNS-SD *instance name* is not a *host name*.** The instance is
  `andy-mac (1f2a3b4c)` — spaces and parentheses are legal and expected — while
  the SRV target must be a DNS label, `[A-Za-z0-9-]`. Derive one from the other
  and the responder cheerfully publishes the service, no A record ever resolves
  for that target, and peers find the host with an **empty address set**. It
  then appears in the fleet listing with no route, which is indistinguishable
  from a laptop that is asleep. The host name is built from the `HostId`
  instead. (`zest-mesh/src/discovery/mdns.rs`, sharp edge 5.)
- **DX12 cannot do per-pixel alpha** through wgpu's ordinary surface path.
  Transparency on Windows is adapter-dependent. Premultiply everywhere
  regardless. (ADR-003.)
- **Emoji are script `Zyyy` and Nerd Font icons are Private Use Area**, so
  script-based font fallback structurally cannot find either. Emoji need an
  explicit `GenericFamily::Emoji` path; PUA needs an installed Nerd Font,
  discovered by name. Get this wrong and the user's shell prompt is blank.
- **Window opacity applies only to cells whose background is `Color::Default`.**
  Applying it to every cell makes TUI panels see-through.
- **A failing pty test that prints raw VT clears your terminal** and scrambles
  its own failure message. Escape test output.
- **`rmp-serde` writes the narrowest integer that fits**, so a `u64` that `ts-rs`
  types as `bigint` reaches a JavaScript decoder as a plain `number` for every
  realistic value. A client that believes the binding and compares `seq === 1n`,
  or calls a `BigInt` method on it, is wrong for every real session and correct
  only for absurd ones. Normalized at one boundary in
  `clients/web/packages/proto/src/wire.ts`; the real fix is in the Rust
  attributes.
- **HKDF-Expand and HKDF are different functions, and the ratchet needs
  Expand.** The session keys are Extract-then-Expand off the transcript hash;
  the rekey at 2^24 records is Expand *alone*, because the current key already
  is the PRK. Full HKDF with an empty salt produces a different key and is one
  identifier away — `@noble/hashes/hkdf` exports both `hkdf` and `expand`. No
  ordinary test can catch it: the branch is 16 million records into a session,
  so it ships and is reported as "it dies after a few hours".
  `fixtures/handshake.json` carries two records straddling the boundary for
  exactly this reason, and caught it the first time a second implementation was
  written against it. (ADR-008.)
- **The `v2` in `zesterm-auth-v2` counts transcript layouts, not protocol
  versions.** The protocol is at 3. Deriving one from the other is one line
  away and produces signatures that will not verify, with nothing in the error
  naming the cause. A test pins the literal and asserts the two numbers differ.
- **A sealed frame's length prefix describes the ciphertext**, which is 16 bytes
  longer than the plaintext. Bound the *plaintext* against `MAX_FRAME` instead
  and every small test passes while a maximal keyframe fails — that is, it
  breaks only on very large grids.
- **The seal switch is positional, and its two halves flip at different
  moments.** Incoming flips when the `Challenge` is *produced*, so a client that
  pipelines its `Auth` behind the `Hello` is still read; outgoing flips when the
  `Challenge` is *written*, because that frame carries the DH key and cannot be
  sealed under a key derived from itself. One flag for both is a bug that only
  appears under pipelining. In the browser, open a frame where it is
  *processed*, not where it arrives — a frame landing while a signature is
  pending is stalled, and at that moment the channel does not exist yet.
- **A JavaScript client must iterate code points, never `text.length`.** That
  counts UTF-16 code units, so one astral-plane emoji counts as two and every
  cell after it shifts left. **CJK does not catch this** — it is BMP, where the
  two counts agree — so the entire recorded corpus was blind to it until a
  synthetic `astral` fixture was added. The corpus now refuses to generate
  without something past U+FFFF in it.
- **`cargo run` costs ~500ms** of workspace resolution and freshness checking
  before the process starts, which is comparable to zesterm's whole startup.
  Measure and demo with the built binary, or startup numbers are meaningless.
- **`--release` is slow to rebuild** (thin LTO, one codegen unit): ~51s for a
  one-line change versus ~3.6s on `--profile fast`. Use `fast` for the edit-run
  loop; it is within a few percent at runtime, so startup and frame numbers
  measured on it are still meaningful.
- **Release builds are GUI-subsystem**, so a shell will not wait for them and
  `zesterm --themes` returns the prompt before printing. That is normal for a
  GUI app; use `Start-Process -Wait` when scripting against it. Debug builds
  keep the console subsystem so the dev loop is unaffected.
- **The daemon's environment is frozen at first spawn, and every shell in the
  fleet inherits it.** A terminal that spawns its own shell leaks only its own
  launch context; zesterm's shells come from a long-lived daemon, so a daemon
  that happened to start from inside an agent session or an IDE hands those
  markers to every window opened afterwards, for hours, from anywhere. Found
  when `claude` inside zesterm reported transcript saving off, having inherited
  `CLAUDE_CODE_CHILD_SESSION`. The markers are cleared in `terminal_env()`
  alongside the terminal-identity ones — but the general hazard remains, so
  anything context-specific in a shell's environment is worth suspecting there
  first. (`zest-pty/src/lib.rs`.)
- **"Always Allow" on the Keychain does not survive `cargo build`.** macOS binds
  the grant to the *binary* that asked, and dev builds are ad-hoc signed
  (`codesign -dv` says `Signature=adhoc`), so every rebuild is a different
  executable and a stranger to the ACL. You did not approve `zest-daemon`; you
  approved that build of it. Signing dev builds with a stable self-signed
  identity would fix it properly; `--ephemeral` sidesteps the keychain entirely
  and is why host ids churn during a bring-up.

  **Measured, so it is not folklore:** an ad-hoc binary designates
  `cdhash H"..."` and two consecutive builds differ; signed with a real identity
  it designates `identifier "..." and anchor apple generic and certificate
  leaf[subject.CN] = "..."`, which is byte-identical after every rebuild. The
  Keychain matches on the designated requirement, so the second survives and the
  first cannot. `export ZESTERM_SIGN_IDENTITY="$(security find-identity -v -p
  codesigning | head -1 | sed 's/.*"\(.*\)"/\1/')"` and `zesterm-dev` signs both
  binaries for you.

  **Windows has no equivalent problem, measured the same way:** a freshly
  rebuilt `zesterm.exe` re-read the stored client key and attached to a remote
  daemon in 48ms with no prompt of any kind. Credential Manager keys generic
  credentials by *target name*, not by binary, so no rebuild can lose the
  grant — and the flip side is worth knowing too: there is no per-binary ACL
  at all, so any process in the user's session can read the key. The signing
  discipline above is macOS-only work; the Windows exposure is the session,
  not the build.
- **On macOS the daemon blocks on a Keychain prompt after every rebuild**, and
  the app gives up waiting after 2s and silently falls back to an in-process
  pty. The window works perfectly and is not daemon-backed, so anything being
  tested through the daemon is not being tested at all — `origin=InProcess` in
  the startup line is the only sign. Keychain keys access to the *binary*, so a
  fresh build is a fresh prompt. Start the daemon yourself with `--ephemeral`
  for the edit-run loop.
- **The agent shell sets `NO_COLOR=1`**, and a pty child inherits it. PowerShell
  honours it by forcing `$PSStyle.OutputRendering = 'PlainText'`, which strips
  every escape *before* it reaches the pty — so a colour test launched from here
  renders monochrome and looks exactly like a broken renderer. It cost a long
  detour once. `Remove-Item Env:\NO_COLOR` before any visual check, and confirm
  a suspected colour bug offscreen with
  `render_dump --replay <capture>` before believing the window.
- **`Start-Process -ArgumentList` does not re-quote array elements**, so
  `'--font','My Font'` reaches the program as two arguments. Quote inside the
  string (`'"My Font"'`). This is the harness, not the argument parser — verify
  which before changing code.
- **Git Bash rewrites unix-looking arguments before the program sees them**
  (MSYS path conversion). `--socket '\\.\pipe\x'` arrives with a backslash
  eaten and the daemon exits on os error 123; worse, `--cmd /bin/cat` sent to a
  *remote* daemon becomes `C:/Program Files/Git/usr/bin/cat` on the wire, and
  the macOS host faithfully tries to spawn a Windows path — a failure that
  reads as the far host being broken. Quoting does not help — the conversion
  runs after the shell. Use PowerShell for anything carrying pipe paths or
  paths destined for another machine, or set `MSYS_NO_PATHCONV=1`. Both halves
  of this bit on the same day, over the same feature. (#20.)
- **Git Bash on the Windows box has no `jq`**, though `gh` embeds one. Shelling
  out to `jq` dies with `command not found` — harmless in a one-shot command,
  quietly fatal inside a polling loop, where the empty result is
  indistinguishable from "the job hasn't finished yet" and the loop waits out
  its timeout reporting nothing. Use `gh … --jq` / `-q`, which is gh's own.

## Related work on this machine

`~/dev/sigx` (`C:\Dev\sigx` on the Windows box) is the user's own framework
(github.com/signalxjs), consumed by the web and mobile clients later. Layout is
git-worktree-per-branch, so the real checkout is `<repo>/main/` — the same
layout this repo now uses. Note `@sigx/terminal` renders TSX *to* a TTY — it is
not a terminal emulator and cannot be the web client's grid renderer. Its
`terminal-zero` token contract *is* reused: `zest-theme`'s `UiTokens` is that
record field-for-field, so `{...theme.ui, name, mode}` is a valid argument to
sigx's `registerTheme()`.

`clients/web/` is a pnpm workspace, Node 24, `node --test`. The proto/theme/
input packages have no runtime dependencies (framing, MessagePack and delta
application are hand-written); crypto is quarantined in `auth`, and sigx
(`@sigx/actors` 0.7.0 with its WebSocket transport) appears only in
`control`/`sidecar`/`app`. Decode+apply runs on the main thread by measurement,
not in a worker — see the README. The sigx packages are published to npm; the
local checkouts lag, so install from npm rather than linking.

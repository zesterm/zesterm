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

1. **[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)** — what the system *is* (the
   "The system" overview: goal, layers, wire path, crate map), then the ADRs:
   decisions that were expensive to reach and are cheap to accidentally undo.
   Argue with the reasoning there before changing any of them.
2. **[docs/CONTRACTS.md](docs/CONTRACTS.md)** — the seams that hold the clients,
   the daemon and the core together. **Read this before touching a shared type.**
3. **[docs/ROADMAP.md](docs/ROADMAP.md)** — current state and open work. Source
   of truth; update it in the same commit as the work it describes.

## Development workflow (issue → PR → Copilot review → merge)

**This is mandatory for EVERY agent-driven change — including one-line fixes.
Never commit straight to `main`.**

1. **Issue first.** If no GitHub issue already tracks the work, create one *before*
   writing code and put the plan in it:
   ```sh
   gh issue create --title "<concise title>" --body "<what & why, plus the plan/checklist>"
   ```
   If you worked in plan mode, the approved plan **is** the issue body. Note the
   number it returns (`#N`).

2. **Worktree, always.** Never work on `main`. Use the worktree flow (below):
   `pnpm wt new <N-short-slug>` gives an isolated checkout on branch
   `<N-short-slug>`. Don't substitute `git switch -c` in the primary checkout —
   it occupies `<repo>/main`, which parallel sessions share.

3. **Implement & verify.** For a bug fix, failing test first — see "Test-first
   bug fixes" under Conventions. Either way, prove the change: the seven gates
   below, plus the TypeScript suite if you touched `clients/web/` or any type on
   the wire, plus the `cloud/` suite if you touched `cloud/`.

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
   not decoration: **read the response back** — it must print
   `copilot-pull-request-reviewer`; anything else, including nothing, means no
   review was requested and the PR waits forever. The bot posts within a minute
   or two.

   The PR description becomes the squash commit **body** verbatim, and the PR
   title (with ` (#<pr>)` appended) becomes its subject — see step 6. Write the
   description as the commit body you want on `main`.

   Two dead ends, both paid for on this box, both in the sigx template: `gh pr
   create --reviewer @copilot` fails with "'@copilot' not found" *before creating
   the PR* (same for `gh pr edit --add-reviewer`), and the REST route
   (`POST /pulls/<pr>/requested_reviewers` with the `[bot]`-suffixed slug)
   returns 200 while requesting nothing. Only the GraphQL mutation above works.

5. **Wait for Copilot's review, then fix.** Do not merge before it has reviewed:
   ```sh
   gh pr view <pr> --json reviews -q '.reviews[].author.login'   # wait for "copilot-pull-request-reviewer"
   gh pr view <pr> --json reviews,comments
   ```
   Address every actionable comment with follow-up commits and push; if the
   review doesn't re-trigger, re-run the step-4 mutation (`union:true` makes it
   idempotent). Repeat until nothing actionable remains.

   **Then resolve the threads.** The ruleset requires review-thread resolution,
   so a PR carrying an unresolved **inline** comment cannot merge however green
   it is — and `gh pr checks` shows nothing wrong. Pushing the fix does not
   resolve a thread, and neither does replying at PR level. There is no `gh pr`
   porcelain — reply on each thread and resolve it over GraphQL:
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

6. **Merge it yourself — with `--auto`.** Once Copilot's feedback is resolved,
   queue the merge (squash — repo rules block merge commits) and clean up:
   ```sh
   pr=123                                     # your PR number (digits only)
   gh pr merge "$pr" --squash --auto --delete-branch \
     --subject "$(gh pr view "$pr" --json title -q .title) (#$pr)" \
     --body "$(gh pr view "$pr" --json body -q .body)"
   ```
   `--auto` merges as soon as the requirements are met, so you do not have to
   sit and watch `gh pr checks`. Pass `--subject`/`--body` explicitly, exactly
   as above — GitHub appends `Co-authored-by:` trailers to every message it
   generates itself whenever a branch-commit author differs from the merging
   account; an explicit message is used verbatim. Then remove the worktree:
   `pnpm wt rm <name>`.

   **You do not need to rebase onto the latest `main` first.** The ruleset
   deliberately does not require an up-to-date branch — with CI in the minutes,
   requiring it made every merge invalidate every open PR. The cost: your PR is
   tested against the `main` it branched from, so a *semantic* conflict (one PR
   renames a function, another adds a caller) can break `main` even though both
   were green alone. Textual conflicts are still blocked. Rebase by hand when
   your change and a freshly-landed one plausibly interact.

### `main` protection

The ruleset **"sigx-standard: protect main"** enforces the workflow above: no
direct pushes, squash-only, review threads must resolve, all six CI check-runs
green, zero approving reviews required so the owner may self-merge once Copilot
has reviewed. Read the live state, never this paragraph:

```sh
gh api repos/zesterm/zesterm/rules/branches/main   # [] means nothing is enforced
```

Reconcile drift, or restore after a deliberate pause, with the idempotent
script (`scripts/apply-branch-protection.mjs`):

```sh
pnpm branch-protection zesterm/zesterm --approvals 0 \
  --checks "test (windows-latest); test (macos-latest); test (ubuntu-latest); \
            invariants; web client; cloud workers"
```

The check names must match `.github/workflows/ci.yml` — a new job has to report
on a real PR *before* being added here, and a name missing from this command is
silently removed from enforcement the next time it runs, so the two move
together or merges stop. Don't add `--strict` (the up-to-date requirement)
without reading step 6's trade-off. To pause enforcement without losing the
configuration, set the ruleset's `enforcement` to `disabled` via
`gh api -X PUT repos/zesterm/zesterm/rulesets/<id>` rather than deleting it,
then restore with the command above — and while it is off, the workflow is held
up by discipline instead of GitHub, which is the weaker of the two. Branch
first anyway.

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

And if you touched `cloud/`, that project too. `dry-run` and `boot` are two
halves, not one check twice: `dry-run` bundles and validates the Worker configs
with no credentials, `boot` actually starts each Worker under workerd and makes
one request — `cloud/README.md` § Gates has the full rationale, including the
relay Worker that could not boot and stayed green for nineteen PRs.

```
pnpm -C cloud install
pnpm -C cloud -r typecheck
pnpm -C cloud -r test
pnpm -C cloud -r dry-run        # needs clients/web/packages/app/dist built first
pnpm -C cloud -r boot           # same; starts each Worker and asks it something
```

CI runs all of it on Windows, macOS and Linux plus the wasm32 build — see
`.github/workflows/ci.yml`.

## The one invariant

`zest-core` must never depend on `wgpu`, `winit`, `windows`, or `tokio`, and must
build for `wasm32-unknown-unknown`. This is what lets the native app, the
daemon, and the browser/mobile clients share one terminal implementation instead
of three that quietly diverge. (ADR-001 has the why.)

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
| land any roadmap-visible work | `docs/ROADMAP.md` **in the same commit**. A roadmap that lags is one nobody trusts |
| touch a shared type on a seam | `docs/CONTRACTS.md` — land **every** consumer in the same PR and update the table. A frozen contract with a half-updated consumer is worse than either shape. Adding a new type *beside* one is free |
| undo or revise an expensive decision | `docs/ARCHITECTURE.md` — argue with the reasoning there first |
| change a command, gate or script | this file, and `README.md` if it names it |
| change the workflow / process itself | this file — and, since it is the shared sigx standard, upstream the same change to [`signalxjs/repo-template`](https://github.com/signalxjs/repo-template) |
| pay for a new trap | "Traps already paid for" below, plus a comment where it bites |

**Never edit `Cargo.lock` by hand** — let cargo write it. The root
`Cargo.toml` registers most crates already, including the skeletons; if yours
is genuinely new, add it to `members` and `[workspace.dependencies]` in the
same commit that creates it. Adding a *dependency* to your own crate's manifest
is fine.

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

zesterm-dev                                    # build the workspace and open a window
zesterm-dev --no-build --attach-probe          # probe flags stay in the foreground
.\scripts\zesterm-dev.ps1                      # the same thing, in PowerShell on Windows
cargo run --profile fast -p zest-app           # the terminal, quick rebuild
./target/fast/zesterm --startup-probe          # time to first paint; fails over 100ms
./target/fast/zesterm --screenshot out.png     # one real frame to a PNG; no window is ever shown
./target/fast/zesterm --theme paper --screenshot-size 1200x800 --screenshot out.png
./target/fast/zesterm --screen themes --screenshot out.png
                                               # open on a design screen: fleet|themes|settings|
                                               #   settings-menu|palette|launcher|profiles|
                                               #   profiles-rename
                                               # ('palette' = the ⌘K search; 'settings-menu' is Settings
                                               #   with the theme dropdown open and 'profiles-rename' is
                                               #   Profiles with the name entry open — the states a
                                               #   screenshot cannot otherwise reach, because opening
                                               #   them takes a click); works without --screenshot too
                                               # fleet is the exception: its content comes from the
                                               #   daemon, which screenshot mode never attaches to, so
                                               #   --screen fleet --screenshot is refused rather than
                                               #   rendered describing the local machine wrongly (#236)
./target/fast/zesterm --tabs-position left     # tab strip placement override, top|left
cargo build --release && ./target/release/zesterm   # the shipping build
cargo run -p zest-app  --example headless      # a terminal with no window
cargo run -p zest-font --example font_dump     # font sample sheet as a PNG
cargo run -p zest-font --example glyph_probe   # hinting vs coverage; prints a verdict
cargo run -p zest-pty  --example pty_dump      # raw VT stream / corpus recorder
cargo run -p zest-render-wgpu --example alpha_probe   # transparency capability

zest-mcp                                       # terminals as an agent's tools, over MCP
                                               #   on stdio. A harness launches it:
                                               #   {"mcpServers":{"zesterm":{"command":"zest-mcp"}}}
zest-mcp --socket <path>                       # talk to a daemon somewhere other than the default
zest-mcp --socket-path                         # print that default and exit

zest-daemon --socket-path                      # where this user's daemon listens
zest-daemon --socket <path>                    # serve this machine's terminals
zest-daemon --listen-lan                       # serve other machines too (off by default)
zest-daemon --listen-ws                        # serve browsers over WebSocket (off by default, port 7718)
zest-daemon --relay <url>                      # dial a relay so this machine is reachable from
                                               #   anywhere, opening no port. An *enrolled* machine
                                               #   dials its account's relay on its own (#229), so
                                               #   this is an override; the origin comes from
                                               #   `GET /api/me` and is cached beside trust.toml.
                                               #   wss://host, or http://127.0.0.1:8787 for a relay
                                               #   on this machine — plaintext is loopback-only
zest-daemon --no-relay                         # dial no relay, even when enrolled (LAN and loopback
                                               #   are unaffected; --ephemeral never dials by itself)
zest-daemon --min-delta-interval <ms>          # coalescing floor; 0 (off) unless the transport asks
                                               #   (--relay sets 30ms per pipe)
zest-daemon --identity                         # this host's public key
zest-daemon --trusted                          # which devices are paired
zest-daemon --ephemeral                        # throwaway key, for the edit-run loop
zest-daemon --enroll <code>                    # join this machine to an account (posts to the control plane)
zest-daemon --account                          # what this machine has stored; never the token itself
zest-daemon --logout                           # forget this machine's copy of the token
cargo run -p zest-daemon --example attach      # drive a daemon session, no GUI
cargo run -p zest-mesh   --example mesh_probe  # advertise and browse the fleet
cargo run -p zest-daemon --example pair -- --addr <host:port>   # pair with a host
cargo run -p zest-cloud  --example shutdown_probe   # the Winsock cut-vs-close truth table, measured

pnpm -C clients/web/packages/sidecar probe:resize
                                               # drive one session over the loopback pipe and dump
                                               #   each block's line range against the rows it holds,
                                               #   before and after a drag

pnpm -C cloud -r dry-run                       # validate the Worker configs, no credentials needed
pnpm -C cloud -r boot                          # start each Worker under workerd and make one request
pnpm -C cloud --filter @zesterm/web-worker dev # the hosted client under workerd, port 8787

pnpm branch-protection zesterm/zesterm         # reconcile main's ruleset
```

(Worktree commands are under "Parallel work with git worktrees" above.)

**Measure with the built binary, not `cargo run`** — cargo re-resolves the
workspace and freshness-checks every file before it execs anything, ~500ms on
this workspace, comparable to zesterm's whole startup. And use `--profile fast`
for the edit-run loop: `--release` (thin LTO, one codegen unit) costs ~51s for
a one-line change versus ~3.6s, while `fast` stays within a few percent at
runtime, so startup and frame numbers measured on it are still meaningful.

Each `--example` above answers "which layer is wrong" without the ones above it.
`attach` is the daemon's `headless`: when a session renders wrongly in the app it
says whether the daemon or the renderer is at fault, with no window, GPU or font
involved. `mesh_probe` is the two-machine check no unit test can perform — it
reports **self-visible** separately from **peers**, so "my multicast is not
leaving this box" and "nothing else is advertising" are distinguishable.
`probe:resize` is the same idea one layer above `pty_dump --resize`: that one
answers "what does ConPTY emit", this one "what does the host's block index look
like afterwards", and between them a resize bug can be placed without guessing
from a rendered pane. A block coming back `rows=0`, or with its `nonBlank` count
halved, is the anchors and the content having diverged (#200).

`--screenshot` is the one to reach for when the question is *how it looks*.
It renders one real frame — real insets, real chrome, real theme, real scale
factor — into a texture and writes a PNG, without ever showing the window. That
matters in three ways a screen capture does not: it needs no screen-recording
permission (which the OS grants to the *hosting terminal*, not to zesterm, and
which fails by silently returning black frames rather than erroring), it puts
nothing on anyone's screen, and it works over SSH and in CI. It composes with
`--theme`, `--font`, `--size`, `--opacity` and `-e`, so a layout question can be
asked of every theme in one loop:

```sh
for t in obsidian nord gum classic paper; do
  ./target/fast/zesterm --theme "$t" --screenshot "/tmp/$t.png"
done
```

Prefer asserting pixels over eyeballing where you can — a corner pixel equal to a
blank cell's is what proves #44 stays fixed, in every theme, without a human
looking. `render_dump` remains the layer *below* it: it knows nothing about
`Insets` or the tab strip, so it answers "is the renderer wrong" rather than
"is the window wrong".

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

Each of these cost real time. The full story lives where the pointer says — an
ADR, a module's gotcha comments, or the issue — and the entry here is the part
you need before you trip on it.

### ConPTY and resize

- **ConPTY needs `STARTF_USESTDHANDLES` with null handles**, or the child writes
  to the parent's stdout whenever ours is redirected — which, for a terminal, is
  always; and **`ClosePseudoConsole` deadlocks** unless the reader is still
  draining, which dictates the whole shutdown protocol (the reader also cannot
  observe child exit at all). Every API call involved reports success.
  (`zest-pty/src/windows.rs`, gotchas.)
- **ConPTY answers a resize by restating the entire viewport, so the grid must
  be resized *before* the pty — and the terminal lock must not be held across
  the call.** Told first, the pty repaints for the *new* size into a grid still
  at the *old* one, which nothing afterwards can undo; holding the lock instead
  is the `ClosePseudoConsole` deadlock wearing a different hat. Both hosts have
  a probe transport asserting the order
  (`the_grid_is_resized_before_the_pty_is_told`). → ADR-013, #200.
- **ConPTY's buffer is only as tall as the viewport, so a grow must not pull
  history back down into it — until the repaint has closed.** The whole
  height-axis ownership dance — the DECTCEM-bracketed settle, the coverage
  guard against stale repaints, the debt that bounds the pull, and the fact
  that only a storm's *first* repaint announces its size — is ADR-013, written
  after #200/#247/#271/#312/#315/#335/#341 each paid for a different corner of
  it — and the *width* axis (#224) is the same ownership question answered the
  same way: the two reflows can never disagree about wrapping (ConPTY restates
  logical lines and relies on our autowrap) but a widen restates **from home**
  where our reflow bottom-anchors, so the viewport re-anchors top-aligned on
  the line the restater still holds, banking the surplus and, when its buffer's
  top row was a wrapped *fragment*, banking through the merged line too. The two
  meta-lessons survive here: **a capture beats a helper** (a synthetic helper
  that announced sizes ConPTY does not kept a broken fix green), and the flag
  is set at the *door* that makes a terminal a replica (`Terminal::remote`),
  because a name that describes one of two callers is how the other goes
  unnoticed.
- **A replica has to give history back too, and the seam has *three* readers,
  not two.** When the host settles, its keyframe re-delivers lines the replica
  still holds, so the same line id exists twice and the session lists
  everything twice — `Grid::drop_scrollback_rows` is the inverse, fixed first
  in the web client and missed in the Rust one (#291), and then found to exist
  **three times with three semantics**: the Rust applier swept on an id
  comparison, the web client dropped only the ids the keyframe names, and
  `decode.rs` — the reference conformance checks — had no take-back at all.
  Ids have gaps, so "unnamed" ≠ "nonexistent on host", and the sweep deleted a
  client's only copy of destroyed rows. The shared rule: **a keyframe un-banks
  exactly the lines it names** (#313). What let the rules diverge: the
  conformance corpus contained no resize, so
  `a_recorded_conpty_drag_keeps_all_three_participants_agreeing` now replays
  the recorded drag through all three. And the reason `zest-proto`'s own tests
  never caught #291: the harness fed the encoder a constant cursor, so every
  resize test exercised a client with no history — a fixture must assert it
  *reached* the state under test before asserting anything else.
- **A row overwritten in place kept a stale `wrapped`, because the fact was
  kept twice.** `Row::wrapped` beside `CellFlags::WRAPLINE` on the last cell,
  cleared together only by `Row::reset`, which a scroll runs and an overwrite
  does not — so a rewrite that replaced the last cell cleared the flag and
  left the bool, and nothing looked wrong until the *next* width change, when
  reflow rejoined rows that were never one logical line and every block below
  described the wrong output — which reads as a resize corrupting the block
  index, and was chased as one. #200 patched the erase path by hand; #219
  removed the second copy: the last cell's flag is the only stored form,
  `Row::wrapped()` derives from it, and an overwrite takes the fact with the
  cell it lives on. The lesson that survives: a fact stored twice needs every
  writer to know about both homes, and the writer that doesn't is the bug.
- **ED 2 and ED 3 differ only in scrollback, pwsh's `cls` emits both, and the
  corpus contains no `3J` at all.** zesterm read the 3 as a repeat of the 2, so
  `cls` kept all history. `Grid::clear_history` + `Keyframe.history_clears` fix
  it; the tests are hand-built because no recording can currently drive this.
  (#314)

### Sockets and pipes on Windows

- **Windows serializes I/O per handle on a *synchronous* named pipe**, so a
  reader parked in `ReadFile` holds off a writer on the same handle — writes
  return success and never arrive, and the peer sees a connection established,
  greeted, then silent. `DuplicateHandle` does not help; it names the same file
  object. The fix is `FILE_FLAG_OVERLAPPED` on both ends — and `ConnectNamedPipe`
  must then be overlapped too, or it returns without waiting and the server
  serves a connection nobody made. (`zest-daemon/src/local.rs`.)
- **`shutdown` does not unpark a socket read that is already parked, on
  Winsock** — and neither does arming `SO_RCVTIMEO` at cut time, which applies
  only to calls issued *after* it is set. POSIX wakes the reader; Windows never
  does, with every call returning success. The fix is a read timeout armed
  **before** the reader can park, plus a flag checked each time one elapses
  (`zest_cloud::tls::READ_POLL`, `zest_daemon::lan::READ_POLL`).

  This cost two features (#94, #99 — a `shutdown`-only watchdog leaked threads
  that each held one of 32 shared mid-handshake slots), and then nearly a third
  when #126 measured ten clean cycles against a peer that closed on EOF and
  concluded the poll was redundant. What the *peer* does decides what you
  observe: a peer that answers your cut with a FIN wakes the reader on any
  platform, a peer that stays up and says nothing is the case the poll exists
  for, and the two are indistinguishable in a log. Node, Python and every shell
  one-liner close on EOF, so a stand-in peer answers the wrong question green —
  `connected_pair()` holds its `_client` open for exactly this reason. **The
  poll is the mechanism, not a fallback.** Cross the two cases before believing
  either: `cargo run -p zest-cloud --example shutdown_probe`.

  Two more edges, both measured on this box: a read timeout is per *handle*
  (a `try_clone`d handle keeps its own copy — have the owning thread disarm its
  own, off a flag), and a read issued *after* the shutdown sometimes returns
  `ConnectionAborted` (10053) rather than zero bytes — map any failure on an
  already-severed connection to end-of-stream, and never latch it (#101).

### Unix ptys

- Four sharp edges, all in `zest-pty/src/unix.rs`'s gotcha comments:
  **`TIOCSWINSZ` on the master fails `ENOTTY` until the slave has been opened
  once** (set the initial size on the slave); **closing the master cannot hang
  up a pty whose reader is parked in `read`** — the reader holds a duplicate fd,
  so `PtyTransport::hangup` signals the process group instead; **EOF arrives as
  `EIO`, not a zero-length read** — treat it as EOF or every clean exit logs an
  I/O error; and **macOS destroys a dead pty's queued output ~0.6s after the
  last slave closes** (instantly if you reap the child first), silently — so
  `UnixPty::spawn` parks its drain thread in `read` *before* forking, which
  removes the deadline rather than shortening it (#54).
- **`split_command_line` eats a backslash even inside double quotes, where sh
  would keep it** — so a `CommandSpec` of `sh -c "…printf '\033[31m…\n'…"`
  hands the shell a script whose escapes read `033` and `n`: no colour, no
  newline, exit code 0, and nothing anywhere to notice. Two daemon test
  fixtures were vacuous on unix from the day they were written because of
  this (#285). Spell no backslashes in a command line — embed the literal
  byte, ESC included; #408 tracks whether the splitter should learn sh's
  double-quote rule.
- **macOS's `/bin/sh` does not pass `SIGINT` on when non-interactive**, so a
  `sh -c 'sleep 30'` test child survives a `^C` a working pty delivered
  correctly. Spawn the binary directly in tests.
- **macOS delivers filesystem events under the resolved path** — `/var` and
  `/tmp` are symlinks into `/private` — so comparing a watched path literally
  against `notify`'s event paths never matches and the config silently stops
  reloading. (`zest-config/src/watch.rs`.)

### Wire and protocol

- **A wire field nothing fills reads exactly like a field nothing *can* fill.**
  `HostMessage::Exited { code: Option<i32> }` shipped with its sole producer
  hard-coding `None`, and `None` is a *legal* value — so the one trustworthy
  exit code in the system did not exist, with no symptom anywhere. **An
  `Option` on the wire hides a missing producer**; test a field's *value* from
  the client side, over the wire, not the event that carries it. → ADR-015's
  "the trap this ADR was written after"; #299.
- **A buffered frame reader is state, and a handoff that drops it drops
  messages.** `DaemonClient::recv` buffers up to 64 KiB and the daemon batches
  its replies, so a handoff to a fresh `FrameReader` deletes coalesced frames —
  and since the seal's nonce is an implicit counter, every later frame then
  fails to open and the window is blank forever with one `warn!` as the trace.
  `into_halves` returns the buffer; `DaemonClient::pending()` lets a handoff
  assert it carried it. (#54)
- **The seal switch is positional, and its two halves flip at different
  moments** — incoming when the `Challenge` is *produced*, outgoing when it is
  *written*. One flag for both is a bug that only appears under pipelining; in
  the browser, open a frame where it is *processed*, not where it arrives.
  → ADR-008.
- **A sealed frame's length prefix describes the ciphertext**, 16 bytes longer
  than the plaintext. Bound the plaintext against `MAX_FRAME` instead and only
  a maximal keyframe fails — that is, only very large grids.
- **HKDF-Expand and HKDF are different functions, and the ratchet needs
  Expand** — full HKDF with an empty salt is one identifier away
  (`@noble/hashes/hkdf` exports both) and no ordinary test reaches a branch 16
  million records in. `fixtures/handshake.json` straddles the 2²⁴ boundary for
  exactly this reason and caught it on first use. → ADR-008.
- **The `v2` in `zesterm-auth-v2` counts transcript layouts, not protocol
  versions** (the protocol is at 3). Deriving one from the other produces
  signatures that will not verify with nothing naming the cause; a test pins
  the literal and asserts the two numbers differ.
- **A cache refreshed only by your own writes reads as live right up until
  somebody else acts.** `zest-mcp`'s `sessions` served `Shared::sessions`, which
  is written only when a `Sessions` message arrives — and the connection sets
  `Watch { sessions: false }`, so the only thing that ever arrived was the reply
  to our own `CreateSession`/`CloseSession`. Every field a session gains *after*
  it spawns (title, OSC 7 cwd, `alt_screen`) therefore reported the value it held
  a millisecond after the spawn: empty, empty, false. The daemon was innocent
  throughout — `Registry::list` reads all three live. What makes this expensive
  to diagnose is that **both obvious ways to check it are the two things that
  hide it**: attaching a viewer to look is a state change that lands a push, and
  creating a second session to compare against refreshes the whole list — so the
  bug is invisible to a human at the tab and to an agent that pokes it twice. The
  fix is that a listing is a *question* (`Conn::list_sessions`), which is what the
  `sessions: false` comment already argued for and what the code had stopped
  doing. (#360)
- **A `Detach` is answered by nothing, and a reader that recovers on its own
  can race it.** `zest-mcp`'s reader answers a refused delta with
  `RequestKeyframe` by itself, and every tool call detaches when it ends — so
  `Detach, RequestKeyframe` was a wire order any busy session could produce,
  the daemon refused the orphan by name, and the *next* attach wore that
  refusal as its own (#347's guard cannot tell them apart, by design). `run`'s
  `exit` step against bash failed 12/12 on Linux with an error about a message
  nobody had sent. The fix that looks obvious — drop the replica before sending
  `Detach` — is the trap: a recovery that *preceded* the detach is answered
  with a keyframe, which then mints a replica for a session nothing is
  subscribed to, and the next tool call waits on it to its deadline, a hang
  where the bug was a refusal. Intent is client-side state
  (`Shared::wanted`): a keyframe, a recovery or a per-session error counts only
  while the session is in the set, and `detach` leaves the set under the same
  lock the reader recovers under, so the orphan is never sent. (#409)
- **`rmp-serde` writes the narrowest integer that fits**, so a `u64` reaches
  JavaScript as a plain `number` for every realistic value — the bindings say
  `number` via `ts(type = "number")` on each such field (#14), and a new wire
  integer must carry the same attribute or its binding lies. The one value
  outside ±2^53 a host actually sends, the `i64::MIN` a blank row is padded
  with, is a power of two and converts exactly (`lineNum` in
  `clients/web/packages/proto/src/wire.ts`).
- **A JavaScript client must iterate code points, never `text.length`** —
  UTF-16 code units count an astral-plane emoji as two. **CJK does not catch
  this** (it is BMP), so the corpus refuses to generate without something past
  U+FFFF in it.

### Signals

- **`EINTR` is not the end of a stream, and every read loop in `zest-daemon`
  treated it as one.** A blocking `read` returns it when a signal lands while it
  is parked, and a process that reaps children gets `SIGCHLD` — so a daemon
  serving sessions, or a client that has started one, can have an unrelated read
  interrupted by a shell exiting elsewhere. `Err(_) => break` then closes a
  healthy peer, ends a live shell, or reports
  `Transport("Interrupted system call (os error 4)")` from a *handshake*, which
  reads as the daemon refusing a key it had accepted. Rare enough to look like a
  flake and not one: it first appeared on `test (ubuntu-latest)` when a change
  added enough child processes to make `SIGCHLD` common. One
  `read_retrying`, because three call sites that each have to remember is how
  two of them forget. (`zest-daemon/src/lib.rs`; #274)

### Blocks and shell integration

- **A prompt redraw re-emits `OSC 133;A`, and a block with no end claims every
  line below it.** zsh emits `C` from preexec and `D` only when something ran,
  so an empty Enter, a `^C` or a resize is an `A` on its own — and one drag is
  dozens of prompts, each leaving an endless `Prompt` block that owns the rest
  of the session. It reads as "the terminal is dead, I can't type" while typing
  works perfectly. pwsh brackets even an empty line with `C`/`D`, so the whole
  class is invisible unless you test on macOS. `begin_prompt` reuses a trailing
  block that ran nothing; `sliceBlocks` bounds an open block at the next
  block's `prompt_line`. (#193)
- **A submitted command mints no block id, so correlating on a new one never
  fires.** OSC 133 `C` makes `begin_output` mutate `blocks.last_mut()` in place,
  so the command lands in the *existing* trailing prompt block at an id **at or
  below** the high-water mark — and `begin_prompt` re-anchors an abandoned
  prompt rather than pushing, so the id can stay put for a whole session. Wait
  for `id > high_water` and every `run` reports a timeout on a command that
  finished instantly. Anchor on the tail block's identity, wait on its *state*,
  and compare `>=`: zsh reuses the id, pwsh mints a fresh one, so a rule written
  on either shell alone is silently wrong on the other. One copy of the rule —
  `tools::block_anchor` and `tools::finished_since`, which both `blocks(wait:)`
  and `run` call — because two would drift and only one of them is replayed
  against a capture. (`zest-mcp/src/run.rs`, ADR-015; #274, #331)
- **What says the anchor is gone is the block's presence, never
  `authoritative_from`.** That field *lowers* on a screen clear where eviction
  raises it — but with `min(lowest_gone)`, and a young session's floor is
  already 0, so a clear that erases the anchor moves it by nothing while the
  next prompt pushes an id *above* it. Read that way, `run clear` reported a
  command that never started and burned the whole deadline. (#274)
- **Between `D` and the next `A` a shell has no prompt, and an exit can arrive
  before the output before it.** Two `run`s back to back land in the first gap
  almost every time, so it must be a different refusal from "a command is
  running" — only one is worth waiting out. And the daemon's reader sets
  `has_exited` on EOF while bytes may still be queued for the parser, so `exit`
  in a real zsh arrives as `Exited` first and the `C` that opened its block a
  beat later, about one run in eight. Nothing on the wire says "that was the
  last delta", so stopping on the exit needs a bounded drain after it. Both were
  found by driving the built binary by hand. (#274)

### Rendering and fonts

- **swash hard-codes an LCD hinting target you cannot select** — the symptom is
  *shapes changing* (`w` reading as `W`), not softness, and it looks exactly
  like a bad font, a bad size, a shaping bug or a broken atlas. What ships is
  **grayscale coverage, grid-fitted** — the opposite of what the first
  investigation concluded, having measured at the wrong size against a broken
  baseline. **Fix the known defect first, then evaluate against it.**
  → ADR-010 (the constraint), ADR-011 (the numbers); #100, #84.
- **Aggregate pixel metrics hide text rendering bugs** — three times in one
  issue. Look at a few pixels (a stem's intensity profile, one element's peak
  brightness), not averages of millions. → ADR-011.
- **DX12 cannot do per-pixel alpha** through wgpu's ordinary surface path;
  transparency on Windows is adapter-dependent. Premultiply everywhere
  regardless. → ADR-003.
- **Window opacity applies only to cells whose background is `Color::Default`**,
  and never to glyphs — anything else makes TUI panels see-through or text
  unreadable. → ADR-003.
- **Emoji are script `Zyyy` and Nerd Font icons are Private Use Area**, so
  script-based fallback structurally cannot find either. Emoji need an explicit
  `GenericFamily::Emoji` path; PUA needs an installed Nerd Font, discovered by
  name. Get this wrong and the user's prompt is blank.
- **A failing pty test that prints raw VT clears your terminal** and scrambles
  its own failure message. Escape test output.

### mDNS

- **A DNS-SD *instance name* is not a *host name*.** The instance may contain
  spaces and parentheses; the SRV target must be a DNS label. Derive one from
  the other and the service publishes fine while no A record ever resolves —
  the host appears in the fleet listing with an empty address set, which is
  indistinguishable from a laptop that is asleep. The host name is built from
  the `HostId` instead. (`zest-mesh/src/discovery/mdns.rs`.)

### Daemon lifecycle, keys and pairing

- **The daemon's environment is frozen at first spawn, and every shell in the
  fleet inherits it** — a daemon started from inside an agent session or an IDE
  hands those markers to every window opened afterwards, for hours, from
  anywhere. The known markers are cleared in `terminal_env()`
  (`zest-pty/src/lib.rs`), but anything context-specific in a shell's
  environment is worth suspecting there first.
- **macOS keychain grants do not survive `cargo build`.** The Keychain binds
  "Always Allow" to the *designated requirement* of the binary that asked, and
  ad-hoc-signed dev builds change identity on every rebuild — so the daemon
  blocks on a fresh Keychain prompt, the app gives up after 2s and silently
  falls back to an in-process pty, and `origin=InProcess` in the startup line
  is the only sign that nothing daemon-backed is being tested. Either sign dev
  builds with a stable identity — `ZESTERM_SIGN_IDENTITY` makes `zesterm-dev`
  do it, and the `--identifier` flag it passes is load-bearing (without it the
  designated requirement names the binary and the grant dies again) — or run
  the daemon yourself with `--ephemeral` for the edit-run loop. Windows has no
  equivalent problem (Credential Manager keys by target name, not binary), and
  the flip side: no per-binary ACL at all, so the exposure there is the
  session, not the build.
- **`--trust <hex>` is a one-shot command, and `--ephemeral` discards the
  result.** It records the pairing and *exits*, so it cannot be combined with
  serving; under `--ephemeral` the trust store is in memory and a
  `--trust-file` is accepted then ignored (a `WARN` says so, easy to read
  past). A foreground daemon can only approve pairings by answering its own
  prompt, and it refuses to prompt unless stdin is a **terminal** — a pipe gets
  `no stdin to prompt on`, and the browser sees a handshake that never
  completes.
- **umask is process-global, and the victims of leaking one are a crate away
  from the culprit.** A scoped save/restore umask guard around the socket bind
  was safe by a comment ("before any other thread exists") that a test binary
  falsified: zest-app's tests run ~22 in-process daemons on libtest's pool, two
  overlapping guards race their save/restore, and the process is left at
  `0o177` forever. Every directory any thread creates after that is born
  without owner-x, so the first *write* into it fails EACCES — which surfaced
  as all of `themes::tests` failing at once on unix CI and read as the
  runner's temp root breaking. Never reach for umask where threads exist;
  `mkdir(2)`'s explicit mode is race-free (a umask can only narrow it), which
  is how `bind_private` (`zest-daemon/src/local.rs`) keeps the socket 0600
  from birth: bind in a 0700 staging dir, chmod, rename into place. (#403)

### App UI

- **A UI text entry must never invent its own key handling.** An entry that
  `return`s before the keymap table swallows every chord that reaches it, and
  the guard that makes a chord "not text" is the same guard that eats ⌘V —
  seven entries had this in two different per-platform ways. The rule:
  `zest-app/src/text_field.rs` owns caret, selection and every clipboard chord
  (`command_for`, consulted **before** each entry's own keys); call sites own
  only Enter, Escape and their list navigation. And a field's clipboard chord
  is not the grid's: `field_clipboard_chord` takes Super *or* plain Ctrl,
  because a field has no shell to protect — reusing the terminal's
  `is_clipboard_chord` shipped a field that could not be pasted into at all on
  the primary platform while working on the Mac it was written on. (#228,
  #251, #270)
- **A text edit whose only commit is Enter loses work through every other
  exit**, and each exit looks like a different bug ("some settings save" is not
  a shape a broken writer can produce). **Leaving a field is a commit** — only
  Esc discards — and the decision lives in one function every exit routes
  through (`profiles_ui::take_pending_edit`), not in the Enter arm. An edit
  that does not parse must *block* the exit; a buffer whose profile vanished
  must be *dropped*, or it writes into whatever the editor fell back to. (#272;
  the Settings tab has the same shape and is not fixed yet.)
- **iOS opens the soft keyboard only for a focus *change* that runs inside
  the gesture's own task — and `focus()` on the element that already holds
  focus is not a change.** The web client focused its hidden textarea off a
  `setTimeout` from `mousedown` — which took focus, showed no keyboard, and on
  an iPad left a terminal that could be read and not typed into, with no
  error anywhere. A synchronous `focus()` in the `click` handler was the
  first fix (#421) and opened nothing either: the textarea is focused at
  mount, so the tap changed no focus. `blur()` then `focus()` in the same
  task is the re-open (`soft-keyboard.ts`, #428) — for a *touch* only, a
  mouse click on a focused terminal must not flash focus-out at vim. And
  `document.activeElement` does not say whether the keyboard is up: iOS's
  own dismiss key hides it without blurring. The visual viewport does
  (`visual-viewport.ts` writes `kbd-up`), and a ⌨ toggle reads that.
  Two neighbours: an input under 16px makes iOS zoom the page on focus, and
  iPadOS Safari reports `navigator.platform === 'MacIntel'` — right for chord
  conventions with a hardware keyboard, wrong as a touch test; detect touch
  with `maxTouchPoints` / `(pointer: coarse)`. And the layout viewport does
  not shrink for the keyboard there: `visual-viewport.ts` sizes the shell to
  the visual one. (#421)
- **A sigx `ref` is called with `null` on unmount, and its JSX typing says
  otherwise.** `ref?: (el: T) => void` is what `@sigx/runtime-dom` declares;
  what the runtime does is call every ref with the element on mount and with
  `null` when the node leaves — so a ref that *dereferences* is a production
  TypeError that `tsc` cannot see, and `Shell` keys `TerminalView` per tab, so
  "when the node leaves" is every tab switch. The textarea's
  `el.setAttribute('autocorrect', 'off')` (#422) was that error on the hosted
  client; the tab strip's `chipEls.set(id, el)` stored the null for a later
  `.isConnected` to trip on. A ref assigns and returns; anything more lives in
  a helper that takes `T | null` and is tested with the null
  (`bindTerminalInput`, `trackChip`). Every ref in the client is annotated
  `(el: T | null)` so the next dereference is a type error. (#440)

### The dev harness on this box

- **The agent shell sets `NO_COLOR=1`**, and a pty child inherits it —
  PowerShell then strips every escape *before* the pty, so a colour test looks
  like a broken renderer. `Remove-Item Env:\NO_COLOR` before any visual check,
  and confirm a suspected colour bug offscreen with `render_dump --replay`
  first.
- **`Start-Process -ArgumentList` does not re-quote array elements** —
  `'--font','My Font'` arrives as two arguments. Quote inside the string
  (`'"My Font"'`). This is the harness, not the argument parser.
- **Git Bash rewrites unix-looking arguments** (MSYS path conversion):
  `\\.\pipe\x` loses a backslash, and `--cmd /bin/cat` sent to a *remote*
  daemon becomes a Windows path on the wire — a failure that reads as the far
  host being broken. Quoting does not help. Use PowerShell for anything
  carrying pipe paths or paths destined for another machine, or set
  `MSYS_NO_PATHCONV=1`. (#20)
- **Git Bash on the Windows box has no `jq`** — harmless in a one-shot command,
  quietly fatal in a polling loop, where the empty result reads as "not
  finished yet". Use `gh … --jq` / `-q`, which is gh's own.
- **Release builds are GUI-subsystem**, so a shell will not wait for them and
  `zesterm --themes` returns the prompt before printing. Use
  `Start-Process -Wait` when scripting against it; debug builds keep the
  console subsystem.

### Local TLS

- **rustls refuses a `CA:TRUE` certificate as an end-entity**, and the error
  (`ExtensionValueInvalid`) reads as a malformed extension while meaning "this
  is a CA and you served it as a server" — `curl` and every browser accept the
  same certificate, so the natural next suspect is the Rust client, wrongly. A
  chain wants the CA in the trust store and a leaf (`CA:FALSE`, `serverAuth`,
  with the SAN) signed by it. And **Windows will not let a non-interactive
  shell add or remove a root certificate** — the protected-root dialog cannot
  be suppressed in either direction, so a locally-trusted CA needs a human at
  the keyboard twice and cannot be scripted into CI. (#126)

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

# zesterm — orientation

A GPU-accelerated, themable terminal, and a **fleet**: every machine runs a
daemon and can be reached from every device. The Mac's shell in a window on
Windows; a Linux build watched from a phone.

## Read these first

1. **[docs/ROADMAP.md](docs/ROADMAP.md)** — the plan, current state, and the
   workstream map. Source of truth; issue #1 mirrors it.
2. **[docs/CONTRACTS.md](docs/CONTRACTS.md)** — the frozen seams between
   workstreams. **Read this before touching a shared type.**
3. **[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)** — decisions that were
   expensive to reach and are cheap to accidentally undo. Argue with the
   reasoning there before changing any of them.

## If you are working on one workstream

Several run in parallel. The rules exist so that stays cheaper than working
serially — which it stops being the moment two streams edit the same file.

- **Find your stream in the ROADMAP table** and work only inside the paths it
  owns. If the job seems to need a file another stream owns, that is a signal
  the seam is wrong: say so rather than reaching across.
- **Never edit the root `Cargo.toml` or `Cargo.lock`.** Every crate the project
  will have is registered already, including the skeletons. Adding a
  *dependency* to your own crate's manifest is fine.
- **Never change a frozen contract.** Open an issue and wait — see
  [docs/CONTRACTS.md](docs/CONTRACTS.md). Adding a new type beside one is fine.
- **One git worktree and branch per stream**, or agents fight over `target/`
  and each other's edits:
  ```
  git worktree add ../zesterm-ws-c ws/c-unix-pty
  ```
  Merge to `main` at stream boundaries, not continuously.
- **Update the roadmap in the same commit as the work**, then refresh the issue.
  A roadmap that lags is one nobody trusts.

Four gates, all of which must pass before you call something done:

```
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo xtask check-deps
cargo xtask check-schema
```

## The one invariant

`zest-core` must never depend on `wgpu`, `winit`, `windows`, or `tokio`, and must
build for `wasm32-unknown-unknown`. This is what lets the native app, the future
daemon, and the browser/mobile clients share one terminal implementation instead
of three that quietly diverge.

```
cargo xtask check-deps
```

CI runs it on every push and pull request, along with the full suite on Windows,
macOS and Linux and the wasm32 build — see `.github/workflows/ci.yml`. If a
dependency genuinely belongs, move the *code* up a layer rather
than relaxing the rule.

## Commands

```
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --all-targets
cargo xtask check-deps
cargo build -p zest-core --no-default-features --target wasm32-unknown-unknown

cargo xtask schema                             # regenerate the settings JSON Schema

cargo run --profile fast -p zest-app           # the terminal, quick rebuild
./target/fast/zesterm --startup-probe          # time to first paint; fails over 100ms
cargo build --release && ./target/release/zesterm   # the shipping build
cargo run -p zest-app  --example headless      # a terminal with no window
cargo run -p zest-font --example font_dump     # font sample sheet as a PNG
cargo run -p zest-pty  --example pty_dump      # raw VT stream / corpus recorder
cargo run -p zest-render-wgpu --example alpha_probe   # transparency capability
```

## Conventions

- **Comments explain *why*, never *what*.** The non-obvious constraint, the
  rejected alternative, the bug this shape prevents. If a line is self-evident,
  it gets no comment.
- **Tests assert behaviour with a reason.** `assert!(x, "why this matters")`.
  Several existing tests exist purely to catch silent regressions (cell size,
  allocation-free scrolling, 0%-idle damage) — those are load-bearing, not
  ceremony.
- Prefer finding bugs at the cheapest layer. The font PNG dump and the headless
  terminal both exist because diagnosing through a renderer means first guessing
  which layer is wrong.

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

## Related work on this machine

`C:\Dev\sigx` is the user's own framework (github.com/signalxjs), consumed by the
web and mobile clients later. Layout is git-worktree-per-branch, so the real
checkout is `<repo>\main\`. Note `@sigx/terminal` renders TSX *to* a TTY — it is
not a terminal emulator and cannot be the web client's grid renderer.

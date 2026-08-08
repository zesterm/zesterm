# zesterm — orientation

A GPU-accelerated, themable terminal, and a **fleet**: every machine runs a
daemon and can be reached from every device. The Mac's shell in a window on
Windows; a Linux build watched from a phone.

## Read these first

1. **[docs/ROADMAP.md](docs/ROADMAP.md)** — the plan, current state, and what is
   being built next. Source of truth; issue #1 mirrors it.
2. **[docs/CONTRACTS.md](docs/CONTRACTS.md)** — the seams that hold the clients,
   the daemon and the core together. **Read this before touching a shared type.**
3. **[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)** — decisions that were
   expensive to reach and are cheap to accidentally undo. Argue with the
   reasoning there before changing any of them.

## How the work runs

**One lane, on `main`.** The project was built by several workstreams in
parallel, each in its own worktree with its own paths; that is over. The streams
survive as *names for bodies of work* in the roadmap — useful for saying what a
commit is about — not as ownership boundaries or branches.

- **Sequential commits on `main`.** No worktree-per-stream, no path ownership. A
  branch is for something genuinely speculative, not for routine work.
- **Never edit the root `Cargo.toml` or `Cargo.lock` by hand.** Every crate the
  project will have is registered already, including the skeletons. Adding a
  *dependency* to your own crate's manifest is fine.
- **A contract in [docs/CONTRACTS.md](docs/CONTRACTS.md) still does not move
  casually.** With one lead the rule is no longer "open an issue and wait" — it
  is: change it deliberately, land it with **every** consumer in one commit,
  update the table, and say so on issue #1. A frozen contract with a
  half-updated consumer is worse than either shape. Adding a new type beside one
  is still free.
- **Update the roadmap in the same commit as the work**, then refresh the issue.
  A roadmap that lags is one nobody trusts.

Six gates, all of which must pass before you call something done:

```
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo xtask check-deps
cargo xtask check-schema
cargo xtask check-bindings
cargo xtask check-fixtures
```

And, **if you touched `clients/web/` or any type on the wire**, the TypeScript
suite too. It is not in the list above on purpose: a Rust-only change that passes
`check-fixtures` cannot break it, and a gate people learn to skip is worse than
no gate.

```
pnpm -C clients/web install
pnpm -C clients/web -r typecheck
pnpm -C clients/web -r test
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
cargo test -p zest-proto --features ts         # regenerate the TypeScript bindings
cargo xtask fixtures                           # regenerate the conformance fixtures
cargo run -p zest-proto --example fixture_dump -- --only vim-macos --print 7
                                               # one fixture frame, decoded, to stdout

zesterm-dev                                    # build both binaries and open a window
zesterm-dev --no-build --attach-probe          # probe flags stay in the foreground
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
zest-daemon --identity                         # this host's public key
zest-daemon --trusted                          # which devices are paired
zest-daemon --ephemeral                        # throwaway key, for the edit-run loop
cargo run -p zest-daemon --example attach      # drive a daemon session, no GUI
cargo run -p zest-mesh   --example mesh_probe  # advertise and browse the fleet
cargo run -p zest-daemon --example pair -- --addr <host:port>   # pair with a host
```

Each `--example` above answers "which layer is wrong" without the ones above it.
`attach` is the daemon's `headless`: when a session renders wrongly in the app it
says whether the daemon or the renderer is at fault, with no window, GPU or font
involved. `mesh_probe` is the two-machine check no unit test can perform — it
reports **self-visible** separately from **peers**, so "my multicast is not
leaving this box" and "nothing else is advertising" are distinguishable.

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
  the far host tries to spawn a Windows path on macOS. Quoting does not help —
  the conversion runs after the shell. Use PowerShell for anything carrying
  pipe paths or paths destined for another machine, or set
  `MSYS_NO_PATHCONV=1`. Both halves of this bit on the same day, over the same
  feature. (#20.)

## Related work on this machine

`~/dev/sigx` (`C:\Dev\sigx` on the Windows box) is the user's own framework
(github.com/signalxjs), consumed by the web and mobile clients later. Layout is
git-worktree-per-branch, so the real checkout is `<repo>/main/`. Note
`@sigx/terminal` renders TSX *to* a TTY — it is not a terminal emulator and
cannot be the web client's grid renderer. Its `terminal-zero` token contract
*is* reused: `zest-theme`'s `UiTokens` is that record field-for-field, so
`{...theme.ui, name, mode}` is a valid argument to sigx's `registerTheme()`.

`clients/web/` is a pnpm workspace, Node 24, `node --test`, and no runtime
dependencies at all — the decoder will run in a worker and the framing,
MessagePack and delta application are hand-written. sigx arrives with the app
shell, not before. The packages are published to npm; the local checkouts lag by
a minor, so install from npm rather than linking.
